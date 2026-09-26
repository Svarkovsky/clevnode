//! Persistent storage for a node's operator-chosen name (Codeberg
//! #235/#238).
//!
//! A host sets the name over the #238 control envelope
//! ([`crate::envelope::TYPE_NODE_NAME`]). Without persistence it would
//! live only in RAM, and the first reset would put the board back to its
//! derived `LNode-<hex8>` — which is precisely the state an operator
//! used the command to leave.
//!
//! Stored form: the same magic + version + checksum envelope the
//! telemetry-target, fixed-position and media-profile stores use, so a
//! blank (erased, all-`0xFF`) or corrupt region decodes to `None`. The
//! record shares the telemetry flash page with those three (the firmware
//! names the layout in `leviculum_nrf::telemetry`); this module only
//! fixes the record's bytes.
//!
//! Layout (41 bytes, padded to 44 for 4-byte flash writes):
//!
//! ```text
//!  0..4   magic "LNAM"
//!  4      format version (0x01)
//!  5      set flag (0x00 = cleared, 0x01 = set), never inferred
//!  6      name length in bytes (0..=NODE_NAME_MAX_LEN)
//!  7..39  name bytes, zero-filled past the length
//! 39..41  checksum over bytes 0..39
//! 41..44  padding
//! ```
//!
//! An explicitly cleared name is stored as a valid record with the set
//! flag absent rather than as an erased region, for the same reason the
//! fixed position is: the store task always rewrites the whole record,
//! and "cleared by the operator" and "never named" mean the same thing on
//! the next boot — the derived default.
//!
//! **`None` is not "no name".** It is "no record", and the caller's
//! default for that is the derived `LNode-<hex8>` / `LN-<hex8>` pair — a
//! board that was never named must stay exactly as distinguishable as it
//! is today.

use crate::node_name::{NodeName, NODE_NAME_MAX_LEN};

const MAGIC: [u8; 4] = [0x4C, 0x4E, 0x41, 0x4D]; // "LNAM"
const FORMAT_VERSION: u8 = 0x01;
const SET_OFFSET: usize = 5;
const LEN_OFFSET: usize = 6;
const NAME_OFFSET: usize = 7;
const CHECKSUM_OFFSET: usize = NAME_OFFSET + NODE_NAME_MAX_LEN; // 39
const CHECKSUM_SIZE: usize = 2;

/// Total encoded size: 39 header+payload + 2 checksum = 41 bytes.
pub const ENCODED_SIZE: usize = CHECKSUM_OFFSET + CHECKSUM_SIZE;

/// Encoded size rounded up to 4-byte alignment (flash writes are
/// word-wide).
pub const ENCODED_SIZE_ALIGNED: usize = ENCODED_SIZE.div_ceil(4) * 4; // 44

/// Same two-byte XOR checksum the identity, radio-config,
/// telemetry-target, fixed-position and media-profile stores use:
/// even-indexed bytes into `a`, odd into `b`.
fn checksum(data: &[u8]) -> [u8; 2] {
    let mut a: u8 = 0;
    let mut b: u8 = 0;
    for (i, &byte) in data.iter().enumerate() {
        if i % 2 == 0 {
            a ^= byte;
        } else {
            b ^= byte;
        }
    }
    [a, b]
}

/// Encode a node name — or the explicit clear (`None`) — into a
/// fixed-size buffer for persistent storage.
pub fn encode_node_name(name: Option<&NodeName>) -> [u8; ENCODED_SIZE_ALIGNED] {
    let mut buf = [0u8; ENCODED_SIZE_ALIGNED];
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4] = FORMAT_VERSION;
    if let Some(name) = name {
        buf[SET_OFFSET] = 0x01;
        buf[LEN_OFFSET] = name.len() as u8;
        buf[NAME_OFFSET..NAME_OFFSET + name.len()].copy_from_slice(name.as_bytes());
    }
    let cs = checksum(&buf[..CHECKSUM_OFFSET]);
    buf[CHECKSUM_OFFSET] = cs[0];
    buf[CHECKSUM_OFFSET + 1] = cs[1];
    buf
}

