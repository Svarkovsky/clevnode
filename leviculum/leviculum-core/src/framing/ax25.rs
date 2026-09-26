//! AX.25 UI-frame addressing over KISS (AX25KISSInterface).
//!
//! Python Reticulum's `AX25KISSInterface` layers AX.25 addressing on top of the
//! same KISS framing the plain `KISSInterface` uses: each outgoing packet is
//! prefixed with a 16-byte AX.25 UI-frame header (dest address, source address,
//! control, PID) and the whole thing is then KISS-framed as a `CMD_DATA` frame.
//! On receive the 16-byte header is stripped back off before the payload is
//! handed up.
//!
//! ```text
//! [ dst callsign x6 | dst SSID ] [ src callsign x6 | src SSID ] [ CTRL ] [ PID ]  payload...
//!  \___________________________ 14-byte address field ________________________/  \__ ctrl+pid __/
//!  \________________________________ 16-byte AX.25 header (HEADER_SIZE) ________________________/
//! ```
//!
//! Only the AX.25 header (this module) is new; the KISS frame/deframe underneath
//! is [`crate::framing::kiss`], shared with the `KISSInterface`.
//!
//! # Byte-for-byte parity with Python
//!
//! The encoding mirrors `AX25KISSInterface.process_outgoing` exactly
//! (`AX25KISSInterface.py:270-289`):
//!
//! - Each callsign character is shifted left one bit (`c << 1`).
//! - A callsign shorter than 6 characters is padded to 6 with the raw byte
//!   `0x20` (Python uses `bytes([0x20])`, *not* a shifted space) — matched here
//!   for wire compatibility even though textbook AX.25 pads with `0x40`.
//! - The SSID byte is `0x60 | (ssid << 1)`, with bit 0 (`0x01`, the HDLC
//!   end-of-address bit) additionally set on the *source* (last) address.
//! - Control is `0x03` (UI), PID is `0xF0` (no layer 3).
//!
//! # no_std Support
//!
//! Requires `alloc` (header construction allocates a `Vec`). [`strip_header`]
//! is allocation-free.

use alloc::vec::Vec;

/// Full AX.25 UI-frame header length: 14-byte address field + control + PID.
///
/// Matches Python `AX25.HEADER_SIZE` (`AX25KISSInterface.py:64`).
pub const HEADER_SIZE: usize = 16;

/// AX.25 control byte for an Unnumbered Information (UI) frame.
pub const CTRL_UI: u8 = 0x03;

/// AX.25 PID for "no layer 3 protocol".
pub const PID_NOLAYER3: u8 = 0xF0;

/// Padding byte for callsigns shorter than six characters.
///
/// Python pads with the raw byte `0x20` (`AX25KISSInterface.py:279,286`), not a
/// shifted space. We reproduce that byte-for-byte for wire compatibility.
pub const ADDRESS_PAD: u8 = 0x20;

/// Default destination callsign Python uses for all outgoing frames
/// (`AX25KISSInterface.py:118`). Reticulum does not route on AX.25 addresses; it
/// is a fixed APRS-style tocall.
pub const DEFAULT_DST_CALLSIGN: &[u8] = b"APZRNS";

/// Default destination SSID (`AX25KISSInterface.py:119`).
pub const DEFAULT_DST_SSID: u8 = 0;

/// Minimum / maximum callsign length Python accepts
/// (`AX25KISSInterface.py:135`).
pub const MIN_CALLSIGN_LEN: usize = 3;
pub const MAX_CALLSIGN_LEN: usize = 6;

/// Maximum SSID value (`AX25KISSInterface.py:138`).
pub const MAX_SSID: u8 = 15;

/// Why an AX.25 addressing configuration was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ax25Error {
    /// Callsign is shorter than 3 or longer than 6 characters.
    CallsignLength,
    /// SSID is outside the 0..=15 range.
    SsidRange,
}

