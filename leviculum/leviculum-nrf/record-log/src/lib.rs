//! A forward-only record log on 4 KB NOR sectors.
//!
//! # What a record is at this layer
//!
//! **A record is a byte string with a 32-byte key, a 4-byte timestamp and a
//! 1-byte tag. Nothing here knows about LXMF, messages, telemetry or
//! diagnostics.** The key is opaque: an LXMF propagation store puts a
//! transient ID in it, the field diagnostics ring (Codeberg #380) puts
//! whatever identifier its entries carry, telemetry retention puts a target
//! hash. The tag is one opaque byte — the propagation store's stamp value in
//! the concept paper's table, a severity or a kind for anyone else. This
//! layer reads neither.
//!
//! That is a deliberate line. The store is the part of a propagation node
//! that is worth having whether or not the node ever exists
//! (`docs/src/concepts/propagation-node-on-a-board.md`, §6), so it must not
//! be reachable only through an LXMF-shaped door.
//!
//! # Why forward-only, and why round-robin reclaim
//!
//! Both parts we drive are rated 100 000 erase cycles per sector. At the
//! duty the 2026-09-09 field walk measured — 22.4 messages/hour — the data
//! itself costs a sector erase every eleven records and outlives the board
//! by three orders of magnitude. **A single fixed metadata sector, rewritten
//! on every accepted record, spends its whole budget in 6.1 months at that
//! same duty** (the paper's endurance table). So there is no superblock, no
//! index sector, no head pointer and no sequence counter at a fixed address.
//! Everything this log needs to mount itself is recovered by reading the
//! sector headers, and a sector header is written exactly once per erase of
//! the sector it heads — which is the definition of level wear.
//!
//! Reclaim is round-robin over the region: when the active sector cannot fit
//! the next record, the *next* sector by index is erased and becomes active,
//! dropping the oldest records with it. Every sector is therefore erased
//! once per lap, and `SimNor::erase_counts` in the tests pins that with its
//! own negative control.
//!
//! # On-flash layout
//!
//! Every offset in the region is a multiple of `PROGRAM_UNIT` (4). That is
//! not a taste: the nRF52840 QSPI peripheral asserts that a program or read
//! address, its length, and the RAM buffer behind it are all 4-byte aligned
//! (`start_write`/`start_read`, `embassy-nrf-0.9.0/src/qspi.rs`), so a store
//! that hands it a 42-byte header at an odd offset panics inside the driver.
//!
//! ## Sector header, 12 bytes, written once per erase
//!
//! | Offset | Bytes | Field |
//! |---|---|---|
//! | 0 | 4 | magic `LVR1` |
//! | 4 | 4 | sequence, u32 LE — strictly increasing across erases |
//! | 8 | 1 | format version |
//! | 9 | 1 | reserved, 0 |
//! | 10 | 2 | CRC-16 over bytes 0..10 |
//!
//! The sequence is what makes a fixed metadata sector unnecessary: the
//! active sector is the one with the highest sequence, and the oldest is the
//! one after it round-robin. Mounting is a read of `sectors` × 12 bytes.
//!
//! ## Record, 42-byte header then the body then 0xFF padding to a multiple of 4
//!
//! | Offset | Bytes | Field |
//! |---|---|---|
//! | 0 | 2 | body length, u16 LE |
//! | 2 | 32 | key |
//! | 34 | 4 | timestamp, u32 LE |
//! | 38 | 1 | tag |
//! | 39 | 1 | flags: `0xFF` uncommitted, `0xFE` live, `0xFC` purged |
//! | 40 | 2 | CRC-16 over header bytes 0..39 and the body |
//! | 42 | len | body |
//!
//! This is the 42-byte header the concept paper tabulates, with its
//! propagation-specific names generalised (transient ID → key, stamp value →
//! tag). The destination hash is not a field: it is the first bytes of the
//! body, exactly as the reference reads it back from the head of its file.
//!
//! ## How a torn write is recognised
//!
//! A record is written in two steps and committed by the second:
//!
//! 1. Header (with `flags` left at `0xFF`), body and padding are programmed
//!    forward from the record offset.
//! 2. The 4-byte word at record offset 36 — `timestamp[2..4]`, `tag`,
//!    `flags` — is programmed again, identical except that `flags` goes
//!    `0xFF` → `0xFE`. NOR programming only clears bits, so the three
//!    unchanged bytes come out of it holding what they already held and the
//!    flags byte is the only bit that moves. 4 bytes because that is the
//!    smallest unit the QSPI peripheral will write.
//!
//!    No bit changing is not the same as nothing happening: the part still
//!    takes a program pulse, on cells that are already at zero. How many
//!    such partial programs a page accepts between erases is a datasheet
//!    property, and no datasheet for either MX25R1635F or IS25LP080D is on
//!    these machines — so the exposure is measured rather than argued.
//!    The host-side `SimNor` counts program operations per 256-byte page
//!    since that page's last erase, and the test
//!    `program_operations_per_page_are_counted_and_bounded` asserts the
//!    worst case this format can produce: 19, on bodyless records at the
//!    minimum stride. When a datasheet turns up, the check is a comparison
//!    against that number instead of a re-derivation.
//!
//! A record therefore counts as present **iff** its flags byte reads `0xFE`
//! or `0xFC` *and* its CRC checks. Any cut before step 2 finishes leaves
//! `0xFF` there and the record is not seen — deterministically, not with
//! 1-in-65536 confidence. The CRC is then doing what a CRC should: catching
//! a bit the part dropped, not standing in for a commit protocol.
//!
//! Recovery adds one rule. After the last valid record in the active sector,
//! if the remainder of that sector is not still erased, the sector is
//! *sealed* — the cursor jumps to the next sector — because programming over
//! a half-written record would need to raise bits, which NOR cannot do. The
//! cost is at most one partly-used sector per power cut; the alternative is
//! silent corruption.

