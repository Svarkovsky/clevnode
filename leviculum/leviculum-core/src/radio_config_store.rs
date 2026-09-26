//! Persistent storage for a node's LoRa radio configuration.
//!
//! A standalone LNode is configured over the serial control channel with a
//! radio-config frame ([`crate::rnode::build_radio_config_frame`]). Without
//! persistence that configuration lives only in RAM, so a reset drops the
//! board back to its compiled default profile and a user's chosen frequency
//! is silently lost.
//!
//! This module defines the *stored* form of that configuration: the same
//! wire payload the host already sends, wrapped in a magic + version +
//! length + checksum envelope so a blank (erased, all-`0xFF`) or corrupt
//! flash page is detected and rejected rather than applied as a radio
//! profile. The wire format itself is untouched — nothing here is visible
//! to a peer, this is local storage only.
//!
//! Layout (27 bytes, padded to 28 for 4-byte flash writes):
//!
//! ```text
//!  0..4   magic "RTRC"
//!  4      format version (0x01)
//!  5      payload length (13..=19, the wire payload's own length)
//!  6..25  wire payload (the radio-config frame minus its 2-byte magic),
//!         zero-padded to the maximum payload length
//! 25..27  checksum over bytes 0..25
//! 27      padding
//! ```

use crate::rnode::{build_radio_config_frame, parse_radio_config, RadioConfigWire};

// Wire format
const MAGIC: [u8; 4] = [0x52, 0x54, 0x52, 0x43]; // "RTRC"
const FORMAT_VERSION: u8 = 0x01;
const HEADER_SIZE: usize = 6; // magic(4) + version(1) + payload_len(1)
/// Largest radio-config wire payload [`parse_radio_config`] accepts.
const MAX_PAYLOAD: usize = 19;
/// Smallest radio-config wire payload [`parse_radio_config`] accepts.
const MIN_PAYLOAD: usize = 13;
/// Payload length that stops just short of the `lt_alock` field, used to
/// store a config the host sent without one (see [`encode_radio_config`]).
const PAYLOAD_WITHOUT_LT_ALOCK: usize = 17;
const CHECKSUM_SIZE: usize = 2;
const CHECKSUM_OFFSET: usize = HEADER_SIZE + MAX_PAYLOAD; // 25

/// Total encoded size: 6 header + 19 payload + 2 checksum = 27 bytes.
pub const ENCODED_SIZE: usize = HEADER_SIZE + MAX_PAYLOAD + CHECKSUM_SIZE;

/// Encoded size rounded up to 4-byte alignment (flash writes are word-wide).
pub const ENCODED_SIZE_ALIGNED: usize = (ENCODED_SIZE + 3) & !3; // 28

/// Same two-byte XOR checksum the identity store uses
/// ([`crate::identity_store`]): even-indexed bytes into `a`, odd into `b`.
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

/// Encode a radio config into a fixed-size buffer for persistent storage.
///
/// `radio_silent` is deliberately **not** persisted (it is stored as
/// `false`): it is a runtime mute used by the test runner to neutralise
/// boards a scenario does not bind, not a radio setting a user chose. A
/// board that persisted it would come back from a reset permanently mute
/// with no visible cause. Every other field round-trips verbatim.
///
/// `lt_alock_present` round-trips through the stored payload *length*, the
/// same way it does on the wire: a config that carried no explicit
/// long-term airtime lock is stored as the 17-byte short payload, so the
/// firmware still derives the ETSI lawful default from its frequency after
/// a reset instead of reading a fabricated explicit `0` (= unlimited).
pub fn encode_radio_config(cfg: &RadioConfigWire) -> [u8; ENCODED_SIZE_ALIGNED] {
    let storable = RadioConfigWire {
        radio_silent: false,
        ..*cfg
    };
    // Reuse the host wire encoder so the stored payload can never drift
    // from the format `parse_radio_config` reads back.
    let frame = build_radio_config_frame(&storable);
    let payload = &frame[2..]; // strip the 2-byte frame magic
    let payload = if storable.lt_alock_present {
        payload
    } else {
        &payload[..PAYLOAD_WITHOUT_LT_ALOCK]
    };

    let mut buf = [0u8; ENCODED_SIZE_ALIGNED];
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4] = FORMAT_VERSION;
    buf[5] = payload.len() as u8;
    buf[HEADER_SIZE..HEADER_SIZE + payload.len()].copy_from_slice(payload);
    let cs = checksum(&buf[..CHECKSUM_OFFSET]);
    buf[CHECKSUM_OFFSET] = cs[0];
    buf[CHECKSUM_OFFSET + 1] = cs[1];
    buf
}

