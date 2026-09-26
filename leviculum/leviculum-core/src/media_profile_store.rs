//! Persistent storage for a node's media profile — which carriers it
//! meshes over.
//!
//! A host sets the profile over the #238 control envelope
//! ([`crate::envelope::TYPE_MEDIA_PROFILE`]). Without persistence it would
//! live only in RAM, and the reset that ends a measurement run would
//! quietly put the node back on both carriers — which is precisely the
//! state the profile exists to leave: a node meshing over LoRa and BLE at
//! once masks a loss on either, so a single-medium measurement against it
//! is falsifiable.
//!
//! Stored form: the same magic + version + checksum envelope the
//! telemetry-target and fixed-position stores use, so a blank (erased,
//! all-`0xFF`) or corrupt region decodes to `None`. The record shares the
//! telemetry flash page with those two (the firmware names the layout in
//! `leviculum_nrf::telemetry`); this module only fixes the record's bytes.
//!
//! Layout (8 bytes, already 4-byte aligned for flash writes):
//!
//! ```text
//!  0..4   magic "LMED"
//!  4      format version (0x01)
//!  5      media flags (bit0 lora, bit1 ble; `MediaProfileWire::flags`)
//!  6..8   checksum over bytes 0..6
//! ```
//!
//! **`None` is not "both carriers off".** It is "no record", and the
//! caller's default for that is both carriers on — today's behaviour, and
//! the one reading under which a firmware update to a fielded board
//! changes nothing. Encoding that default explicitly is still a valid
//! record: a user who sets both-on has said something, and the boot banner
//! reports `src=flash` for it rather than `src=default`.

use crate::envelope::MediaProfileWire;

const MAGIC: [u8; 4] = [0x4C, 0x4D, 0x45, 0x44]; // "LMED"
const FORMAT_VERSION: u8 = 0x01;
const FLAGS_OFFSET: usize = 5;
const CHECKSUM_OFFSET: usize = 6;
const CHECKSUM_SIZE: usize = 2;

/// Total encoded size: 6 header+payload + 2 checksum = 8 bytes.
pub const ENCODED_SIZE: usize = CHECKSUM_OFFSET + CHECKSUM_SIZE;

/// Encoded size rounded up to 4-byte alignment (flash writes are
/// word-wide). Already aligned; the constant exists so the firmware's
/// buffers are declared the same way for all three records on the page.
pub const ENCODED_SIZE_ALIGNED: usize = ENCODED_SIZE.div_ceil(4) * 4; // 8

/// Same two-byte XOR checksum the identity, radio-config, telemetry-target
/// and fixed-position stores use: even-indexed bytes into `a`, odd into `b`.
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

/// Encode a media profile into a fixed-size buffer for persistent storage.
pub fn encode_media_profile(profile: &MediaProfileWire) -> [u8; ENCODED_SIZE_ALIGNED] {
    let mut buf = [0u8; ENCODED_SIZE_ALIGNED];
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4] = FORMAT_VERSION;
    buf[FLAGS_OFFSET] = profile.flags();
    let cs = checksum(&buf[..CHECKSUM_OFFSET]);
    buf[CHECKSUM_OFFSET] = cs[0];
    buf[CHECKSUM_OFFSET + 1] = cs[1];
    buf
}

/// Decode a media profile from a persistent storage buffer.
///
/// Returns `None` for a blank region (erased flash reads as `0xFF`), a
/// wrong magic or version, a checksum mismatch, or a flag byte carrying a
/// carrier bit this firmware does not know. Every one of those means "no
/// usable record", and the caller's answer to that is
/// [`MediaProfileWire::BOTH`] — never a guess at what the damaged byte
/// might have meant.
pub fn decode_media_profile(buf: &[u8]) -> Option<MediaProfileWire> {
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
    MediaProfileWire::from_flags(buf[FLAGS_OFFSET])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lora_only() -> MediaProfileWire {
        MediaProfileWire {
            lora_enabled: true,
            ble_enabled: false,
        }
    }

    #[test]
    fn every_profile_round_trips() {
        for lora in [false, true] {
            for ble in [false, true] {
                let profile = MediaProfileWire {
                    lora_enabled: lora,
                    ble_enabled: ble,
                };
                assert_eq!(
                    decode_media_profile(&encode_media_profile(&profile)),
                    Some(profile),
                    "lora={lora} ble={ble}"
                );
            }
        }
    }

    #[test]
    fn both_off_is_a_record_and_not_an_absence() {
        // The one profile that looks like "nothing stored" if the encoding
        // were the flags alone: a node deliberately taken off both
        // carriers must come back that way, not on both.
        let silent = MediaProfileWire {
            lora_enabled: false,
            ble_enabled: false,
        };
        assert_eq!(
            decode_media_profile(&encode_media_profile(&silent)),
            Some(silent)
        );
    }

    #[test]
    fn a_blank_region_is_no_record() {
        // And "no record" is both-on at the caller — the property that
        // makes this feature invisible to a board that never sets it.
        assert!(decode_media_profile(&[0xFF; ENCODED_SIZE_ALIGNED]).is_none());
    }

    #[test]
    fn a_zeroed_region_is_no_record() {
        // Not erased flash, but not ours either. Read as a profile it
        // would be "both carriers off" — a silent way to take a board off
        // the mesh, which is exactly why the magic is checked first.
        assert!(decode_media_profile(&[0x00; ENCODED_SIZE_ALIGNED]).is_none());
    }

    #[test]
    fn a_bit_flip_anywhere_is_no_record() {
        for i in 0..ENCODED_SIZE {
            let mut buf = encode_media_profile(&lora_only());
            buf[i] ^= 0x01;
            assert!(
                decode_media_profile(&buf).is_none(),
                "bit flip at byte {i} was accepted"
            );
        }
    }

    #[test]
    fn a_wrong_magic_or_version_is_no_record() {
        let mut buf = encode_media_profile(&lora_only());
        buf[0] = 0x00;
        assert!(decode_media_profile(&buf).is_none());

        let mut buf = encode_media_profile(&lora_only());
        buf[4] = 0x99;
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET] = cs[0];
        buf[CHECKSUM_OFFSET + 1] = cs[1];
        assert!(decode_media_profile(&buf).is_none());
    }

    #[test]
    fn an_unknown_carrier_bit_is_no_record() {
        // A record written by firmware that knows a third carrier. Reading
        // it as "that carrier is off" would be an invented answer; the
        // caller's both-on default is the honest one.
        let mut buf = encode_media_profile(&lora_only());
        buf[FLAGS_OFFSET] |= 0b1000_0000;
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET] = cs[0];
        buf[CHECKSUM_OFFSET + 1] = cs[1];
        assert!(decode_media_profile(&buf).is_none());
    }

    #[test]
    fn a_short_buffer_is_no_record() {
        let buf = encode_media_profile(&lora_only());
        assert!(decode_media_profile(&buf[..ENCODED_SIZE - 1]).is_none());
    }

    #[test]
    fn the_encoded_size_is_word_aligned() {
        assert_eq!(ENCODED_SIZE, 8);
        assert_eq!(ENCODED_SIZE_ALIGNED, 8);
    }

    #[test]
    fn the_same_profile_encodes_to_the_same_bytes() {
        // The read-compare-write save path must not burn a flash cycle on
        // residue; the record has no optional field, so this only has to
        // stay true of the padding.
        assert_eq!(
            encode_media_profile(&lora_only()),
            encode_media_profile(&lora_only())
        );
    }
}