/// Append the 7-byte AX.25 encoding of one address (6 callsign bytes + SSID) to
/// `out`.
///
/// `last` sets the HDLC end-of-address bit (`0x01`) on the SSID byte, which
/// Python sets on the source (final) address only.
fn encode_address(callsign: &[u8], ssid: u8, last: bool, out: &mut Vec<u8>) {
    for i in 0..MAX_CALLSIGN_LEN {
        if i < callsign.len() {
            out.push(callsign[i] << 1);
        } else {
            out.push(ADDRESS_PAD);
        }
    }
    let ext = if last { 0x01 } else { 0x00 };
    out.push(0x60 | (ssid << 1) | ext);
}

/// Build the 16-byte AX.25 UI-frame header (destination address, source
/// address, control, PID).
///
/// Byte-for-byte identical to the `addr + CTRL_UI + PID_NOLAYER3` prefix Python
/// prepends in `process_outgoing` (`AX25KISSInterface.py:270-289`). Callsigns
/// are taken verbatim; the caller uppercases to match Python's
/// `callsign.upper()`.
pub fn encode_header(dst_call: &[u8], dst_ssid: u8, src_call: &[u8], src_ssid: u8) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_SIZE);
    encode_address(dst_call, dst_ssid, false, &mut header);
    encode_address(src_call, src_ssid, true, &mut header);
    header.push(CTRL_UI);
    header.push(PID_NOLAYER3);
    header
}

/// Strip the 16-byte AX.25 header off a received UI frame, returning the
/// payload.
///
/// Returns `None` if the frame is not strictly longer than the header, matching
/// Python `process_incoming`'s `if (len(data) > AX25.HEADER_SIZE)` guard
/// (`AX25KISSInterface.py:257`): a frame of exactly the header size carries no
/// payload and is dropped. The header contents are not validated — Reticulum
/// does not route on AX.25 addresses, so the header is opaque framing.
pub fn strip_header(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() > HEADER_SIZE {
        Some(&frame[HEADER_SIZE..])
    } else {
        None
    }
}

/// A validated AX.25 addressing configuration for one interface.
///
/// Holds the source callsign/SSID (the destination is Reticulum's fixed
/// `APZRNS-0` tocall) and precomputes the constant 16-byte UI-frame header so
/// the TX path is a single prepend per packet.
#[derive(Debug, Clone)]
pub struct Ax25Addressing {
    header: Vec<u8>,
}

impl Ax25Addressing {
    /// Validate the source callsign/SSID (Python `__init__`,
    /// `AX25KISSInterface.py:135-139`) and precompute the UI-frame header
    /// against the fixed `APZRNS-0` destination.
    ///
    /// `src_call` must already be uppercased ASCII by the caller (Python does
    /// `callsign.upper().encode("ascii")`).
    pub fn new(src_call: &[u8], src_ssid: u8) -> Result<Self, Ax25Error> {
        if src_call.len() < MIN_CALLSIGN_LEN || src_call.len() > MAX_CALLSIGN_LEN {
            return Err(Ax25Error::CallsignLength);
        }
        if src_ssid > MAX_SSID {
            return Err(Ax25Error::SsidRange);
        }
        let header = encode_header(DEFAULT_DST_CALLSIGN, DEFAULT_DST_SSID, src_call, src_ssid);
        Ok(Self { header })
    }

