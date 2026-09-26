//! Persistent storage for a node's telemetry target (Codeberg #236).
//!
//! A host sets the telemetry target over the #238 control envelope
//! ([`crate::envelope::TYPE_TELEMETRY_TARGET`]). Without persistence the
//! target lives only in RAM, so a reset switches telemetry off silently —
//! and since the configured target *is* the on-switch, that is not a lost
//! preference but a lost feature.
//!
//! This module defines the *stored* form of that target: the same payload
//! the host already sends, wrapped in the magic + version + checksum
//! envelope [`crate::radio_config_store`] established, so a blank (erased,
//! all-`0xFF`) or corrupt flash page decodes to "no target" — telemetry
//! off, the default — rather than to a garbage destination the node would
//! then try to encrypt for. Nothing here is visible to a peer; this is
//! local storage only.
//!
//! Layout (89 bytes, padded to 92 for 4-byte flash writes):
//!
//! ```text
//!  0..4   magic "LTTG"
//!  4      format version (0x01)
//!  5      profile id (the wire ids of `TELEMETRY_PROFILE_*`)
//!  6..22  destination hash
//! 22      key-present flag (0x00 / 0x01), never inferred from a length
//! 23..87  public key, zero-filled when absent
//! 87..89  checksum over bytes 0..87
//! 89..92  padding
//! ```
//!
//! The public key is stored when the host supplied one, so a board that
//! was configured with a key does not re-resolve it over the air on every
//! boot. A hash-only target — the common case per the 2026-08-22 UX
//! decision — stores the flag as absent and comes back up in
//! `awaiting-key`, which is the honest state for it.

use crate::constants::{IDENTITY_KEY_SIZE, TRUNCATED_HASHBYTES};
use crate::envelope::TelemetryTargetWire;

const MAGIC: [u8; 4] = [0x4C, 0x54, 0x54, 0x47]; // "LTTG"
const FORMAT_VERSION: u8 = 0x01;
const PROFILE_OFFSET: usize = 5;
const HASH_OFFSET: usize = 6;
const KEY_FLAG_OFFSET: usize = HASH_OFFSET + TRUNCATED_HASHBYTES; // 22
const KEY_OFFSET: usize = KEY_FLAG_OFFSET + 1; // 23
const CHECKSUM_OFFSET: usize = KEY_OFFSET + IDENTITY_KEY_SIZE; // 87
const CHECKSUM_SIZE: usize = 2;

/// Total encoded size: 87 header+payload + 2 checksum = 89 bytes.
pub const ENCODED_SIZE: usize = CHECKSUM_OFFSET + CHECKSUM_SIZE;

/// Encoded size rounded up to 4-byte alignment (flash writes are
/// word-wide).
pub const ENCODED_SIZE_ALIGNED: usize = ENCODED_SIZE.div_ceil(4) * 4; // 92

/// Same two-byte XOR checksum the identity and radio-config stores use:
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

/// Encode a telemetry target into a fixed-size buffer for persistent
/// storage.
pub fn encode_telemetry_target(target: &TelemetryTargetWire) -> [u8; ENCODED_SIZE_ALIGNED] {
    let mut buf = [0u8; ENCODED_SIZE_ALIGNED];
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4] = FORMAT_VERSION;
    buf[PROFILE_OFFSET] = target.profile;
    buf[HASH_OFFSET..KEY_FLAG_OFFSET].copy_from_slice(&target.dest_hash);
    if let Some(key) = &target.public_key {
        buf[KEY_FLAG_OFFSET] = 0x01;
        buf[KEY_OFFSET..CHECKSUM_OFFSET].copy_from_slice(key);
    }
    let cs = checksum(&buf[..CHECKSUM_OFFSET]);
    buf[CHECKSUM_OFFSET] = cs[0];
    buf[CHECKSUM_OFFSET + 1] = cs[1];
    buf
}

