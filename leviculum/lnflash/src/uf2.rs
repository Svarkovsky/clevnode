//! UF2 blocks: read, write, filter by address, renumber.
//!
//! One rule is enforced here rather than left to callers: [`Image::encode`]
//! refuses to serialise a bootloader-family image. That family writes MBR,
//! bootloader and UICR, and a failure there needs an SWD probe to undo
//! (docs/src/concepts/lnode-flashing.md, "What a UF2 is allowed to write").
//! Making it unrepresentable in the one function that produces bytes is
//! cheaper than remembering not to.

use crate::ihex::Span;

pub const BLOCK_SIZE: usize = 512;
const HEADER_SIZE: usize = 32;
/// Bytes of payload a block can carry. The rest of the 512 is header and
/// trailing magic.
pub const MAX_PAYLOAD: usize = BLOCK_SIZE - HEADER_SIZE - 4;
/// Payload per block that every nRF tool in this ecosystem uses, and the
/// granularity a converted image is aligned to.
pub const PAGE: u32 = 256;

pub const MAGIC_START0: u32 = 0x0A32_4655;
pub const MAGIC_START1: u32 = 0x9E5D_5157;
pub const MAGIC_END: u32 = 0x0AB1_6F30;

/// The last word is a family ID rather than a file size.
pub const FLAG_FAMILY_ID: u32 = 0x0000_2000;
/// Block is not destined for main flash and must be ignored by the target.
pub const FLAG_NOT_MAIN_FLASH: u32 = 0x0000_0001;

/// nRF52840 application family — the only one we ever emit.
pub const FAMILY_NRF52840_APP: u32 = 0xADA5_2840;
/// nRF52 bootloader self-update. Never emit this; see the module docs.
pub const FAMILY_NRF52_BOOTLOADER: u32 = 0xD663_823C;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("{len} bytes is not a whole number of {BLOCK_SIZE}-byte UF2 blocks")]
    NotBlockAligned { len: usize },
    #[error("block {index}: bad start magic")]
    BadStartMagic { index: usize },
    #[error("block {index}: bad end magic")]
    BadEndMagic { index: usize },
    #[error("block {index}: payload size {size} exceeds {MAX_PAYLOAD}")]
    PayloadTooLarge { index: usize, size: usize },
    #[error("block {index}: says it is block {block_no} of {num_blocks}")]
    BadNumbering {
        index: usize,
        block_no: u32,
        num_blocks: u32,
    },
    #[error(
        "refusing to write a bootloader-family image ({FAMILY_NRF52_BOOTLOADER:#010x}): \
         it rewrites MBR, bootloader and UICR, and a failure there needs SWD to undo"
    )]
    BootloaderFamily,
    #[error("an image with no blocks")]
    Empty,
}

/// One 512-byte UF2 block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub flags: u32,
    pub target_addr: u32,
    pub block_no: u32,
    pub num_blocks: u32,
    /// familyID when [`FLAG_FAMILY_ID`] is set, otherwise the total file size.
    pub family_id: u32,
    /// Exactly `payloadSize` bytes; the on-disk zero padding is not kept.
    pub data: Vec<u8>,
}

impl Block {
    /// One past the last flash address this block covers.
    pub fn end(&self) -> u32 {
        self.target_addr + self.data.len() as u32
    }

    pub fn has_family_id(&self) -> bool {
        self.flags & FLAG_FAMILY_ID != 0
    }
}

/// A whole UF2 file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub blocks: Vec<Block>,
}

