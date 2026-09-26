//! Persistent storage for a node's user-set fixed position.
//!
//! A host sets the fixed position over the #238 control envelope
//! ([`crate::envelope::TYPE_FIXED_POSITION`]). Without persistence it
//! lives only in RAM, and a reset would silently return the node to
//! sensor reporting — for a GNSS-less board that is a pin that vanishes.
//!
//! Stored form: the same magic + version + checksum envelope the
//! telemetry-target store uses, so a blank (erased, all-`0xFF`) or
//! corrupt region decodes to "no fixed position" — sensor reporting, the
//! default. The record shares the telemetry flash page with the target
//! (the firmware names the layout in `leviculum_nrf::telemetry`); this
//! module only fixes the record's bytes.
//!
//! Layout (21 bytes, padded to 24 for 4-byte flash writes):
//!
//! ```text
//!  0..4   magic "LFPO"
//!  4      format version (0x01)
//!  5      set flag (0x00 = cleared, 0x01 = set), never inferred
//!  6..10  latitude  × 1e6, i32 BE
//! 10..14  longitude × 1e6, i32 BE
//! 14      altitude-present flag (0x00 / 0x01)
//! 15..19  altitude × 1e2, i32 BE, zero-filled when absent
//! 19..21  checksum over bytes 0..19
//! 21..24  padding
//! ```
//!
//! An explicitly cleared position is stored as a valid record with the
//! set flag absent rather than as an erased region: the store task always
//! rewrites the whole record, and "cleared by the user" and "never
//! configured" mean the same thing on the next boot — sensor reporting.

use crate::envelope::{FixedPositionWire, FIXED_POSITION_MAX_LAT_E6, FIXED_POSITION_MAX_LON_E6};

const MAGIC: [u8; 4] = [0x4C, 0x46, 0x50, 0x4F]; // "LFPO"
const FORMAT_VERSION: u8 = 0x01;
const SET_OFFSET: usize = 5;
const LAT_OFFSET: usize = 6;
const LON_OFFSET: usize = 10;
const ALT_FLAG_OFFSET: usize = 14;
const ALT_OFFSET: usize = 15;
const CHECKSUM_OFFSET: usize = ALT_OFFSET + 4; // 19
const CHECKSUM_SIZE: usize = 2;

/// Total encoded size: 19 header+payload + 2 checksum = 21 bytes.
pub const ENCODED_SIZE: usize = CHECKSUM_OFFSET + CHECKSUM_SIZE;

/// Encoded size rounded up to 4-byte alignment (flash writes are
/// word-wide).
pub const ENCODED_SIZE_ALIGNED: usize = ENCODED_SIZE.div_ceil(4) * 4; // 24

/// Same two-byte XOR checksum the identity, radio-config and
/// telemetry-target stores use: even-indexed bytes into `a`, odd into `b`.
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

/// Encode a fixed position — or the explicit clear (`None`) — into a
/// fixed-size buffer for persistent storage.
pub fn encode_fixed_position(position: Option<&FixedPositionWire>) -> [u8; ENCODED_SIZE_ALIGNED] {
    let mut buf = [0u8; ENCODED_SIZE_ALIGNED];
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4] = FORMAT_VERSION;
    if let Some(pos) = position {
        buf[SET_OFFSET] = 0x01;
        buf[LAT_OFFSET..LON_OFFSET].copy_from_slice(&pos.latitude_e6.to_be_bytes());
        buf[LON_OFFSET..ALT_FLAG_OFFSET].copy_from_slice(&pos.longitude_e6.to_be_bytes());
        if let Some(alt) = pos.altitude_e2 {
            buf[ALT_FLAG_OFFSET] = 0x01;
            buf[ALT_OFFSET..CHECKSUM_OFFSET].copy_from_slice(&alt.to_be_bytes());
        }
    }
    let cs = checksum(&buf[..CHECKSUM_OFFSET]);
    buf[CHECKSUM_OFFSET] = cs[0];
    buf[CHECKSUM_OFFSET + 1] = cs[1];
    buf
}