#![no_std]

#[cfg(any(test, feature = "sim"))]
extern crate alloc;

#[cfg(any(test, feature = "sim"))]
pub mod sim;

#[cfg(test)]
mod tests;

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

/// Erase granularity of both parts, and the only one this log supports.
pub const SECTOR_SIZE: u32 = 4096;
/// Smallest unit that may be programmed or read, in bytes.
///
/// Set by the nRF52840 QSPI peripheral, not by the NOR parts (which program
/// single bytes happily).
pub const PROGRAM_UNIT: u32 = 4;
/// Length of the per-record header.
pub const HEADER_LEN: usize = 42;
/// Length of the per-sector header, written once per erase of that sector.
pub const SECTOR_HEADER_LEN: u32 = 12;
/// Length of a record key.
pub const KEY_LEN: usize = 32;

/// Bytes of a sector available to records.
pub const SECTOR_PAYLOAD: u32 = SECTOR_SIZE - SECTOR_HEADER_LEN;
/// Largest body this layer will store. A record never straddles a sector.
pub const MAX_BODY: usize = SECTOR_PAYLOAD as usize - HEADER_LEN;
/// Stride of a record with an empty body — the smallest room a record needs.
pub const MIN_STRIDE: u32 = align_up(HEADER_LEN) as u32;

/// `LVR1`, little-endian.
const MAGIC: u32 = 0x3152_564C;
const VERSION: u8 = 1;

/// Uncommitted: the erased state. Not a record.
const FLAG_ERASED: u8 = 0xFF;
/// Committed and current.
pub const FLAG_LIVE: u8 = 0xFE;
/// Committed and withdrawn. Still occupies its bytes until its sector is
/// reclaimed; that is the whole point of a forward-only log.
pub const FLAG_PURGED: u8 = 0xFC;

/// Offset within a record header of the 4-byte commit word.
const COMMIT_OFF: u32 = 36;
/// Index of the flags byte inside the commit word.
const COMMIT_FLAGS_IX: usize = 3;

const CRC_INIT: u16 = 0xFFFF;
const CRC_POLY: u16 = 0x1021;

