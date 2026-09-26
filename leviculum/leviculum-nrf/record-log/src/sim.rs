//! A simulated NOR part with the semantics that actually bite.
//!
//! Three of them, and each is a bug this crate could otherwise ship:
//!
//! 1. **Erase sets `0xFF`, programming only clears bits.** A program that
//!    would raise a bit panics rather than quietly doing what RAM would do,
//!    because on the real part it would leave the byte at its old value and
//!    the store would read back something it never wrote. `SimNor` is the
//!    only place in the test suite that can catch that.
//! 2. **4 KB erase granularity**, and 4-byte alignment on every program and
//!    read address, length and buffer — the nRF52840 QSPI peripheral asserts
//!    all three (`start_write`/`start_read`, `embassy-nrf-0.9.0/src/qspi.rs`)
//!    and a violation there is a panic inside the driver, on the board, in
//!    the field.
//! 3. **Power cuts at a byte.** [`SimNor::arm_power_cut`] gives the device a
//!    budget of program bytes; when it runs out, the bytes up to that point
//!    are on the part, everything after is not, and every later access fails
//!    until [`SimNor::power_on`]. That is what makes "reopen with every
//!    completed record and no partial one" a testable claim at every offset
//!    rather than at three hand-picked ones.
//!
//! It also counts two things the log cannot see from the inside. Erases per
//! sector are the wear pin: round-robin reclaim is only level wear if the
//! counts stay within one of each other, and a store with a fixed metadata
//! sector fails that immediately. Program operations per [`PAGE_SIZE`] page
//! since that page was last erased are the exposure the commit word buys:
//! re-programming a byte to the value it already holds changes no bit but is
//! still a program, and how many of those a page accepts between erases is a
//! datasheet property of the part. [`SimNor::max_page_programs`] turns that
//! from an argument into a number.

use alloc::vec;
use alloc::vec::Vec;

use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

use crate::{PROGRAM_UNIT, SECTOR_SIZE};

/// The erased state of a NOR cell.
pub const ERASED: u8 = 0xFF;

/// Program-page size. A NOR page program applies to one page and no more,
/// so a write spanning two pages is two program operations on the part —
/// which is the unit a datasheet's partial-program limit is stated in.
pub const PAGE_SIZE: u32 = 256;

/// What a simulated part refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimError {
    /// The access ran past the end of the part.
    OutOfBounds,
    /// The part lost power mid-operation and has not been powered on again.
    PowerCut,
}

impl NorFlashError for SimError {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            SimError::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            SimError::PowerCut => NorFlashErrorKind::Other,
        }
    }
}

/// A NOR part in memory.
pub struct SimNor {
    data: Vec<u8>,
    erases: Vec<u32>,
    /// Program operations each page has taken since it was last erased.
    page_programs: Vec<u32>,
    /// Program/erase bytes left before the lights go out. `None` = mains.
    budget: Option<usize>,
    /// Program/erase bytes this part has done since it was made. What the
    /// power-cut sweep uses to learn how many offsets a record write has.
    spent: usize,
    dark: bool,
}

impl SimNor {
    /// A fresh, fully erased part of `sectors` × 4 KB.
    pub fn new(sectors: u32) -> Self {
        Self {
            data: vec![ERASED; (sectors * SECTOR_SIZE) as usize],
            erases: vec![0; sectors as usize],
            page_programs: vec![0; (sectors * SECTOR_SIZE / PAGE_SIZE) as usize],
            budget: None,
            spent: 0,
            dark: false,
        }
    }

    /// Cut the power once `bytes` further bytes have been programmed or
    /// erased. `0` cuts before the very next byte.
    pub fn arm_power_cut(&mut self, bytes: usize) {
        self.budget = Some(bytes);
    }

    /// Power the part back on: the budget is gone and the contents are
    /// whatever survived.
    pub fn power_on(&mut self) {
        self.budget = None;
        self.dark = false;
    }

    /// Whether the armed cut has fired.
    pub fn is_dark(&self) -> bool {
        self.dark
    }

    /// Bytes programmed or erased since the part was made.
    pub fn spent(&self) -> usize {
        self.spent
    }

    /// Erases per sector, index by sector. The wear pin reads this.
    pub fn erase_counts(&self) -> &[u32] {
        &self.erases
    }

    /// Program operations per page since that page was last erased, indexed
    /// by page. A write that spans two pages counts on both.
    pub fn page_programs(&self) -> &[u32] {
        &self.page_programs
    }

    /// The most program operations any one page has taken since its last
    /// erase. This is the number a part's partial-program limit is compared
    /// against.
    pub fn max_page_programs(&self) -> u32 {
        self.page_programs.iter().copied().max().unwrap_or(0)
    }