/// Decode a fixed position from a persistent storage buffer.
///
/// Returns `None` for a blank region (erased flash reads as `0xFF`), a
/// wrong magic or version, a flag byte that is neither 0 nor 1, a
/// checksum mismatch, a coordinate outside the globe — and for a valid
/// record whose set flag says cleared. Every one of those means "no fixed
/// position", which is sensor reporting, the default.
pub fn decode_fixed_position(buf: &[u8]) -> Option<FixedPositionWire> {
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
    let latitude_e6 = i32::from_be_bytes(buf[LAT_OFFSET..LON_OFFSET].try_into().ok()?);
    let longitude_e6 = i32::from_be_bytes(buf[LON_OFFSET..ALT_FLAG_OFFSET].try_into().ok()?);
    if latitude_e6.unsigned_abs() > FIXED_POSITION_MAX_LAT_E6 as u32
        || longitude_e6.unsigned_abs() > FIXED_POSITION_MAX_LON_E6 as u32
    {
        return None;
    }
    let altitude_e2 = match buf[ALT_FLAG_OFFSET] {
        0x00 => None,
        0x01 => Some(i32::from_be_bytes(
            buf[ALT_OFFSET..CHECKSUM_OFFSET].try_into().ok()?,
        )),
        _ => return None,
    };
    Some(FixedPositionWire {
        latitude_e6,
        longitude_e6,
        altitude_e2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_alt() -> FixedPositionWire {
        FixedPositionWire {
            latitude_e6: 52_520_008,
            longitude_e6: 13_404_954,
            altitude_e2: Some(3_400),
        }
    }

    fn without_alt() -> FixedPositionWire {
        FixedPositionWire {
            latitude_e6: -36_848_460,
            longitude_e6: -73_044_440,
            altitude_e2: None,
        }
    }

    #[test]
    fn a_position_round_trips_with_and_without_an_altitude() {
        for pos in [with_alt(), without_alt()] {
            assert_eq!(
                decode_fixed_position(&encode_fixed_position(Some(&pos))),
                Some(pos)
            );
        }
    }

    #[test]
    fn an_explicit_clear_decodes_to_no_position() {
        // Cleared-by-the-user and never-configured are the same state on
        // the next boot: sensor reporting.
        assert_eq!(decode_fixed_position(&encode_fixed_position(None)), None);
    }

    #[test]
    fn a_blank_region_is_no_position() {
        assert!(decode_fixed_position(&[0xFF; ENCODED_SIZE_ALIGNED]).is_none());
    }

    #[test]
    fn a_zeroed_region_is_no_position() {
        // Not erased flash, but not ours either.
        assert!(decode_fixed_position(&[0x00; ENCODED_SIZE_ALIGNED]).is_none());
    }

    #[test]
    fn a_bit_flip_anywhere_is_no_position() {
        for i in 0..ENCODED_SIZE {
            let mut buf = encode_fixed_position(Some(&with_alt()));
            buf[i] ^= 0x01;
            assert!(
                decode_fixed_position(&buf).is_none(),
                "bit flip at byte {i} was accepted"
            );
        }
    }

    #[test]
    fn a_wrong_magic_or_version_is_no_position() {
        let mut buf = encode_fixed_position(Some(&with_alt()));
        buf[0] = 0x00;
        assert!(decode_fixed_position(&buf).is_none());

        let mut buf = encode_fixed_position(Some(&with_alt()));
        buf[4] = 0x99;
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET] = cs[0];
        buf[CHECKSUM_OFFSET + 1] = cs[1];
        assert!(decode_fixed_position(&buf).is_none());
    }

    #[test]
    fn a_coordinate_outside_the_globe_is_no_position() {
        // A record can only get here through a firmware defect or flash
        // damage the checksum missed; either way it must not become a pin
        // in the Barents Sea at lat 91.
        let mut buf = encode_fixed_position(Some(&with_alt()));
        buf[LAT_OFFSET..LON_OFFSET].copy_from_slice(&91_000_000i32.to_be_bytes());
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET] = cs[0];
        buf[CHECKSUM_OFFSET + 1] = cs[1];
        assert!(decode_fixed_position(&buf).is_none());
    }

    #[test]
    fn a_short_buffer_is_no_position() {
        let buf = encode_fixed_position(Some(&with_alt()));
        assert!(decode_fixed_position(&buf[..ENCODED_SIZE - 1]).is_none());
    }

    #[test]
    fn an_absent_altitude_is_zero_filled_so_records_compare_equal() {
        // Same rule as the target store's key bytes: the read-compare-write
        // save path must not burn a flash cycle on residue.
        let a = encode_fixed_position(Some(&without_alt()));
        let b = encode_fixed_position(Some(&without_alt()));
        assert_eq!(a, b);
        assert!(a[ALT_OFFSET..CHECKSUM_OFFSET].iter().all(|&b| b == 0));
    }

    #[test]
    fn the_encoded_size_is_word_aligned() {
        assert_eq!(ENCODED_SIZE, 21);
        assert_eq!(ENCODED_SIZE_ALIGNED, 24);
    }
}