/// Round up to the program unit.
pub const fn align_up(n: usize) -> usize {
    (n + (PROGRAM_UNIT as usize - 1)) & !(PROGRAM_UNIT as usize - 1)
}

/// Bytes a record with `body_len` bytes of body occupies on flash.
pub const fn record_stride(body_len: usize) -> usize {
    align_up(HEADER_LEN + body_len)
}

/// CRC-16/CCITT-FALSE, incremental.
///
/// Same polynomial, seed and bit order as `leviculum_core::framing::crc16`;
/// the standard check value (`"123456789"` → `0x29B1`) is asserted in this
/// crate's tests, which is what keeps the two from drifting. Duplicated
/// rather than shared because these sixteen lines are the only thing this
/// crate would take from `leviculum-core`, and the firmware's pure crates
/// carry no dependencies for a reason.
pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for byte in data {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ CRC_POLY;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// What went wrong. `E` is the underlying device's error type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// The device said no.
    Flash(E),
    /// Body longer than [`MAX_BODY`]; a record never straddles a sector.
    BodyTooLarge,
    /// Region base or length is not a whole number of sectors, or the
    /// region is shorter than the two sectors reclaim needs.
    BadRegion,
    /// The region does not fit inside the device.
    OutOfBounds,
    /// The device's erase, program or read granularity is not one this log
    /// can drive.
    UnsupportedGeometry,
}

/// One record's header, plus where it sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// The caller's key. Opaque here.
    pub key: [u8; KEY_LEN],
    /// The caller's timestamp. Opaque here.
    pub time: u32,
    /// The caller's tag byte. Opaque here.
    pub tag: u8,
    /// [`FLAG_LIVE`] or [`FLAG_PURGED`].
    pub flags: u8,
    /// Body length in bytes.
    pub len: u16,
    /// Absolute device offset of the record header.
    pub offset: u32,
}

impl Record {
    /// Absolute device offset of the body.
    pub fn body_offset(&self) -> u32 {
        self.offset + HEADER_LEN as u32
    }

    /// Bytes this record occupies on flash, padding included.
    pub fn stride(&self) -> u32 {
        record_stride(self.len as usize) as u32
    }

    /// Whether the record is still current.
    pub fn is_live(&self) -> bool {
        self.flags == FLAG_LIVE
    }
}

/// A 4-byte-aligned scratch buffer.
///
/// The QSPI peripheral DMAs straight out of (and into) the buffer we hand
/// it and asserts `ptr % 4 == 0`, so every buffer that reaches the device
/// goes through one of these.
#[repr(align(4))]
struct Aligned<const N: usize>([u8; N]);

/// Window used for streaming reads and programs. Any multiple of 4 works;
/// 64 keeps the stack cost of a scan at one cache-line-ish buffer.
const WINDOW: usize = 64;

/// A forward-only record log over a region of a NOR part.
pub struct RecordLog<F> {
    flash: F,
    base: u32,
    sectors: u32,
    active: u32,
    seq: u32,
    /// Offset of the next record *within* the active sector.
    cursor: u32,
}

impl<F: NorFlash> RecordLog<F> {
    /// Mount the log on `[base, base + len)` without writing anything.
    ///
    /// `Ok(None)` means no sector in the region carries a header this log
    /// wrote — an unformatted region, or somebody else's data. Use this
    /// where formatting would be a decision rather than a detail: the
    /// firmware's boot probe reads the part it has never driven before and
    /// must not erase whatever is on it.
    pub fn mount(mut flash: F, base: u32, len: u32) -> Result<Option<Self>, Error<F::Error>> {
        let sectors = check_region::<F>(base, len, flash.capacity())?;
        let Some((active, seq)) = find_active(&mut flash, base, sectors)? else {
            return Ok(None);
        };
        let cursor = scan_tail(&mut flash, base + active * SECTOR_SIZE)?;
        Ok(Some(Self {
            flash,
            base,
            sectors,
            active,
            seq,
            cursor,
        }))
    }

