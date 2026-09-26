//! Intel HEX into address/data spans.
//!
//! Only what a firmware image needs: data records, both extended-address
//! forms, and the two start-address records, which carry no flash content
//! and are skipped. The parser is strict — a bad checksum or a truncated
//! record is an error, never a warning — because its output decides what
//! lands in somebody else's flash.

use std::fmt;

/// A contiguous run of bytes at a known absolute address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Absolute address of the first byte.
    pub start: u32,
    pub data: Vec<u8>,
}

impl Span {
    /// One past the last address this span covers.
    pub fn end(&self) -> u32 {
        self.start + self.data.len() as u32
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}-{:#x}", self.start, self.end())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("line {line}: does not start with ':'")]
    NotARecord { line: usize },
    #[error("line {line}: not an even run of hex digits")]
    BadHexDigits { line: usize },
    #[error("line {line}: record shorter than the 5 mandatory bytes")]
    ShortRecord { line: usize },
    #[error("line {line}: byte count {declared} disagrees with the {actual} bytes present")]
    LengthMismatch {
        line: usize,
        declared: usize,
        actual: usize,
    },
    #[error("line {line}: checksum {declared:#04x}, computed {computed:#04x}")]
    Checksum {
        line: usize,
        declared: u8,
        computed: u8,
    },
    #[error("line {line}: unknown record type {kind:#04x}")]
    UnknownRecordType { line: usize, kind: u8 },
    #[error("line {line}: address record type {kind:#04x} must carry exactly 2 bytes")]
    BadAddressRecord { line: usize, kind: u8 },
    #[error("two records both claim address {at:#x}")]
    Overlap { at: u32 },
    #[error("no end-of-file record")]
    NoEof,
}

const REC_DATA: u8 = 0x00;
const REC_EOF: u8 = 0x01;
const REC_EXT_SEGMENT: u8 = 0x02;
const REC_START_SEGMENT: u8 = 0x03;
const REC_EXT_LINEAR: u8 = 0x04;
const REC_START_LINEAR: u8 = 0x05;

/// Parse Intel HEX text into address-sorted, non-overlapping spans.
///
/// Adjacent records are coalesced, so the span count is the number of
/// discontiguous regions in the image, not the number of records.
pub fn parse(text: &str) -> Result<Vec<Span>, Error> {
    let mut chunks: Vec<Span> = Vec::new();
    // Extended segment (<<4) and extended linear (<<16) both reduce to a
    // base that is added to the record offset: a linear base has its low
    // 16 bits clear, so `+` and `|` agree there.
    let mut base: u32 = 0;
    let mut seen_eof = false;

    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let record = raw.trim();
        if record.is_empty() {
            continue;
        }
        let body = record.strip_prefix(':').ok_or(Error::NotARecord { line })?;
        let bytes = decode_hex(body).ok_or(Error::BadHexDigits { line })?;
        if bytes.len() < 5 {
            return Err(Error::ShortRecord { line });
        }
        let declared = bytes[0] as usize;
        if bytes.len() != declared + 5 {
            return Err(Error::LengthMismatch {
                line,
                declared,
                actual: bytes.len() - 5,
            });
        }
        verify_checksum(&bytes, line)?;

        let offset = u32::from(u16::from_be_bytes([bytes[1], bytes[2]]));
        let kind = bytes[3];
        let data = &bytes[4..4 + declared];

        match kind {
            REC_DATA => push(&mut chunks, base.wrapping_add(offset), data),
            REC_EOF => {
                seen_eof = true;
                break;
            }
            REC_EXT_SEGMENT | REC_EXT_LINEAR => {
                if declared != 2 {
                    return Err(Error::BadAddressRecord { line, kind });
                }
                let value = u32::from(u16::from_be_bytes([data[0], data[1]]));
                base = if kind == REC_EXT_SEGMENT {
                    value << 4
                } else {
                    value << 16
                };
            }
            // Entry points, not flash content. The UF2 bootloader takes its
            // entry point from the vector table, so these say nothing we act on.
            REC_START_SEGMENT | REC_START_LINEAR => {}
            _ => return Err(Error::UnknownRecordType { line, kind }),
        }
    }