/// Decode a telemetry target from a persistent storage buffer.
///
/// Returns `None` for a blank page (erased flash reads as `0xFF`), a wrong
/// magic or version, a key-present flag that is neither 0 nor 1, or a
/// checksum mismatch. Every one of those means "no target", which is the
/// same state a board that was never configured is in.
///
/// The profile id is *not* validated here: an id this firmware does not
/// know is a target set by a newer host, and the policy layer decides what
/// to do with it (fall back to the default profile) rather than the store
/// deciding to forget the destination.
pub fn decode_telemetry_target(buf: &[u8]) -> Option<TelemetryTargetWire> {
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
    let mut dest_hash = [0u8; TRUNCATED_HASHBYTES];
    dest_hash.copy_from_slice(&buf[HASH_OFFSET..KEY_FLAG_OFFSET]);
    let public_key = match buf[KEY_FLAG_OFFSET] {
        0x00 => None,
        0x01 => {
            let mut key = [0u8; IDENTITY_KEY_SIZE];
            key.copy_from_slice(&buf[KEY_OFFSET..CHECKSUM_OFFSET]);
            Some(key)
        }
        _ => return None,
    };
    Some(TelemetryTargetWire {
        profile: buf[PROFILE_OFFSET],
        dest_hash,
        public_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{TELEMETRY_PROFILE_STATION, TELEMETRY_PROFILE_TRACKER};

    fn hash_only() -> TelemetryTargetWire {
        TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_STATION,
            dest_hash: [0xA7; TRUNCATED_HASHBYTES],
            public_key: None,
        }
    }

    fn with_key() -> TelemetryTargetWire {
        TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_TRACKER,
            dest_hash: [0x3C; TRUNCATED_HASHBYTES],
            public_key: Some([0x5E; IDENTITY_KEY_SIZE]),
        }
    }

    #[test]
    fn a_hash_only_target_round_trips() {
        let t = hash_only();
        assert_eq!(
            decode_telemetry_target(&encode_telemetry_target(&t)),
            Some(t)
        );
    }

    #[test]
    fn a_target_with_a_key_round_trips() {
        let t = with_key();
        assert_eq!(
            decode_telemetry_target(&encode_telemetry_target(&t)),
            Some(t)
        );
    }

    #[test]
    fn a_blank_page_is_no_target() {
        assert!(decode_telemetry_target(&[0xFF; ENCODED_SIZE_ALIGNED]).is_none());
    }

    #[test]
    fn a_zeroed_page_is_no_target() {
        // Not erased flash, but not ours either.
        assert!(decode_telemetry_target(&[0x00; ENCODED_SIZE_ALIGNED]).is_none());
    }

    #[test]
    fn a_bit_flip_anywhere_is_no_target() {
        for i in 0..ENCODED_SIZE {
            let mut buf = encode_telemetry_target(&with_key());
            buf[i] ^= 0x01;
            assert!(
                decode_telemetry_target(&buf).is_none(),
                "bit flip at byte {i} was accepted"
            );
        }
    }

    #[test]
    fn a_wrong_magic_or_version_is_no_target() {
        let mut buf = encode_telemetry_target(&hash_only());
        buf[0] = 0x00;
        assert!(decode_telemetry_target(&buf).is_none());

        let mut buf = encode_telemetry_target(&hash_only());
        buf[4] = 0x99;
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET] = cs[0];
        buf[CHECKSUM_OFFSET + 1] = cs[1];
        assert!(decode_telemetry_target(&buf).is_none());
    }

    #[test]
    fn a_key_flag_that_is_neither_present_nor_absent_is_no_target() {
        // The wire format refuses to guess key presence from a length;
        // the stored form refuses the same way.
        let mut buf = encode_telemetry_target(&hash_only());
        buf[KEY_FLAG_OFFSET] = 0x02;
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET] = cs[0];
        buf[CHECKSUM_OFFSET + 1] = cs[1];
        assert!(decode_telemetry_target(&buf).is_none());
    }

    #[test]
    fn a_short_buffer_is_no_target() {
        let buf = encode_telemetry_target(&hash_only());
        assert!(decode_telemetry_target(&buf[..ENCODED_SIZE - 1]).is_none());
    }

    #[test]
    fn an_unknown_profile_id_keeps_its_destination() {
        // A newer host set a profile this firmware does not know. Losing
        // the destination over it would switch telemetry off; the policy
        // layer falls back to the default cadence instead.
        let t = TelemetryTargetWire {
            profile: 0x7F,
            ..hash_only()
        };
        assert_eq!(
            decode_telemetry_target(&encode_telemetry_target(&t)),
            Some(t)
        );
    }

    #[test]
    fn a_stored_key_does_not_leak_into_a_hash_only_record() {
        // The key bytes are zero-filled when absent, so two records that
        // differ only in an earlier tenant's key compare equal and the
        // read-compare-write save path does not burn a flash cycle.
        let a = encode_telemetry_target(&hash_only());
        let b = encode_telemetry_target(&hash_only());
        assert_eq!(a, b);
        assert!(a[KEY_OFFSET..CHECKSUM_OFFSET].iter().all(|&b| b == 0));
    }

    #[test]
    fn the_encoded_size_is_word_aligned() {
        assert_eq!(ENCODED_SIZE, 89);
        assert_eq!(ENCODED_SIZE_ALIGNED, 92);
    }
}