    /// Mount the log on `[base, base + len)`, formatting it if no sector
    /// there carries a valid header.
    ///
    /// Reads `sectors` × 12 bytes of sector headers plus one scan of the
    /// active sector. Writes nothing unless the region is unformatted, in
    /// which case it costs one erase and one 12-byte header.
    pub fn open(mut flash: F, base: u32, len: u32) -> Result<Self, Error<F::Error>> {
        let sectors = check_region::<F>(base, len, flash.capacity())?;
        let (active, seq) = match find_active(&mut flash, base, sectors)? {
            Some(found) => found,
            None => {
                erase_sector(&mut flash, base)?;
                write_sector_header(&mut flash, base, 0)?;
                (0, 0)
            }
        };
        let cursor = scan_tail(&mut flash, base + active * SECTOR_SIZE)?;
        Ok(Self {
            flash,
            base,
            sectors,
            active,
            seq,
            cursor,
        })
    }

    /// How many records are on the part: `(live, purged)`.
    pub fn count(&mut self) -> Result<(u32, u32), Error<F::Error>> {
        let mut live = 0u32;
        let mut purged = 0u32;
        self.for_each(|record| {
            if record.is_live() {
                live += 1;
            } else {
                purged += 1;
            }
        })?;
        Ok((live, purged))
    }

    /// Append a record and commit it. Returns its absolute device offset.
    ///
    /// Reclaims the next sector round-robin if the active one cannot hold
    /// the record; the records in that sector are gone when it returns.
    pub fn append(
        &mut self,
        key: &[u8; KEY_LEN],
        time: u32,
        tag: u8,
        body: &[u8],
    ) -> Result<u32, Error<F::Error>> {
        if body.len() > MAX_BODY {
            return Err(Error::BodyTooLarge);
        }
        let stride = record_stride(body.len()) as u32;
        if stride > SECTOR_SIZE - self.cursor {
            self.advance()?;
        }
        let offset = self.base + self.active * SECTOR_SIZE + self.cursor;

        let mut header = [FLAG_ERASED; HEADER_LEN];
        header[0..2].copy_from_slice(&(body.len() as u16).to_le_bytes());
        header[2..34].copy_from_slice(key);
        header[34..38].copy_from_slice(&time.to_le_bytes());
        header[38] = tag;
        header[39] = FLAG_ERASED;
        let crc = crc16_update(crc16_update(CRC_INIT, &header[0..39]), body);
        header[40..42].copy_from_slice(&crc.to_le_bytes());

        self.program_record(offset, &header, body)?;

        // The commit. Three of these four bytes are re-programmed to the
        // values they already hold, which on NOR clears no further bit;
        // the fourth is the flags byte going 0xFF -> 0xFE.
        let mut commit = Aligned([0u8; PROGRAM_UNIT as usize]);
        commit.0.copy_from_slice(
            &header[COMMIT_OFF as usize..COMMIT_OFF as usize + PROGRAM_UNIT as usize],
        );
        commit.0[COMMIT_FLAGS_IX] = FLAG_LIVE;
        self.flash
            .write(offset + COMMIT_OFF, &commit.0)
            .map_err(Error::Flash)?;

        self.cursor += stride;
        Ok(offset)
    }

    /// Mark a record withdrawn. Clears one further bit of its flags byte;
    /// the bytes stay put until the sector is reclaimed.
    pub fn purge(&mut self, record: &Record) -> Result<(), Error<F::Error>> {
        if record.flags == FLAG_PURGED {
            return Ok(());
        }
        let mut commit = Aligned([0u8; PROGRAM_UNIT as usize]);
        commit.0[0..2].copy_from_slice(&record.time.to_le_bytes()[2..4]);
        commit.0[2] = record.tag;
        commit.0[COMMIT_FLAGS_IX] = FLAG_PURGED;
        self.flash
            .write(record.offset + COMMIT_OFF, &commit.0)
            .map_err(Error::Flash)?;
        Ok(())
    }