/// Decode a radio config from a persistent storage buffer.
///
/// Returns `None` for a blank page (erased flash reads as `0xFF`), a wrong
/// magic or version, an out-of-range payload length, a checksum mismatch,
/// or a payload the wire parser itself rejects.
pub fn decode_radio_config(buf: &[u8]) -> Option<RadioConfigWire> {
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
    let len = buf[5] as usize;
    if !(MIN_PAYLOAD..=MAX_PAYLOAD).contains(&len) {
        return None;
    }
    let stored = [buf[CHECKSUM_OFFSET], buf[CHECKSUM_OFFSET + 1]];
    if stored != checksum(&buf[..CHECKSUM_OFFSET]) {
        return None;
    }
    parse_radio_config(&buf[HEADER_SIZE..HEADER_SIZE + len])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RadioConfigWire {
        RadioConfigWire {
            frequency_hz: 867_200_000,
            bandwidth_hz: 125_000,
            sf: 10,
            cr: 6,
            tx_power_dbm: 14,
            preamble_len: 32,
            csma_enabled: true,
            radio_silent: false,
            st_alock: 1500,
            lt_alock: 200,
            lt_alock_present: true,
        }
    }

    #[test]
    fn roundtrip() {
        let cfg = sample();
        let buf = encode_radio_config(&cfg);
        assert_eq!(decode_radio_config(&buf), Some(cfg));
    }

    #[test]
    fn blank_page_returns_none() {
        let buf = [0xFF; ENCODED_SIZE_ALIGNED];
        assert!(decode_radio_config(&buf).is_none());
    }

    #[test]
    fn zeroed_page_returns_none() {
        // A page of zeroes is not erased flash but is not ours either.
        let buf = [0x00; ENCODED_SIZE_ALIGNED];
        assert!(decode_radio_config(&buf).is_none());
    }

    #[test]
    fn bit_flip_anywhere_returns_none() {
        // Every byte the checksum covers, flipped one bit at a time.
        for i in 0..CHECKSUM_OFFSET + CHECKSUM_SIZE {
            let mut buf = encode_radio_config(&sample());
            buf[i] ^= 0x01;
            assert!(
                decode_radio_config(&buf).is_none(),
                "bit flip at byte {i} was accepted"
            );
        }
    }

    #[test]
    fn bad_magic_returns_none() {
        let mut buf = encode_radio_config(&sample());
        buf[0] = 0x00;
        assert!(decode_radio_config(&buf).is_none());
    }

    #[test]
    fn wrong_version_returns_none() {
        let mut buf = encode_radio_config(&sample());
        buf[4] = 0x99;
        assert!(decode_radio_config(&buf).is_none());
    }

    #[test]
    fn out_of_range_length_returns_none() {
        for len in [0u8, 12, 20, 255] {
            let mut buf = encode_radio_config(&sample());
            buf[5] = len;
            let cs = checksum(&buf[..CHECKSUM_OFFSET]);
            buf[CHECKSUM_OFFSET] = cs[0];
            buf[CHECKSUM_OFFSET + 1] = cs[1];
            assert!(decode_radio_config(&buf).is_none(), "length {len} accepted");
        }
    }

    #[test]
    fn short_buffer_returns_none() {
        let buf = encode_radio_config(&sample());
        assert!(decode_radio_config(&buf[..ENCODED_SIZE - 1]).is_none());
    }

    #[test]
    fn payload_rejected_by_wire_parser_returns_none() {
        // sf=99 is out of the parser's 5..=12 range: a page that survives
        // magic, version, length and checksum must still not be applied.
        let mut buf = encode_radio_config(&sample());
        buf[HEADER_SIZE + 8] = 99; // sf byte within the wire payload
        let cs = checksum(&buf[..CHECKSUM_OFFSET]);
        buf[CHECKSUM_OFFSET] = cs[0];
        buf[CHECKSUM_OFFSET + 1] = cs[1];
        assert!(decode_radio_config(&buf).is_none());
    }

    #[test]
    fn absent_lt_alock_stays_absent() {
        // A config the host sent without an explicit long-term airtime lock
        // must not come back as an explicit 0 (= unlimited): the firmware
        // would stop deriving the ETSI lawful cap from its frequency.
        let cfg = RadioConfigWire {
            lt_alock_present: false,
            lt_alock: 0,
            ..sample()
        };
        let decoded = decode_radio_config(&encode_radio_config(&cfg)).unwrap();
        assert!(!decoded.lt_alock_present);
        assert_eq!(decoded, cfg);
    }

    #[test]
    fn radio_silent_is_not_persisted() {
        let cfg = RadioConfigWire {
            radio_silent: true,
            ..sample()
        };
        let decoded = decode_radio_config(&encode_radio_config(&cfg)).unwrap();
        assert!(!decoded.radio_silent);
        assert_eq!(decoded.frequency_hz, cfg.frequency_hz);
    }

    #[test]
    fn silent_flag_alone_does_not_change_the_stored_bytes() {
        // The save path compares encoded bytes; a mute toggle must not
        // count as a change and burn a flash cycle.
        let cfg = sample();
        let muted = RadioConfigWire {
            radio_silent: true,
            ..cfg
        };
        assert_eq!(encode_radio_config(&cfg), encode_radio_config(&muted));
    }

    #[test]
    fn encoded_size_is_correct() {
        assert_eq!(ENCODED_SIZE, 27);
        assert_eq!(ENCODED_SIZE_ALIGNED, 28);
    }
}