    /// Raw contents, for a byte-exact digest or a deliberate corruption.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Clear bits in one byte directly, the way a decaying cell would.
    ///
    /// Only clearing is possible, so `value` must be reachable from what is
    /// there — and it must actually change the byte. A corruption that
    /// silently does nothing turns the test that uses it into a test of
    /// nothing, so it panics instead.
    pub fn corrupt(&mut self, offset: u32, value: u8) {
        let cell = &mut self.data[offset as usize];
        let after = *cell & value;
        assert_ne!(
            after, *cell,
            "corrupting {offset:#x} with {value:#04x} over {cell:#04x} changes no bit"
        );
        *cell = after;
    }

    /// Sectors on the part.
    pub fn sectors(&self) -> u32 {
        self.erases.len() as u32
    }

    /// Charge `n` bytes against the power budget and return how many of
    /// them actually happen. A budget that lands exactly on the end of an
    /// operation lets that operation finish and cuts before the next one.
    fn charge(&mut self, n: usize) -> usize {
        let done = match self.budget {
            None => n,
            Some(left) => core::cmp::min(left, n),
        };
        if let Some(left) = self.budget {
            self.budget = Some(left - done);
        }
        self.spent += done;
        if done < n {
            self.dark = true;
        }
        done
    }

    /// The pages `len` bytes from `offset` touch. Empty for `len == 0`: a
    /// cut before the first byte applies no pulse to anything.
    fn pages(offset: u32, len: usize) -> core::ops::Range<usize> {
        if len == 0 {
            return 0..0;
        }
        let first = (offset / PAGE_SIZE) as usize;
        let last = ((offset as usize + len - 1) / PAGE_SIZE as usize) + 1;
        first..last
    }

    fn check_live(&self) -> Result<(), SimError> {
        if self.dark {
            return Err(SimError::PowerCut);
        }
        Ok(())
    }
}

impl ErrorType for SimNor {
    type Error = SimError;
}

impl ReadNorFlash for SimNor {
    const READ_SIZE: usize = PROGRAM_UNIT as usize;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.check_live()?;
        assert_eq!(
            offset % PROGRAM_UNIT,
            0,
            "read address must be 4-byte aligned"
        );
        assert_eq!(
            bytes.len() % PROGRAM_UNIT as usize,
            0,
            "read length must be a multiple of 4"
        );
        assert_eq!(
            bytes.as_ptr() as usize % PROGRAM_UNIT as usize,
            0,
            "read buffer must be 4-byte aligned"
        );
        let end = offset as usize + bytes.len();
        if end > self.data.len() {
            return Err(SimError::OutOfBounds);
        }
        bytes.copy_from_slice(&self.data[offset as usize..end]);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.data.len()
    }
}

impl NorFlash for SimNor {
    const WRITE_SIZE: usize = PROGRAM_UNIT as usize;
    const ERASE_SIZE: usize = SECTOR_SIZE as usize;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.check_live()?;
        assert_eq!(from % SECTOR_SIZE, 0, "erase start must be sector-aligned");
        assert_eq!(to % SECTOR_SIZE, 0, "erase end must be sector-aligned");
        assert!(from < to, "erase range must be non-empty");
        if to as usize > self.data.len() {
            return Err(SimError::OutOfBounds);
        }
        for sector in (from..to).step_by(SECTOR_SIZE as usize) {
            self.erases[(sector / SECTOR_SIZE) as usize] += 1;
            // An erase is not atomic either: charge it byte by byte so a cut
            // can land inside one, which is what the reclaim path has to
            // survive.
            let done = self.charge(SECTOR_SIZE as usize);
            let base = sector as usize;
            self.data[base..base + done].fill(ERASED);
            // Only a page erased end to end starts counting again. A cut
            // mid-erase leaves the page it stopped in with the programs it
            // had, which is the conservative reading and the one the part
            // will hold us to on the retry.
            let erase_base = sector / PAGE_SIZE;
            for page in erase_base..(sector + done as u32) / PAGE_SIZE {
                self.page_programs[page as usize] = 0;
            }
            if done < SECTOR_SIZE as usize {
                return Err(SimError::PowerCut);
            }
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.check_live()?;
        assert_eq!(
            offset % PROGRAM_UNIT,
            0,
            "program address must be 4-byte aligned"
        );
        assert_eq!(
            bytes.len() % PROGRAM_UNIT as usize,
            0,
            "program length must be a multiple of 4"
        );
        assert_eq!(
            bytes.as_ptr() as usize % PROGRAM_UNIT as usize,
            0,
            "program buffer must be 4-byte aligned"
        );
        let end = offset as usize + bytes.len();
        if end > self.data.len() {
            return Err(SimError::OutOfBounds);
        }
        let done = self.charge(bytes.len());
        for page in Self::pages(offset, done) {
            self.page_programs[page] += 1;
        }
        for (i, byte) in bytes[..done].iter().enumerate() {
            let cell = &mut self.data[offset as usize + i];
            assert!(
                *byte & !*cell == 0,
                "program at {:#x} would raise a bit: {:#04x} over {:#04x}. \
                 NOR cannot do that; the sector has to be erased first.",
                offset as usize + i,
                byte,
                cell
            );
            *cell &= *byte;
        }
        if done < bytes.len() {
            return Err(SimError::PowerCut);
        }
        Ok(())
    }
}