    /// Visit every record still on the part, oldest sector first.
    ///
    /// Purged records are visited too — the caller decides. Reading a body
    /// needs `&mut self`, so the closure gets the header and the caller
    /// reads bodies afterwards from the [`Record`] it kept.
    pub fn for_each(&mut self, mut visit: impl FnMut(&Record)) -> Result<(), Error<F::Error>> {
        for step in 1..=self.sectors {
            let idx = (self.active + step) % self.sectors;
            let sector = self.base + idx * SECTOR_SIZE;
            if read_sector_header(&mut self.flash, sector)?.is_none() {
                continue;
            }
            let mut off = SECTOR_HEADER_LEN;
            while SECTOR_SIZE - off >= MIN_STRIDE {
                match probe_record(&mut self.flash, sector + off, SECTOR_SIZE - off)? {
                    Some((record, intact)) => {
                        off += record.stride();
                        if intact {
                            visit(&record);
                        }
                    }
                    None => break,
                }
            }
        }
        Ok(())
    }

    /// Read a record's body into `out`. Returns the number of bytes read,
    /// which is `min(out.len(), record.len)`.
    pub fn read_body(&mut self, record: &Record, out: &mut [u8]) -> Result<usize, Error<F::Error>> {
        let want = core::cmp::min(out.len(), record.len as usize);
        let mut written = 0usize;
        read_span(&mut self.flash, record.body_offset(), want, |chunk| {
            out[written..written + chunk.len()].copy_from_slice(chunk);
            written += chunk.len();
        })?;
        Ok(want)
    }

    /// Erase the next sector round-robin and make it active.
    fn advance(&mut self) -> Result<(), Error<F::Error>> {
        let next = (self.active + 1) % self.sectors;
        let seq = self.seq.wrapping_add(1);
        let sector = self.base + next * SECTOR_SIZE;
        erase_sector(&mut self.flash, sector)?;
        write_sector_header(&mut self.flash, sector, seq)?;
        self.active = next;
        self.seq = seq;
        self.cursor = SECTOR_HEADER_LEN;
        Ok(())
    }

    /// Program header, body and 0xFF padding as one forward run of
    /// 4-byte-aligned writes out of an aligned window.
    fn program_record(
        &mut self,
        offset: u32,
        header: &[u8; HEADER_LEN],
        body: &[u8],
    ) -> Result<(), Error<F::Error>> {
        let mut window = Aligned([FLAG_ERASED; WINDOW]);
        let mut filled = 0usize;
        let mut at = offset;
        for part in [header.as_slice(), body] {
            let mut rest = part;
            while !rest.is_empty() {
                let take = core::cmp::min(WINDOW - filled, rest.len());
                window.0[filled..filled + take].copy_from_slice(&rest[..take]);
                filled += take;
                rest = &rest[take..];
                if filled == WINDOW {
                    self.flash.write(at, &window.0).map_err(Error::Flash)?;
                    at += WINDOW as u32;
                    filled = 0;
                }
            }
        }
        if filled > 0 {
            let total = align_up(filled);
            // Padding stays erased: programming 0xFF clears no bit.
            window.0[filled..total].fill(FLAG_ERASED);
            self.flash
                .write(at, &window.0[..total])
                .map_err(Error::Flash)?;
        }
        Ok(())
    }

    /// Index of the sector records are currently appended to.
    pub fn active_sector(&self) -> u32 {
        self.active
    }

    /// Sequence number of the active sector: how many sectors this log has
    /// erased since it was formatted.
    pub fn sequence(&self) -> u32 {
        self.seq
    }

    /// Sectors in the region.
    pub fn sectors(&self) -> u32 {
        self.sectors
    }

    /// Bytes still free in the active sector.
    pub fn sector_room(&self) -> u32 {
        SECTOR_SIZE - self.cursor
    }

    /// The device, for an owner that needs it for something else — reading
    /// a JEDEC id, driving a second region. Writing inside this log's
    /// region behind its back corrupts it.
    pub fn flash_mut(&mut self) -> &mut F {
        &mut self.flash
    }

    /// Give the device back.
    pub fn into_flash(self) -> F {
        self.flash
    }
}