    if !seen_eof {
        return Err(Error::NoEof);
    }
    merge(chunks)
}

/// Append to the span in progress when the address continues it, otherwise
/// open a new one. Handles the common case — a monotonic file — in one pass;
/// `merge` cleans up files that jump around.
fn push(chunks: &mut Vec<Span>, addr: u32, data: &[u8]) {
    match chunks.last_mut() {
        Some(last) if last.end() == addr => last.data.extend_from_slice(data),
        _ => chunks.push(Span {
            start: addr,
            data: data.to_vec(),
        }),
    }
}

fn merge(mut chunks: Vec<Span>) -> Result<Vec<Span>, Error> {
    chunks.sort_by_key(|s| s.start);
    let mut out: Vec<Span> = Vec::with_capacity(chunks.len());
    for span in chunks {
        match out.last_mut() {
            Some(prev) if prev.end() == span.start => prev.data.extend_from_slice(&span.data),
            Some(prev) if prev.end() > span.start => return Err(Error::Overlap { at: span.start }),
            _ => out.push(span),
        }
    }
    Ok(out)
}

fn verify_checksum(bytes: &[u8], line: usize) -> Result<(), Error> {
    let (declared, payload) = bytes.split_last().expect("length checked by caller");
    let sum = payload.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    let computed = (!sum).wrapping_add(1);
    if computed == *declared {
        Ok(())
    } else {
        Err(Error::Checksum {
            line,
            declared: *declared,
            computed,
        })
    }
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let raw = s.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `:0AAAAATT<data>CC` with the checksum filled in, so tests state the
    /// record they mean rather than a hand-computed trailer.
    fn record(kind: u8, addr: u16, data: &[u8]) -> String {
        let mut bytes = vec![data.len() as u8];
        bytes.extend_from_slice(&addr.to_be_bytes());
        bytes.push(kind);
        bytes.extend_from_slice(data);
        let sum = bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        bytes.push((!sum).wrapping_add(1));
        let mut s = String::from(":");
        for b in bytes {
            s.push_str(&format!("{b:02X}"));
        }
        s
    }

    fn eof() -> String {
        record(REC_EOF, 0, &[])
    }

    #[test]
    fn contiguous_records_coalesce_into_one_span() {
        let text = format!(
            "{}\n{}\n{}\n",
            record(REC_DATA, 0x0000, &[1, 2, 3, 4]),
            record(REC_DATA, 0x0004, &[5, 6]),
            eof()
        );
        let spans = parse(&text).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].data, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(spans[0].end(), 6);
    }

    #[test]
    fn a_gap_opens_a_second_span() {
        let text = format!(
            "{}\n{}\n{}\n",
            record(REC_DATA, 0x0000, &[1, 2]),
            record(REC_DATA, 0x0010, &[3, 4]),
            eof()
        );
        let spans = parse(&text).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].start, spans[0].end()), (0x0, 0x2));
        assert_eq!((spans[1].start, spans[1].end()), (0x10, 0x12));
    }

    #[test]
    fn extended_linear_address_raises_the_base() {
        let text = format!(
            "{}\n{}\n{}\n",
            record(REC_EXT_LINEAR, 0, &[0x00, 0x01]),
            record(REC_DATA, 0x2000, &[0xAA]),
            eof()
        );
        let spans = parse(&text).unwrap();
        assert_eq!(spans[0].start, 0x0001_2000);
    }

    #[test]
    fn extended_segment_address_scales_by_sixteen() {
        let text = format!(
            "{}\n{}\n{}\n",
            record(REC_EXT_SEGMENT, 0, &[0x10, 0x00]),
            record(REC_DATA, 0x0020, &[0xAA]),
            eof()
        );
        let spans = parse(&text).unwrap();
        assert_eq!(spans[0].start, 0x0001_0020);
    }

    #[test]
    fn start_address_records_contribute_no_bytes() {
        let text = format!(
            "{}\n{}\n{}\n{}\n",
            record(REC_START_LINEAR, 0, &[0x00, 0x01, 0x00, 0x00]),
            record(REC_DATA, 0x0000, &[0xAA]),
            record(REC_START_SEGMENT, 0, &[0x00, 0x00, 0x00, 0x00]),
            eof()
        );
        let spans = parse(&text).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].data, vec![0xAA]);
    }

    #[test]
    fn crlf_and_blank_lines_are_tolerated() {
        let text = format!("{}\r\n\r\n{}\r\n", record(REC_DATA, 0, &[7]), eof());
        assert_eq!(parse(&text).unwrap()[0].data, vec![7]);
    }

    #[test]
    fn the_two_line_endings_parse_to_the_same_spans() {
        // Nordic ships the S140 hex with CRLF, so that is the ending the
        // vendored payload exercises. This keeps the LF path covered, and
        // pins the property that decides it: the ending is not content.
        let records = [
            record(REC_EXT_LINEAR, 0, &[0x00, 0x01]),
            record(REC_DATA, 0x2000, &[1, 2, 3, 4]),
            record(REC_DATA, 0x2004, &[5, 6]),
            eof(),
        ];
        let lf = records.join("\n") + "\n";
        let crlf = records.join("\r\n") + "\r\n";
        assert!(!lf.contains('\r'));

        let from_lf = parse(&lf).unwrap();
        assert_eq!(from_lf.len(), 1);
        assert_eq!(from_lf[0].start, 0x0001_2000);
        assert_eq!(from_lf[0].data, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(parse(&crlf).unwrap(), from_lf);
    }

    #[test]
    fn a_flipped_checksum_byte_is_an_error() {
        let mut good = record(REC_DATA, 0x0000, &[1, 2, 3, 4]);
        good.pop();
        good.pop();
        good.push_str("FF");
        let text = format!("{}\n{}\n", good, eof());
        assert!(matches!(parse(&text), Err(Error::Checksum { line: 1, .. })));
    }

    #[test]
    fn a_byte_count_that_lies_is_an_error() {
        // Declares 4 data bytes, carries 2.
        let text = format!(":04000000010266\n{}\n", eof());
        assert!(matches!(
            parse(&text),
            Err(Error::LengthMismatch { line: 1, .. })
        ));
    }

    #[test]
    fn a_missing_eof_record_is_an_error() {
        let text = format!("{}\n", record(REC_DATA, 0, &[1]));
        assert_eq!(parse(&text), Err(Error::NoEof));
    }

    #[test]
    fn a_line_without_a_colon_is_an_error() {
        let text = format!("oops\n{}\n", eof());
        assert_eq!(parse(&text), Err(Error::NotARecord { line: 1 }));
    }

    #[test]
    fn an_unknown_record_type_is_an_error() {
        let text = format!("{}\n{}\n", record(0x06, 0, &[]), eof());
        assert!(matches!(
            parse(&text),
            Err(Error::UnknownRecordType {
                line: 1,
                kind: 0x06
            })
        ));
    }

    #[test]
    fn records_that_double_back_are_sorted_and_merged() {
        let text = format!(
            "{}\n{}\n{}\n",
            record(REC_DATA, 0x0004, &[3, 4]),
            record(REC_DATA, 0x0000, &[1, 2, 3, 4]),
            eof()
        );
        let spans = parse(&text).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].data, vec![1, 2, 3, 4, 3, 4]);
    }

    #[test]
    fn two_records_claiming_the_same_address_are_an_error() {
        let text = format!(
            "{}\n{}\n{}\n",
            record(REC_DATA, 0x0000, &[1, 2, 3, 4]),
            record(REC_DATA, 0x0002, &[9]),
            eof()
        );
        assert_eq!(parse(&text), Err(Error::Overlap { at: 0x2 }));
    }

    #[test]
    fn records_after_eof_are_not_read() {
        let text = format!("{}\n{}\n", eof(), record(REC_DATA, 0, &[1]));
        assert!(parse(&text).unwrap().is_empty());
    }
}