/// Decode a node name from a persistent storage buffer.
///
/// Returns `None` for a blank region (erased flash reads as `0xFF`), a
/// wrong magic or version, a set flag that is neither 0 nor 1, a length
/// past [`NODE_NAME_MAX_LEN`], a checksum mismatch, a name this firmware
/// cannot display ([`NodeName::decode`]) — and for a valid record whose
/// set flag says cleared. Every one of those means "no stored name",
/// which is the derived default, which is today's behaviour.
///
/// The empty name is **not** a stored name either: a record claiming
/// `set=1, len=0` would leave both name surfaces blank, and a board
/// nobody can point at on a scanner list is worse than one with a hex
/// suffix.
pub fn decode_node_name(buf: &[u8]) -> Option<NodeName> {
    if buf.len() < ENCODED_SIZE {
        return None;
    }
    if buf[0] == 0xFF {
        return None; // erased flash
    }
    if buf[0..4] != MAGIC {
        return None;
    }
    if buf[4] != FORMAT_VERSION {
        return None;
    }
    let stored = [buf[CHECKSUM_OFFSET], buf[CHECKSUM_OFFSET + 1]];
    if stored != checksum(&buf[..CHECKSUM_OFFSET]) {
        return None;
    }
    match buf[SET_OFFSET] {
        0x00 => return None, // explicitly cleared
        0x01 => {}
        _ => return None,
    }
    let len = usize::from(buf[LEN_OFFSET]);
    if len == 0 || len > NODE_NAME_MAX_LEN {
        return None;
    }
    NodeName::decode(&buf[NAME_OFFSET..NAME_OFFSET + len]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> NodeName {
        NodeName::parse(text.as_bytes()).unwrap()
    }

    #[test]
    fn a_name_round_trips() {
        let n = name("Balkon-Nord");
        assert_eq!(decode_node_name(&encode_node_name(Some(&n))), Some(n));
    }

    #[test]
    fn a_name_at_the_cap_round_trips() {
        // The record is sized for exactly this and the checksum must
        // still land where the decoder looks for it.
        let n = name(&"x".repeat(NODE_NAME_MAX_LEN));
        assert_eq!(decode_node_name(&encode_node_name(Some(&n))), Some(n));
    }

    #[test]
    fn a_utf8_name_round_trips_byte_for_byte() {
        let n = name("Küche");
        let back = decode_node_name(&encode_node_name(Some(&n))).unwrap();
        assert_eq!(back.as_str(), "Küche");
        assert_eq!(back.len(), 6);
    }

    #[test]
    fn an_explicit_clear_is_a_record_and_reads_as_no_name() {
        // Not an absence: the store task rewrites the whole record on
        // every save, so "cleared" has to survive being written down.
        // What it means on read is the same as never-named — the derived
        // default — which is why both are `None`.
        let cleared = encode_node_name(None);
        assert_ne!(cleared[0], 0xFF, "a clear is written, not erased");
        assert_eq!(decode_node_name(&cleared), None);
    }

    #[test]
    fn a_blank_region_is_no_record() {
        assert!(decode_node_name(&[0xFF; ENCODED_SIZE_ALIGNED]).is_none());
    }

    #[test]
    fn a_zeroed_region_is_no_record() {
        assert!(decode_node_name(&[0x00; ENCODED_SIZE_ALIGNED]).is_none());
    }

    #[test]
    fn a_bit_flip_anywhere_is_no_record() {
        // The torn-write case: a save interrupted mid-page must not
        // produce a board announcing a name nobody set.
        for i in 0..ENCODED_SIZE {
            let mut buf = encode_node_name(Some(&name("Balkon-Nord")));
            buf[i] ^= 0x01;
            assert!(
                decode_node_name(&buf).is_none(),
                "bit flip at byte {i} was accepted"
            );
        }
    }

    #[test]
    fn a_truncated_page_is_no_record() {
        // The other torn write: the erase landed but the record did not,
        // so the tail of the page is still erased flash inside a record
        // whose head is ours.
        let mut buf = encode_node_name(Some(&name("Balkon-Nord")));
        buf[NAME_OFFSET + 4..].fill(0xFF);
        assert!(decode_node_name(&buf).is_none());
    }

    #[test]
    fn a_wrong_magic_or_version_is_no_record() {
        let mut buf = encode_node_name(Some(&name("Balkon")));
        buf[0] = 0x00;
        assert!(decode_node_name(&buf).is_none());

        let mut buf = encode_node_name(Some(&name("Balkon")));
        buf[4] = 0x99;
        reseal(&mut buf);
        assert!(decode_node_name(&buf).is_none());
    }

    #[test]
    fn a_length_past_the_cap_is_no_record() {
        // A record written by firmware with a larger cap. Reading it as
        // a truncated name would put a name on the air that nobody set;
        // the derived default is the honest answer.
        let mut buf = encode_node_name(Some(&name("Balkon")));
        buf[LEN_OFFSET] = (NODE_NAME_MAX_LEN + 1) as u8;
        reseal(&mut buf);
        assert!(decode_node_name(&buf).is_none());
    }

    #[test]
    fn a_set_record_with_a_zero_length_is_no_record() {
        // Neither surface may end up blank: a board nobody can point at
        // in a scanner list is worse than one with a hex suffix.
        let mut buf = encode_node_name(Some(&name("Balkon")));
        buf[LEN_OFFSET] = 0;
        reseal(&mut buf);
        assert!(decode_node_name(&buf).is_none());
    }

    #[test]
    fn a_set_flag_that_is_neither_is_no_record() {
        let mut buf = encode_node_name(Some(&name("Balkon")));
        buf[SET_OFFSET] = 0x02;
        reseal(&mut buf);
        assert!(decode_node_name(&buf).is_none());
    }

    #[test]
    fn a_name_the_display_surfaces_cannot_carry_is_no_record() {
        // Invalid UTF-8 in an otherwise well-formed record: the GAP name
        // needs a `&str` and the announce is read as text, so there is no
        // honest rendering. Same answer as a torn write.
        let mut buf = encode_node_name(Some(&name("Balkon")));
        buf[NAME_OFFSET] = 0xFF;
        reseal(&mut buf);
        assert!(decode_node_name(&buf).is_none());
    }

    #[test]
    fn a_short_buffer_is_no_record() {
        let buf = encode_node_name(Some(&name("Balkon")));
        assert!(decode_node_name(&buf[..ENCODED_SIZE - 1]).is_none());
    }

    #[test]
    fn the_encoded_size_is_word_aligned_and_fits_the_page_budget() {
        assert_eq!(ENCODED_SIZE, 41);
        assert_eq!(ENCODED_SIZE_ALIGNED, 44);
        // The firmware's page layout gives each record a 0x100 slot; a
        // record that outgrew it would silently overwrite its neighbour.
        // Asserted here as well as by the firmware's compile-time check,
        // because this is where the size is decided.
        const { assert!(ENCODED_SIZE_ALIGNED <= 0x100) };
    }

    #[test]
    fn the_same_name_encodes_to_the_same_bytes() {
        // The read-compare-write save path must not burn a flash cycle
        // on residue: the buffer past `len` is zero-filled, not left
        // holding an earlier, longer name.
        let long = encode_node_name(Some(&name("Balkon-Nord-Solar")));
        let short = encode_node_name(Some(&name("Ab")));
        let short_again = encode_node_name(Some(&name("Ab")));
        assert_eq!(short, short_again);
        assert_ne!(long, short);
        assert!(short[NAME_OFFSET + 2..CHECKSUM_OFFSET]
            .iter()
            .all(|b| *b == 0));
    }

    /// Re-checksum a hand-damaged record, so the test is about the field
    /// it changed rather than about the checksum catching it.
    fn reseal(buf: &mut [u8; ENCODED_SIZE_ALIGNED]) {
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET] = cs[0];
        buf[CHECKSUM_OFFSET + 1] = cs[1];
    }
}