/// Whether `[base, base + len)` already carries this log's format.
///
/// Reads only, and borrows the device rather than taking it — which is
/// what lets a caller ask the question before deciding whether formatting
/// is its to do, and what lets a test assert that asking cost nothing.
pub fn is_formatted<F: NorFlash>(
    flash: &mut F,
    base: u32,
    len: u32,
) -> Result<bool, Error<F::Error>> {
    let sectors = check_region::<F>(base, len, flash.capacity())?;
    Ok(find_active(flash, base, sectors)?.is_some())
}

/// Check the region against the device's geometry and its own bounds,
/// returning the number of sectors in it.
fn check_region<F: NorFlash>(base: u32, len: u32, capacity: usize) -> Result<u32, Error<F::Error>> {
    let unit = PROGRAM_UNIT as usize;
    if F::ERASE_SIZE != SECTOR_SIZE as usize
        || F::WRITE_SIZE > unit
        || !unit.is_multiple_of(F::WRITE_SIZE)
        || F::READ_SIZE > unit
        || !unit.is_multiple_of(F::READ_SIZE)
    {
        return Err(Error::UnsupportedGeometry);
    }
    if !base.is_multiple_of(SECTOR_SIZE)
        || !len.is_multiple_of(SECTOR_SIZE)
        || len < 2 * SECTOR_SIZE
    {
        return Err(Error::BadRegion);
    }
    if u64::from(base) + u64::from(len) > capacity as u64 {
        return Err(Error::OutOfBounds);
    }
    Ok(len / SECTOR_SIZE)
}

/// The active sector and its sequence: the highest sequence on the part.
///
/// This read is the whole of the log's mount state. Nothing on the part
/// records where the head is, which is exactly why no sector wears faster
/// than the rest.
fn find_active<F: ReadNorFlash>(
    flash: &mut F,
    base: u32,
    sectors: u32,
) -> Result<Option<(u32, u32)>, Error<F::Error>> {
    let mut best: Option<(u32, u32)> = None;
    for idx in 0..sectors {
        if let Some(seq) = read_sector_header(flash, base + idx * SECTOR_SIZE)? {
            if best.is_none_or(|(_, s)| seq > s) {
                best = Some((idx, seq));
            }
        }
    }
    Ok(best)
}

/// Read a sector header. `Some(sequence)` iff it is intact.
fn read_sector_header<F: ReadNorFlash>(
    flash: &mut F,
    sector: u32,
) -> Result<Option<u32>, Error<F::Error>> {
    let mut buf = Aligned([0u8; SECTOR_HEADER_LEN as usize]);
    flash.read(sector, &mut buf.0).map_err(Error::Flash)?;
    if u32::from_le_bytes([buf.0[0], buf.0[1], buf.0[2], buf.0[3]]) != MAGIC || buf.0[8] != VERSION
    {
        return Ok(None);
    }
    let stored = u16::from_le_bytes([buf.0[10], buf.0[11]]);
    if crc16_update(CRC_INIT, &buf.0[0..10]) != stored {
        return Ok(None);
    }
    Ok(Some(u32::from_le_bytes([
        buf.0[4], buf.0[5], buf.0[6], buf.0[7],
    ])))
}