impl Image {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if !bytes.len().is_multiple_of(BLOCK_SIZE) {
            return Err(Error::NotBlockAligned { len: bytes.len() });
        }
        let blocks = bytes
            .chunks_exact(BLOCK_SIZE)
            .enumerate()
            .map(|(index, raw)| parse_block(raw, index))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { blocks })
    }

    /// Serialise, refusing a bootloader-family image. This is the only place
    /// UF2 bytes are produced, which is what makes that refusal total.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if self.blocks.is_empty() {
            return Err(Error::Empty);
        }
        if self
            .blocks
            .iter()
            .any(|b| b.has_family_id() && b.family_id == FAMILY_NRF52_BOOTLOADER)
        {
            return Err(Error::BootloaderFamily);
        }
        let mut out = Vec::with_capacity(self.blocks.len() * BLOCK_SIZE);
        for block in &self.blocks {
            if block.data.len() > MAX_PAYLOAD {
                return Err(Error::PayloadTooLarge {
                    index: block.block_no as usize,
                    size: block.data.len(),
                });
            }
            out.extend_from_slice(&MAGIC_START0.to_le_bytes());
            out.extend_from_slice(&MAGIC_START1.to_le_bytes());
            out.extend_from_slice(&block.flags.to_le_bytes());
            out.extend_from_slice(&block.target_addr.to_le_bytes());
            out.extend_from_slice(&(block.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&block.block_no.to_le_bytes());
            out.extend_from_slice(&block.num_blocks.to_le_bytes());
            out.extend_from_slice(&block.family_id.to_le_bytes());
            out.extend_from_slice(&block.data);
            out.resize(out.len() + (MAX_PAYLOAD - block.data.len()), 0);
            out.extend_from_slice(&MAGIC_END.to_le_bytes());
        }
        Ok(out)
    }

    /// Convert address/data spans into an application-family image.
    ///
    /// Blocks are aligned down to [`PAGE`] and zero-filled, which is what
    /// every nRF UF2 producer does and what makes the 608-block figure for
    /// the S140 hex reproducible.
    pub fn from_spans(spans: &[Span], family_id: u32) -> Self {
        let mut blocks: Vec<Block> = Vec::new();
        for span in spans {
            if span.is_empty() {
                continue;
            }
            let mut page = span.start & !(PAGE - 1);
            while page < span.end() {
                let from = page.max(span.start);
                let to = span.end().min(page + PAGE);
                let at = (from - page) as usize;
                let src = (from - span.start) as usize..(to - span.start) as usize;

                // A page already opened by an earlier span is filled further
                // rather than duplicated: two spans separated by a gap smaller
                // than a page still share one block.
                let block = match blocks.last_mut() {
                    Some(last) if last.target_addr == page => last,
                    _ => {
                        blocks.push(Block {
                            flags: FLAG_FAMILY_ID,
                            target_addr: page,
                            block_no: 0,
                            num_blocks: 0,
                            family_id,
                            data: vec![0u8; PAGE as usize],
                        });
                        blocks.last_mut().expect("just pushed")
                    }
                };
                block.data[at..at + src.len()].copy_from_slice(&span.data[src]);
                page += PAGE;
            }
        }
        let mut image = Self { blocks };
        image.renumber();
        image
    }

    /// The single family ID the whole image carries, or `None` if the image
    /// is empty, mixes families, or has blocks without the family flag.
    pub fn family_id(&self) -> Option<u32> {
        let first = self.blocks.first()?;
        if !first.has_family_id() {
            return None;
        }
        self.blocks
            .iter()
            .all(|b| b.has_family_id() && b.family_id == first.family_id)
            .then_some(first.family_id)
    }

    /// Lowest address written and one past the highest byte touched.
    pub fn address_range(&self) -> Option<(u32, u32)> {
        let low = self.blocks.iter().map(|b| b.target_addr).min()?;
        let high = self.blocks.iter().map(|b| b.end()).max()?;
        Some((low, high))
    }

    /// The discontiguous regions the image covers, in address order.
    pub fn spans(&self) -> Vec<(u32, u32)> {
        let mut ranges: Vec<(u32, u32)> = self
            .blocks
            .iter()
            .map(|b| (b.target_addr, b.end()))
            .collect();
        ranges.sort_unstable();
        let mut out: Vec<(u32, u32)> = Vec::new();
        for (start, end) in ranges {
            match out.last_mut() {
                Some(prev) if prev.1 >= start => prev.1 = prev.1.max(end),
                _ => out.push((start, end)),
            }
        }
        out
    }

    /// How many blocks target an address below `addr`. For an image the
    /// bootloader will partly decline, this is the count it silently drops.
    pub fn blocks_below(&self, addr: u32) -> usize {
        self.blocks.iter().filter(|b| b.target_addr < addr).count()
    }

    /// Keep only blocks at or above `addr`, renumbering the result. This is
    /// how a `CURRENT.UF2` dump becomes a restorable application image: the
    /// SoftDevice part of the dump would only rewrite identical bytes.
    pub fn filter_at_or_above(&self, addr: u32) -> Self {
        let mut out = Self {
            blocks: self
                .blocks
                .iter()
                .filter(|b| b.target_addr >= addr)
                .cloned()
                .collect(),
        };
        out.renumber();
        out
    }

    /// Rewrite `blockNo`/`numBlocks` to match the blocks actually present.
    pub fn renumber(&mut self) {
        let total = self.blocks.len() as u32;
        for (index, block) in self.blocks.iter_mut().enumerate() {
            block.block_no = index as u32;
            block.num_blocks = total;
        }
    }

    /// Check that the file's own numbering is self-consistent. A dump read
    /// off a board should satisfy this; nothing we build needs it, since
    /// `renumber` makes it true by construction.
    pub fn check_numbering(&self) -> Result<(), Error> {
        let total = self.blocks.len() as u32;
        for (index, block) in self.blocks.iter().enumerate() {
            if block.block_no != index as u32 || block.num_blocks != total {
                return Err(Error::BadNumbering {
                    index,
                    block_no: block.block_no,
                    num_blocks: block.num_blocks,
                });
            }
        }
        Ok(())
    }

    /// Read `len` bytes at an absolute flash address, if one block holds
    /// them all. Used to read the SoftDevice version word out of a
    /// `CURRENT.UF2` dump without writing anything.
    pub fn read_at(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let last = addr.checked_add(len as u32)?;
        let block = self
            .blocks
            .iter()
            .find(|b| b.target_addr <= addr && last <= b.end())?;
        let at = (addr - block.target_addr) as usize;
        block.data.get(at..at + len)
    }
}

