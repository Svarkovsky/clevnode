//! KISS framing for serial interfaces (RNode, KISSInterface)
//!
//! KISS (Keep It Simple, Stupid) framing uses `0xC0` (FEND) delimiters and
//! `0xDB` (FESC) escape sequences. Unlike HDLC, KISS frames carry a command
//! byte immediately after the opening FEND.
//!
//! ```text
//! [FEND 0xC0] [command] [escaped payload] [FEND 0xC0]
//! ```
//!
//! The command byte is stored raw, no port nibble masking is applied. This
//! preserves RNode multi-interface commands (CMD_INT1_DATA=0x10, etc.).
//!
//! # no_std Support
//!
//! [`frame_to_slice`] and [`max_framed_size`] work without allocation.
//! [`KissDeframer`] and [`frame`] require `alloc`.

use alloc::vec::Vec;

/// KISS frame delimiter
pub const FEND: u8 = 0xC0;

/// KISS escape byte
pub const FESC: u8 = 0xDB;

/// Transposed FEND (after FESC)
pub const TFEND: u8 = 0xDC;

/// Transposed FESC (after FESC)
pub const TFESC: u8 = 0xDD;

// --- KISS TNC command bytes (the low nibble after the opening FEND) ---
//
// These are the standard KISS protocol commands a host sends to a TNC. They
// are shared by the `KISSInterface` and are reusable by AX.25 framing (#97),
// which layers on the same command set. Values match Python Reticulum's
// `KISS` class in `KISSInterface.py`.

/// Data frame: the payload is a packet to transmit / that was received.
pub const CMD_DATA: u8 = 0x00;
/// TX delay / preamble length (value in units of 10 ms).
pub const CMD_TXDELAY: u8 = 0x01;
/// Persistence parameter `p` for the CSMA/p-persistent algorithm.
pub const CMD_P: u8 = 0x02;
/// Slot time (value in units of 10 ms).
pub const CMD_SLOTTIME: u8 = 0x03;
/// TX tail (value in units of 10 ms).
pub const CMD_TXTAIL: u8 = 0x04;
/// Software flow-control "ready for next frame" signal.
pub const CMD_READY: u8 = 0x0F;

/// Check if a byte needs KISS escaping
#[inline]
pub fn needs_escape(byte: u8) -> bool {
    byte == FEND || byte == FESC
}

/// Calculate the maximum framed size for a given payload length
///
/// Worst case: FEND + command + every payload byte escaped (doubled) + FEND.
#[inline]
pub const fn max_framed_size(payload_len: usize) -> usize {
    3 + payload_len * 2
}

/// Frame a KISS packet into a Vec
///
/// Writes FEND + command + escaped payload + FEND into `output`.
/// The output Vec is cleared before writing.
pub fn frame(command: u8, payload: &[u8], output: &mut Vec<u8>) {
    output.clear();
    output.reserve(max_framed_size(payload.len()));

    output.push(FEND);
    output.push(command);

    for &byte in payload {
        match byte {
            FEND => {
                output.push(FESC);
                output.push(TFEND);
            }
            FESC => {
                output.push(FESC);
                output.push(TFESC);
            }
            _ => output.push(byte),
        }
    }

    output.push(FEND);
}

/// Frame a KISS packet into a fixed-size buffer (no allocation)
///
/// Returns the number of bytes written, or `None` if the buffer is too small.
pub fn frame_to_slice(command: u8, payload: &[u8], output: &mut [u8]) -> Option<usize> {
    let mut pos = 0;

    let mut push = |byte: u8| -> Option<()> {
        if pos < output.len() {
            output[pos] = byte;
            pos += 1;
            Some(())
        } else {
            None
        }
    };

    push(FEND)?;
    push(command)?;

    for &byte in payload {
        match byte {
            FEND => {
                push(FESC)?;
                push(TFEND)?;
            }
            FESC => {
                push(FESC)?;
                push(TFESC)?;
            }
            _ => {
                push(byte)?;
            }
        }
    }

    push(FEND)?;

    Some(pos)
}

/// Result from the KISS deframer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KissDeframeResult {
    /// Need more data to complete a frame
    NeedMore,
    /// Complete frame with command byte and payload
    Frame { command: u8, payload: Vec<u8> },
    /// Frame ended without a command byte ever being set.
    ///
    /// This indicates an internal state machine bug, not malformed input:
    /// `process_byte` only finalizes frames whose command byte was seen.
    MissingCommand,
}