fn write_sector_header<F: NorFlash>(
    flash: &mut F,
    sector: u32,
    seq: u32,
) -> Result<(), Error<F::Error>> {
    let mut buf = Aligned([0u8; SECTOR_HEADER_LEN as usize]);
    buf.0[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf.0[4..8].copy_from_slice(&seq.to_le_bytes());
    buf.0[8] = VERSION;
    buf.0[9] = 0;
    let crc = crc16_update(CRC_INIT, &buf.0[0..10]);
    buf.0[10..12].copy_from_slice(&crc.to_le_bytes());
    flash.write(sector, &buf.0).map_err(Error::Flash)
}

fn erase_sector<F: NorFlash>(flash: &mut F, sector: u32) -> Result<(), Error<F::Error>> {
    flash
        .erase(sector, sector + SECTOR_SIZE)
        .map_err(Error::Flash)
}

/// Read `len` bytes from `off` — which need not be aligned — handing them to
/// `sink` in windows.
fn read_span<F: ReadNorFlash>(
    flash: &mut F,
    mut off: u32,
    mut len: usize,
    mut sink: impl FnMut(&[u8]),
) -> Result<(), Error<F::Error>> {
    let mut buf = Aligned([0u8; WINDOW]);
    while len > 0 {
        let start = off & !(PROGRAM_UNIT - 1);
        let skip = (off - start) as usize;
        let span = align_up(core::cmp::min(len + skip, WINDOW));
        flash
            .read(start, &mut buf.0[..span])
            .map_err(Error::Flash)?;
        let take = core::cmp::min(len, span - skip);
        sink(&buf.0[skip..skip + take]);
        off += take as u32;
        len -= take;
    }
    Ok(())
}

/// Read the record at `at`.
///
/// `None` means there is no record here and the scan of this sector ends:
/// the flags byte is still erased (nothing was ever committed here) or it
/// holds a value no commit produces, or the length is one no record could
/// have had. `Some((record, intact))` means a record was committed here and
/// occupies `record.stride()` bytes whether or not its CRC still checks —
/// `intact` says whether it does. A record whose body lost a bit therefore
/// costs itself and not the records behind it.
///
/// `room` is what is left of the sector from `at`; a header claiming a body
/// that would straddle the sector boundary is not a record.
fn probe_record<F: ReadNorFlash>(
    flash: &mut F,
    at: u32,
    room: u32,
) -> Result<Option<(Record, bool)>, Error<F::Error>> {
    debug_assert!(room >= MIN_STRIDE);
    let mut buf = Aligned([0u8; align_up(HEADER_LEN)]);
    flash.read(at, &mut buf.0).map_err(Error::Flash)?;

    let flags = buf.0[39];
    if flags != FLAG_LIVE && flags != FLAG_PURGED {
        return Ok(None);
    }
    let len = u16::from_le_bytes([buf.0[0], buf.0[1]]);
    if len as usize > MAX_BODY || record_stride(len as usize) as u32 > room {
        return Ok(None);
    }

    let stored = u16::from_le_bytes([buf.0[40], buf.0[41]]);
    let mut crc = crc16_update(CRC_INIT, &buf.0[0..39]);
    read_span(flash, at + HEADER_LEN as u32, len as usize, |chunk| {
        crc = crc16_update(crc, chunk);
    })?;

    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&buf.0[2..34]);
    let record = Record {
        key,
        time: u32::from_le_bytes([buf.0[34], buf.0[35], buf.0[36], buf.0[37]]),
        tag: buf.0[38],
        flags,
        len,
        offset: at,
    };
    Ok(Some((record, crc == stored)))
}

/// Where the next record goes in `sector`, given what survived.
///
/// Walks the committed records, then checks that the rest of the sector is
/// still erased. If it is not — a cut left a half-written record there —
/// the sector is sealed by returning [`SECTOR_SIZE`], because programming
/// over those bytes would have to raise bits.
fn scan_tail<F: ReadNorFlash>(flash: &mut F, sector: u32) -> Result<u32, Error<F::Error>> {
    let mut off = SECTOR_HEADER_LEN;
    while SECTOR_SIZE - off >= MIN_STRIDE {
        match probe_record(flash, sector + off, SECTOR_SIZE - off)? {
            // A record with a bad CRC still owns its bytes, so the cursor
            // steps over it exactly like an intact one.
            Some((record, _)) => off += record.stride(),
            None => break,
        }
    }
    if off < SECTOR_SIZE && !span_is_erased(flash, sector + off, SECTOR_SIZE - off)? {
        return Ok(SECTOR_SIZE);
    }
    Ok(off)
}

fn span_is_erased<F: ReadNorFlash>(
    flash: &mut F,
    off: u32,
    len: u32,
) -> Result<bool, Error<F::Error>> {
    let mut clean = true;
    read_span(flash, off, len as usize, |chunk| {
        if chunk.iter().any(|b| *b != FLAG_ERASED) {
            clean = false;
        }
    })?;
    Ok(clean)
}