fn parse_block(raw: &[u8], index: usize) -> Result<Block, Error> {
    let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
    if word(0) != MAGIC_START0 || word(4) != MAGIC_START1 {
        return Err(Error::BadStartMagic { index });
    }
    if word(BLOCK_SIZE - 4) != MAGIC_END {
        return Err(Error::BadEndMagic { index });
    }
    let size = word(16) as usize;
    if size > MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge { index, size });
    }
    Ok(Block {
        flags: word(8),
        target_addr: word(12),
        block_no: word(20),
        num_blocks: word(24),
        family_id: word(28),
        data: raw[HEADER_SIZE..HEADER_SIZE + size].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: u32, len: usize) -> Span {
        Span {
            start,
            data: (0..len).map(|i| (i % 251) as u8).collect(),
        }
    }

    #[test]
    fn a_written_image_reads_back_identical() {
        let image = Image::from_spans(&[span(0x2_7000, 700)], FAMILY_NRF52840_APP);
        let bytes = image.encode().unwrap();
        assert_eq!(bytes.len(), image.blocks.len() * BLOCK_SIZE);
        assert_eq!(Image::parse(&bytes).unwrap(), image);
    }

    #[test]
    fn conversion_aligns_blocks_down_to_a_page_and_zero_fills() {
        // 0x2710 is mid-page: the block starts at 0x2700 and the 16 bytes
        // before the span are zero.
        let image = Image::from_spans(&[span(0x2710, 16)], FAMILY_NRF52840_APP);
        assert_eq!(image.blocks.len(), 1);
        assert_eq!(image.blocks[0].target_addr, 0x2700);
        assert_eq!(image.blocks[0].data.len(), PAGE as usize);
        assert_eq!(&image.blocks[0].data[..0x10], &[0u8; 16]);
        assert_eq!(image.blocks[0].data[0x10], 0);
        assert_eq!(image.blocks[0].data[0x11], 1);
        assert_eq!(&image.blocks[0].data[0x20..], &[0u8; 224]);
    }

    #[test]
    fn two_spans_inside_one_page_share_a_block() {
        let spans = vec![span(0x1000, 4), span(0x1080, 4)];
        let image = Image::from_spans(&spans, FAMILY_NRF52840_APP);
        assert_eq!(image.blocks.len(), 1);
        assert_eq!(image.blocks[0].target_addr, 0x1000);
        assert_eq!(&image.blocks[0].data[0x04..0x80], &[0u8; 124]);
    }

    #[test]
    fn a_current_uf2_dump_filters_to_the_measured_backup_image() {
        // The backup path: `CURRENT.UF2` covers the whole writable window,
        // 0x1000..0xEA000. Filtered to the application region it is
        // restorable; the SoftDevice part would only rewrite identical bytes.
        // Verified on both rig boards 2026-08-09: 3120 of 3728 blocks.
        let dump = Image::from_spans(&[span(0x1000, 0xE9000)], FAMILY_NRF52840_APP);
        assert_eq!(dump.blocks.len(), 3728);
        let backup = dump.filter_at_or_above(0x2_7000);
        assert_eq!(backup.blocks.len(), 3120);
        assert_eq!(backup.address_range(), Some((0x2_7000, 0xEA000)));
        backup.check_numbering().unwrap();
    }

    #[test]
    fn numbering_is_written_by_conversion() {
        let image = Image::from_spans(&[span(0, 1000)], FAMILY_NRF52840_APP);
        assert_eq!(image.blocks.len(), 4);
        image.check_numbering().unwrap();
        assert_eq!(image.blocks[3].block_no, 3);
        assert_eq!(image.blocks[3].num_blocks, 4);
    }

    #[test]
    fn filtering_drops_low_blocks_and_renumbers_the_rest() {
        let image = Image::from_spans(&[span(0x0, 0x2_8000)], FAMILY_NRF52840_APP);
        let before = image.blocks.len();
        let filtered = image.filter_at_or_above(0x2_7000);
        assert_eq!(filtered.blocks.len(), before - 0x27000 / PAGE as usize);
        assert_eq!(filtered.blocks[0].target_addr, 0x2_7000);
        filtered.check_numbering().unwrap();
        assert_eq!(filtered.blocks[0].num_blocks, filtered.blocks.len() as u32);
    }

    #[test]
    fn blocks_below_the_writable_window_are_counted_not_dropped() {
        let image = Image::from_spans(
            &[span(0x0, 0xB00), span(0x1000, 0x400)],
            FAMILY_NRF52840_APP,
        );
        assert_eq!(image.blocks_below(0x1000), 11);
        assert_eq!(image.blocks.len(), 11 + 4);
    }

    #[test]
    fn encoding_a_bootloader_family_image_is_refused() {
        let mut image = Image::from_spans(&[span(0, 16)], FAMILY_NRF52_BOOTLOADER);
        assert_eq!(image.encode(), Err(Error::BootloaderFamily));
        // ...and the same bytes under the application family are fine.
        image.blocks[0].family_id = FAMILY_NRF52840_APP;
        assert!(image.encode().is_ok());
    }

    #[test]
    fn a_truncated_file_is_rejected_rather_than_half_read() {
        let bytes = Image::from_spans(&[span(0, 600)], FAMILY_NRF52840_APP)
            .encode()
            .unwrap();
        assert_eq!(
            Image::parse(&bytes[..bytes.len() - 8]),
            Err(Error::NotBlockAligned {
                len: bytes.len() - 8
            })
        );
    }

    #[test]
    fn a_corrupted_magic_is_rejected() {
        let mut bytes = Image::from_spans(&[span(0, 600)], FAMILY_NRF52840_APP)
            .encode()
            .unwrap();
        bytes[BLOCK_SIZE] ^= 0xFF;
        assert_eq!(Image::parse(&bytes), Err(Error::BadStartMagic { index: 1 }));
        bytes[BLOCK_SIZE] ^= 0xFF;
        bytes[2 * BLOCK_SIZE - 1] ^= 0xFF;
        assert_eq!(Image::parse(&bytes), Err(Error::BadEndMagic { index: 1 }));
    }

    #[test]
    fn family_and_range_are_reported_off_the_blocks() {
        let image = Image::from_spans(
            &[span(0x0, 0xB00), span(0x1000, 0x400)],
            FAMILY_NRF52840_APP,
        );
        assert_eq!(image.family_id(), Some(FAMILY_NRF52840_APP));
        assert_eq!(image.address_range(), Some((0x0, 0x1400)));
        assert_eq!(image.spans(), vec![(0x0, 0xB00), (0x1000, 0x1400)]);
    }

    #[test]
    fn a_mixed_family_image_reports_no_family() {
        let mut image = Image::from_spans(&[span(0, 600)], FAMILY_NRF52840_APP);
        image.blocks[1].family_id = 0x1234_5678;
        assert_eq!(image.family_id(), None);
    }

    #[test]
    fn reading_a_word_at_an_absolute_address_crosses_no_block_boundary() {
        let image = Image::from_spans(&[span(0x1000, 0x400)], FAMILY_NRF52840_APP);
        assert_eq!(image.read_at(0x1004, 4).unwrap().len(), 4);
        // 4 bytes straddling the 0x1100 boundary live in two blocks.
        assert_eq!(image.read_at(0x10FE, 4), None);
        assert_eq!(image.read_at(0x9999, 4), None);
    }
}