/// KISS deframer state machine
///
/// Processes a stream of bytes and extracts complete KISS frames.
///
/// # Example
///
/// ```
/// use leviculum_core::framing::kiss::{KissDeframer, KissDeframeResult, frame};
///
/// let mut framed = Vec::new();
/// frame(0x00, b"Hello", &mut framed);
///
/// let mut deframer = KissDeframer::with_max_payload(508);
/// let results = deframer.process(&framed);
///
/// assert_eq!(results.len(), 1);
/// match &results[0] {
///     KissDeframeResult::Frame { command, payload } => {
///         assert_eq!(*command, 0x00);
///         assert_eq!(payload.as_slice(), b"Hello");
///     }
///     _ => panic!("Expected frame"),
/// }
/// ```
pub struct KissDeframer {
    buffer: Vec<u8>,
    in_frame: bool,
    escape_next: bool,
    command: Option<u8>,
    max_payload: usize,
}

impl KissDeframer {
    /// Create a new KISS deframer with the given maximum payload size
    ///
    /// Frames with payloads exceeding `max_payload` are truncated but still
    /// delivered (matching Python Reticulum behavior).
    ///
    /// Common values: RNode=508, KISSInterface=564.
    pub fn with_max_payload(max_payload: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(max_payload),
            in_frame: false,
            escape_next: false,
            command: None,
            max_payload,
        }
    }

    /// Whether the deframer is currently mid-frame (an opening FEND was seen
    /// but the closing FEND has not arrived yet). Used by stream interfaces to
    /// arm an incomplete-frame timeout.
    pub fn is_in_frame(&self) -> bool {
        self.in_frame
    }

    /// Number of payload bytes buffered for the in-progress frame. Used by
    /// stream interfaces to bound memory by discarding a runaway partial frame.
    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Reset the deframer state, discarding any partial frame
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.in_frame = false;
        self.escape_next = false;
        self.command = None;
    }

    /// Process incoming bytes, returning any complete frames
    pub fn process(&mut self, data: &[u8]) -> Vec<KissDeframeResult> {
        let mut results = Vec::new();

        for &byte in data {
            if let Some(result) = self.process_byte(byte) {
                results.push(result);
            }
        }

        results
    }

    /// Process a single byte through the state machine
    fn process_byte(&mut self, byte: u8) -> Option<KissDeframeResult> {
        if byte == FEND {
            if self.in_frame && self.command.is_some() {
                let result = self.finalize_frame();
                // FEND is also the opening delimiter of the next frame
                self.in_frame = true;
                self.command = None;
                self.buffer.clear();
                self.escape_next = false;
                return Some(result);
            } else {
                // Start of frame (or back-to-back FEND)
                self.in_frame = true;
                self.command = None;
                self.buffer.clear();
                self.escape_next = false;
            }
        } else if self.in_frame {
            if self.command.is_none() {
                self.command = Some(byte);
            } else if self.escape_next {
                let resolved = match byte {
                    TFEND => FEND,
                    TFESC => FESC,
                    other => other,
                };
                self.escape_next = false;
                if self.buffer.len() < self.max_payload {
                    self.buffer.push(resolved);
                }
            } else if byte == FESC {
                self.escape_next = true;
            } else if self.buffer.len() < self.max_payload {
                self.buffer.push(byte);
            }
        }
        // Bytes outside frame are silently ignored

        None
    }

    /// Finalize a complete frame
    fn finalize_frame(&mut self) -> KissDeframeResult {
        match self.command {
            Some(command) => KissDeframeResult::Frame {
                command,
                payload: self.buffer.clone(),
            },
            // Unreachable via process_byte (which guards on command.is_some());
            // report instead of fabricating a command-0 data frame.
            None => KissDeframeResult::MissingCommand,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    // --- Primitive tests ---

    #[test]
    fn test_needs_escape() {
        assert!(needs_escape(FEND));
        assert!(needs_escape(FESC));
        assert!(!needs_escape(TFEND));
        assert!(!needs_escape(TFESC));
        assert!(!needs_escape(0x00));
        assert!(!needs_escape(0xFF));
        assert!(!needs_escape(0x42));
    }

    #[test]
    fn test_max_framed_size() {
        assert_eq!(max_framed_size(0), 3);
        assert_eq!(max_framed_size(5), 13);
        assert_eq!(max_framed_size(508), 1019);
    }

    // --- Framing tests ---

    #[test]
    fn test_frame_simple() {
        let mut out = Vec::new();
        frame(0x00, b"Hello", &mut out);
        assert_eq!(out, vec![0xC0, 0x00, 0x48, 0x65, 0x6C, 0x6C, 0x6F, 0xC0]);
    }

    #[test]
    fn test_frame_with_fend_in_payload() {
        let mut out = Vec::new();
        frame(0x00, &[0xC0], &mut out);
        assert_eq!(out, vec![FEND, 0x00, FESC, TFEND, FEND]);
    }

    #[test]
    fn test_frame_with_fesc_in_payload() {
        let mut out = Vec::new();
        frame(0x00, &[0xDB], &mut out);
        assert_eq!(out, vec![FEND, 0x00, FESC, TFESC, FEND]);
    }

    #[test]
    fn test_frame_with_both_specials() {
        let mut out = Vec::new();
        frame(0x00, &[FEND, FESC], &mut out);
        assert_eq!(out, vec![FEND, 0x00, FESC, TFEND, FESC, TFESC, FEND]);
    }

    #[test]
    fn test_frame_empty_payload() {
        let mut out = Vec::new();
        frame(0x00, &[], &mut out);
        assert_eq!(out, vec![FEND, 0x00, FEND]);
    }

    #[test]
    fn test_frame_to_slice_simple() {
        let mut buf = [0u8; 32];
        let len = frame_to_slice(0x00, b"Hello", &mut buf).unwrap();
        assert_eq!(len, 8);
        assert_eq!(
            &buf[..len],
            &[0xC0, 0x00, 0x48, 0x65, 0x6C, 0x6C, 0x6F, 0xC0]
        );
    }

    #[test]
    fn test_frame_to_slice_too_small() {
        let mut buf = [0u8; 2];
        assert!(frame_to_slice(0x00, b"Hello", &mut buf).is_none());
    }

    // --- Round-trip tests ---

    #[test]
    fn test_roundtrip_simple() {
        let mut framed = Vec::new();
        frame(0x00, b"Hello", &mut framed);

        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&framed);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), b"Hello");
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_roundtrip_with_escaping() {
        let data = [FEND, 0x42, FESC, 0x00];
        let mut framed = Vec::new();
        frame(0x00, &data, &mut framed);

        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&framed);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &data);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_roundtrip_all_byte_values() {
        let data: Vec<u8> = (0..=255).collect();
        let mut framed = Vec::new();
        frame(0x00, &data, &mut framed);

        let mut deframer = KissDeframer::with_max_payload(512);
        let results = deframer.process(&framed);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), data.as_slice());
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_roundtrip_max_size() {
        let data = vec![0xAB; 508];
        let mut framed = Vec::new();
        frame(0x00, &data, &mut framed);

        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&framed);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.len(), 508);
                assert_eq!(payload.as_slice(), data.as_slice());
            }
            _ => panic!("Expected Frame"),
        }
    }

    // --- Deframer tests ---

    #[test]
    fn test_deframe_simple() {
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, 0x00, 0x41, 0x42, FEND]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[0x41, 0x42]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_deframe_byte_at_a_time() {
        let data = [FEND, 0x00, 0x41, 0x42, FEND];
        let mut deframer = KissDeframer::with_max_payload(508);

        for (i, &byte) in data.iter().enumerate() {
            let results = deframer.process(&[byte]);
            if i < data.len() - 1 {
                assert!(results.is_empty(), "Unexpected result at byte {i}");
            } else {
                assert_eq!(results.len(), 1);
                match &results[0] {
                    KissDeframeResult::Frame { command, payload } => {
                        assert_eq!(*command, 0x00);
                        assert_eq!(payload.as_slice(), &[0x41, 0x42]);
                    }
                    _ => panic!("Expected Frame"),
                }
            }
        }
    }

    #[test]
    fn test_deframe_back_to_back_fends() {
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, FEND]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_deframe_shared_fend() {
        // Closing FEND of first frame = opening FEND of second frame
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, 0x00, 0x41, FEND, 0x00, 0x42, FEND]);

        assert_eq!(results.len(), 2);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[0x41]);
            }
            _ => panic!("Expected Frame 1"),
        }
        match &results[1] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[0x42]);
            }
            _ => panic!("Expected Frame 2"),
        }
    }

    #[test]
    fn test_deframe_multiple_frames() {
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, 0x00, 0x41, FEND, FEND, 0x00, 0x42, FEND]);

        assert_eq!(results.len(), 2);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[0x41]);
            }
            _ => panic!("Expected Frame 1"),
        }
        match &results[1] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[0x42]);
            }
            _ => panic!("Expected Frame 2"),
        }
    }

    #[test]
    fn test_deframe_garbage_before_fend() {
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[0xFF, 0x42, 0x00, FEND, 0x00, 0x41, FEND]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[0x41]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_deframe_command_extraction() {
        let mut deframer = KissDeframer::with_max_payload(508);

        for cmd in [0x00, 0x0F, 0x10, 0x20, 0xFF] {
            let results = deframer.process(&[FEND, cmd, 0x41, FEND]);
            assert_eq!(results.len(), 1);
            match &results[0] {
                KissDeframeResult::Frame { command, payload } => {
                    assert_eq!(*command, cmd, "Command mismatch for 0x{cmd:02X}");
                    assert_eq!(payload.as_slice(), &[0x41]);
                }
                _ => panic!("Expected Frame for command 0x{cmd:02X}"),
            }
        }
    }

    #[test]
    fn test_deframe_empty_payload() {
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, 0x00, FEND]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert!(payload.is_empty());
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_deframe_escaped_fend() {
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, 0x00, FESC, TFEND, FEND]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[FEND]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_deframe_escaped_fesc() {
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, 0x00, FESC, TFESC, FEND]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[FESC]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_deframe_invalid_escape() {
        // FESC followed by neither TFEND nor TFESC, accept byte as-is
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, 0x00, FESC, 0x42, FEND]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[0x42]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_deframe_trailing_fesc() {
        // FESC at end of frame: escape_next is set, then FEND finalizes.
        // The incomplete escape is silently lost.
        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&[FEND, 0x00, FESC, FEND]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert!(payload.is_empty());
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_deframe_oversize_truncated() {
        let max = 4;
        let mut deframer = KissDeframer::with_max_payload(max);
        // Payload of 8 bytes, but max_payload is 4
        let results = deframer.process(&[
            FEND, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, FEND,
        ]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.len(), max);
                assert_eq!(payload.as_slice(), &[0x01, 0x02, 0x03, 0x04]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_deframe_reset() {
        let mut deframer = KissDeframer::with_max_payload(508);
        // Start a partial frame
        deframer.process(&[FEND, 0x00, 0x41]);
        // Reset mid-frame
        deframer.reset();
        // New frame should work fine
        let results = deframer.process(&[FEND, 0x00, 0x42, FEND]);

        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[0x42]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_finalize_without_command_is_not_silent() {
        // Drive the deframer into the internally-impossible state of an open
        // frame with no command byte (only reachable via a state machine bug;
        // process_byte guards finalization on command.is_some()).
        let mut deframer = KissDeframer::with_max_payload(508);
        deframer.in_frame = true;
        deframer.buffer.extend_from_slice(&[0x41, 0x42]);

        let result = deframer.finalize_frame();
        // Must be reported as malformed, not silently fabricated as a
        // command-0 data frame.
        assert_eq!(result, KissDeframeResult::MissingCommand);
    }

    // --- Python interop vectors ---

    #[test]
    fn test_python_vector_escape_fend() {
        // Payload [0xC0] should escape to [DB DC]
        let mut out = Vec::new();
        frame(0x00, &[FEND], &mut out);
        assert_eq!(out, vec![FEND, 0x00, FESC, TFEND, FEND]);

        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&out);
        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[FEND]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_python_vector_escape_fesc() {
        // Payload [0xDB] should escape to [DB DD]
        let mut out = Vec::new();
        frame(0x00, &[FESC], &mut out);
        assert_eq!(out, vec![FEND, 0x00, FESC, TFESC, FEND]);

        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&out);
        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[FESC]);
            }
            _ => panic!("Expected Frame"),
        }
    }

    #[test]
    fn test_python_vector_escape_both() {
        // Payload [DB, C0] should escape to [DB DD, DB DC]
        let mut out = Vec::new();
        frame(0x00, &[FESC, FEND], &mut out);
        assert_eq!(out, vec![FEND, 0x00, FESC, TFESC, FESC, TFEND, FEND]);

        let mut deframer = KissDeframer::with_max_payload(508);
        let results = deframer.process(&out);
        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, 0x00);
                assert_eq!(payload.as_slice(), &[FESC, FEND]);
            }
            _ => panic!("Expected Frame"),
        }
    }
}