    /// The precomputed 16-byte UI-frame header.
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// Prepend the AX.25 UI header to `payload`, yielding the AX.25 frame that
    /// is then KISS-framed. Mirrors `data = addr+ctrl+pid+data` in Python
    /// `process_outgoing`.
    pub fn wrap(&self, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(HEADER_SIZE + payload.len());
        frame.extend_from_slice(&self.header);
        frame.extend_from_slice(payload);
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    // Known-answer test: the exact 16 header bytes Python's process_outgoing
    // emits for a given callsign/SSID, computed by hand from the reference
    // encoding rules (AX25KISSInterface.py:270-289).
    #[test]
    fn header_kat_n0call_ssid0() {
        // src = "N0CALL", ssid 0.
        let header = encode_header(DEFAULT_DST_CALLSIGN, DEFAULT_DST_SSID, b"N0CALL", 0);

        // Destination "APZRNS" shifted << 1: A=0x41->0x82, P=0x50->0xA0,
        // Z=0x5A->0xB4, R=0x52->0xA4, N=0x4E->0x9C, S=0x53->0xA6.
        // Dest SSID byte: 0x60 | (0<<1) = 0x60 (end-of-address bit clear).
        // Source "N0CALL" shifted: N=0x4E->0x9C, 0=0x30->0x60, C=0x43->0x86,
        // A=0x41->0x82, L=0x4C->0x98, L=0x4C->0x98.
        // Source SSID byte: 0x60 | (0<<1) | 0x01 = 0x61 (end-of-address set).
        // Control 0x03 (UI), PID 0xF0.
        assert_eq!(
            header,
            vec![
                0x82, 0xA0, 0xB4, 0xA4, 0x9C, 0xA6, 0x60, // dst APZRNS-0
                0x9C, 0x60, 0x86, 0x82, 0x98, 0x98, 0x61, // src N0CALL-0
                0x03, 0xF0, // ctrl UI, PID no-layer-3
            ]
        );
        assert_eq!(header.len(), HEADER_SIZE);
    }

    // SSID is shifted << 1 into the SSID byte; the end-of-address bit stays set
    // on the source address.
    #[test]
    fn header_kat_ssid_encoding() {
        let header = encode_header(DEFAULT_DST_CALLSIGN, 0, b"N0CALL", 7);
        // Source SSID byte: 0x60 | (7<<1) | 0x01 = 0x60 | 0x0E | 0x01 = 0x6F.
        assert_eq!(header[13], 0x6F);
        // Max SSID 15: 0x60 | (15<<1) | 0x01 = 0x60 | 0x1E | 0x01 = 0x7F.
        let header = encode_header(DEFAULT_DST_CALLSIGN, 0, b"N0CALL", 15);
        assert_eq!(header[13], 0x7F);
    }

    // A callsign shorter than 6 chars is right-padded with raw 0x20 (Python's
    // literal, not a shifted space).
    #[test]
    fn header_kat_short_callsign_padding() {
        let header = encode_header(DEFAULT_DST_CALLSIGN, 0, b"ABC", 0);
        // A=0x41->0x82, B=0x42->0x84, C=0x43->0x86, then three pad bytes 0x20.
        assert_eq!(&header[7..14], &[0x82, 0x84, 0x86, 0x20, 0x20, 0x20, 0x61]);
    }

    #[test]
    fn strip_roundtrips_wrap() {
        let addr = Ax25Addressing::new(b"N0CALL", 0).expect("valid addressing");
        let payload = b"hello reticulum";
        let frame = addr.wrap(payload);
        assert_eq!(&frame[..HEADER_SIZE], addr.header());
        assert_eq!(strip_header(&frame), Some(&payload[..]));
    }

    #[test]
    fn strip_rejects_header_only_or_short() {
        // Exactly header-sized carries no payload -> dropped (Python `>`).
        let exactly_header = vec![0u8; HEADER_SIZE];
        assert_eq!(strip_header(&exactly_header), None);
        let too_short = vec![0u8; HEADER_SIZE - 1];
        assert_eq!(strip_header(&too_short), None);
        // One payload byte survives.
        let mut with_payload = vec![0u8; HEADER_SIZE];
        with_payload.push(0x42);
        assert_eq!(strip_header(&with_payload), Some(&[0x42u8][..]));
    }

    #[test]
    fn validation_rejects_bad_callsign_and_ssid() {
        assert_eq!(
            Ax25Addressing::new(b"AB", 0).err(),
            Some(Ax25Error::CallsignLength)
        );
        assert_eq!(
            Ax25Addressing::new(b"TOOLONGX", 0).err(),
            Some(Ax25Error::CallsignLength)
        );
        assert_eq!(
            Ax25Addressing::new(b"N0CALL", 16).err(),
            Some(Ax25Error::SsidRange)
        );
        // Boundaries accepted.
        assert!(Ax25Addressing::new(b"ABC", 0).is_ok());
        assert!(Ax25Addressing::new(b"ABCDEF", 15).is_ok());
    }
}
