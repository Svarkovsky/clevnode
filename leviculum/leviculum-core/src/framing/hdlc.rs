//! HDLC framing for stream-based interfaces
//!
//! Used by TCP and Serial interfaces to frame packets.
//!
//! Reticulum uses simplified HDLC framing WITHOUT CRC:
//! Format: [FLAG (0x7E)] [Escaped Data] [FLAG (0x7E)]
//!
//! # no_std Support
//!
//! Core framing functions (`frame_to_slice`, `crc16`) work without allocation.
//! The `Deframer` and `frame()` convenience functions require the `alloc` feature.

use alloc::vec::Vec;

use crate::constants::{BITS_PER_BYTE, CRC_HIGH_BIT, CRC_INITIAL};

/// HDLC flag byte
pub const FLAG: u8 = 0x7E;

/// HDLC escape byte
pub const ESCAPE: u8 = 0x7D;

/// XOR value for escaped bytes
pub const ESCAPE_XOR: u8 = 0x20;

/// CRC-16-CCITT polynomial
const CRC_POLY: u16 = 0x1021;

/// Calculate CRC-16-CCITT
///
/// This is a pure function with no allocation.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = CRC_INITIAL;

    for byte in data {
        crc ^= (*byte as u16) << BITS_PER_BYTE;
        for _ in 0..BITS_PER_BYTE {
            if crc & CRC_HIGH_BIT != 0 {
                crc = (crc << 1) ^ CRC_POLY;
            } else {
                crc <<= 1;
            }
        }
    }

    crc
}

/// Check if a byte needs escaping
#[inline]
pub fn needs_escape(byte: u8) -> bool {
    byte == FLAG || byte == ESCAPE
}

/// Calculate the maximum framed size for given data length
///
/// Worst case: every byte needs escaping (doubles) plus 2 flag bytes.
#[inline]
pub const fn max_framed_size(data_len: usize) -> usize {
    2 + (data_len * 2)
}

/// Frame data into a buffer (no allocation)
///
/// Returns the number of bytes written, or `None` if the buffer is too small.
///
/// # Example
///
/// ```
/// use leviculum_core::framing::hdlc::{frame_to_slice, max_framed_size};
///
/// let data = b"Hello";
/// let mut buf = [0u8; 32];
/// let len = frame_to_slice(data, &mut buf).unwrap();
/// assert!(len >= 7); // FLAG + 5 bytes + FLAG
/// ```
pub fn frame_to_slice(data: &[u8], output: &mut [u8]) -> Option<usize> {
    // Check pessimistically - assumes worst case where every byte needs escaping.
    // Could still fit if few escapes needed, but checking as we go is more reliable.

    let mut pos = 0;

    // Helper to push a byte
    let mut push = |byte: u8| -> Option<()> {
        if pos < output.len() {
            output[pos] = byte;
            pos += 1;
            Some(())
        } else {
            None
        }
    };

    // Start flag
    push(FLAG)?;

    // Escape and write data
    for &byte in data {
        if needs_escape(byte) {
            push(ESCAPE)?;
            push(byte ^ ESCAPE_XOR)?;
        } else {
            push(byte)?;
        }
    }

    // End flag
    push(FLAG)?;

    Some(pos)
}

/// Frame data with simplified HDLC encoding (no CRC)
///
/// This matches Python Reticulum's framing format.
pub fn frame(data: &[u8], output: &mut Vec<u8>) {
    output.clear();
    output.reserve(max_framed_size(data.len()));

    // Start flag
    output.push(FLAG);

    // Escape and write data
    for &byte in data {
        if needs_escape(byte) {
            output.push(ESCAPE);
            output.push(byte ^ ESCAPE_XOR);
        } else {
            output.push(byte);
        }
    }

    // End flag
    output.push(FLAG);
}

/// Frame data with HDLC encoding including CRC-16
pub fn frame_with_crc(data: &[u8], output: &mut Vec<u8>) {
    output.clear();

    // Start flag
    output.push(FLAG);

    // Calculate CRC over unescaped data
    let crc = crc16(data);

    // Escape and write data
    for &byte in data {
        if needs_escape(byte) {
            output.push(ESCAPE);
            output.push(byte ^ ESCAPE_XOR);
        } else {
            output.push(byte);
        }
    }

    // Escape and write CRC (big-endian)
    let crc_bytes = crc.to_be_bytes();
    for &byte in &crc_bytes {
        if needs_escape(byte) {
            output.push(ESCAPE);
            output.push(byte ^ ESCAPE_XOR);
        } else {
            output.push(byte);
        }
    }

    // End flag
    output.push(FLAG);
}

/// Deframing result
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeframeResult {
    /// Need more data
    NeedMore,
    /// Complete frame
    Frame(Vec<u8>),
    /// Frame too short (empty)
    TooShort,
    /// The frame grew past the deframer's per-frame ceiling and was discarded.
    ///
    /// The partial payload is dropped rather than delivered truncated — half a
    /// packet is not a packet — and the deframer resynchronises on the next
    /// FLAG. See [`DEFAULT_MAX_FRAME`].
    Oversized,
}

/// Bytes a fresh [`Deframer`] reserves up front. Frames on every interface we
/// speak are far smaller than any of our HW_MTUs, so the buffer starts small
/// and grows, rather than reserving the ceiling per connection.
const INITIAL_BUFFER_CAPACITY: usize = 600;

/// Default ceiling on the unescaped payload a [`Deframer`] will accumulate for
/// one frame, in bytes.
///
/// Codeberg #271: `process_byte` used to push every non-FLAG byte with no
/// bound of its own, which made the cap each caller's job — and three of the
/// callers (`tcp.rs`, `local.rs`, `i2p/mod.rs`) did not do it, while four
/// others did. A peer streaming bytes that never contain a FLAG octet grew the
/// `Vec` until the process was out of memory; on the Local interface anything
/// that can reach the shared-instance socket could do the same. The bound now
/// lives here, so a caller cannot forget it; callers that know a tighter
/// HW_MTU pass it to [`Deframer::with_max_frame`].
///
/// 262144 is the largest HW_MTU we speak — the shared-instance interface's
/// (`LocalInterface.HW_MTU`, LocalInterface.py:71) — so the default never
/// discards a frame a Python peer may legitimately send. TCP is not the
/// ceiling any more: its HW_MTU is derived from the interface bitrate like
/// Python's (Codeberg #355) and passes the tighter bound to
/// [`Deframer::with_max_frame`].
///
/// Note the reference does *not* bound its own HDLC path: `TCPInterface.py`'s
/// `len(data_buffer) < self.HW_MTU` check sits in the KISS branch
/// (TCPInterface.py:362), not the HDLC branch (:380-397). The justification for
/// capping is our own — four of our interfaces already treated this as a bound
/// worth enforcing, and nothing made the other three agree.
pub const DEFAULT_MAX_FRAME: usize = 262_144;

/// HDLC deframer state machine
///
/// Processes a stream of bytes and extracts complete frames.
///
/// # Example
///
/// ```
/// use leviculum_core::framing::hdlc::{Deframer, DeframeResult, frame};
///
/// let data = b"Hello";
/// let mut framed = Vec::new();
/// frame(data, &mut framed);
///
/// let mut deframer = Deframer::new();
/// let results = deframer.process(&framed);
///
/// assert_eq!(results.len(), 1);
/// match &results[0] {
///     DeframeResult::Frame(decoded) => assert_eq!(decoded.as_slice(), data),
///     _ => panic!("Expected frame"),
/// }
/// ```
pub struct Deframer {
    buffer: Vec<u8>,
    in_frame: bool,
    escape_next: bool,
    max_frame: usize,
}

impl Deframer {
    /// Create a new deframer bounded at [`DEFAULT_MAX_FRAME`].
    pub fn new() -> Self {
        Self::with_max_frame(DEFAULT_MAX_FRAME)
    }

    /// Create a deframer that discards any frame whose unescaped payload grows
    /// past `max_frame` bytes.
    ///
    /// Interfaces pass their own HW_MTU here. That is the only mechanism
    /// bounding the buffer — an interface must not also poll
    /// [`buffer_len`](Self::buffer_len) and [`reset`](Self::reset) itself
    /// (Codeberg #271).
    pub fn with_max_frame(max_frame: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(INITIAL_BUFFER_CAPACITY.min(max_frame)),
            in_frame: false,
            escape_next: false,
            max_frame,
        }
    }

    /// Reset the deframer state
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.in_frame = false;
        self.escape_next = false;
    }

    /// Whether the deframer is currently inside a frame (waiting for closing FLAG).
    ///
    /// Used by serial interfaces to detect stale partial frames: if `is_in_frame()`
    /// is true and no data arrives for 100ms, the frame should be discarded via
    /// `reset()` to prevent desynchronization from noise/corruption.
    pub fn is_in_frame(&self) -> bool {
        self.in_frame
    }

    /// Current accumulated buffer length, for diagnostics and tests.
    ///
    /// Not a bound an interface has to enforce: the deframer caps itself at
    /// `max_frame` (Codeberg #271). Interfaces used to poll this and call
    /// `reset()` past their HW_MTU; they now pass the HW_MTU to
    /// [`with_max_frame`](Self::with_max_frame) instead.
    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Process incoming bytes
    pub fn process(&mut self, data: &[u8]) -> Vec<DeframeResult> {
        let mut results = Vec::new();

        for &byte in data {
            if let Some(result) = self.process_byte(byte) {
                results.push(result);
            }
        }

        results
    }

    /// Process a single byte
    fn process_byte(&mut self, byte: u8) -> Option<DeframeResult> {
        if byte == FLAG {
            if self.in_frame && !self.buffer.is_empty() {
                // End of frame
                let result = self.finalize_frame();
                self.reset();
                return Some(result);
            } else {
                // Start of frame
                self.in_frame = true;
                self.buffer.clear();
                self.escape_next = false;
            }
        } else if self.in_frame {
            if self.escape_next {
                self.escape_next = false;
                return self.push(byte ^ ESCAPE_XOR);
            } else if byte == ESCAPE {
                self.escape_next = true;
            } else {
                return self.push(byte);
            }
        }
        // Bytes outside of frame are ignored

        None
    }

    /// Buffer one unescaped payload byte, discarding the whole frame if it
    /// would grow past `max_frame` (Codeberg #271).
    ///
    /// Discarding leaves the deframer outside a frame, so the rest of the
    /// runaway bytes are ignored for free and the next FLAG opens a fresh
    /// frame — the same resynchronisation the interface-side `reset()` gave,
    /// but exact rather than one read-chunk late.
    fn push(&mut self, byte: u8) -> Option<DeframeResult> {
        if self.buffer.len() >= self.max_frame {
            self.reset();
            // Do not hold the ceiling's worth of capacity for the rest of the
            // connection's life just because a peer once sent garbage.
            self.buffer.shrink_to(INITIAL_BUFFER_CAPACITY);
            return Some(DeframeResult::Oversized);
        }
        self.buffer.push(byte);
        None
    }

    /// Finalize a complete frame (CRC not validated - Reticulum uses simplified HDLC)
    fn finalize_frame(&mut self) -> DeframeResult {
        if self.buffer.is_empty() {
            return DeframeResult::TooShort;
        }

        DeframeResult::Frame(self.buffer.clone())
    }
}

impl Default for Deframer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (Codeberg #271): a peer that opens a frame and then streams
    /// bytes containing no FLAG octet must not grow the buffer without limit.
    ///
    /// Before the cap moved into the deframer this held every byte fed to it —
    /// verified red at 266240 bytes buffered — because the bound was the
    /// caller's job and `tcp.rs`, `local.rs` and `i2p/mod.rs` never did it.
    #[test]
    fn runaway_frame_is_bounded_by_default() {
        let mut deframer = Deframer::new();
        let mut stream = alloc::vec![FLAG];
        stream.resize(1 + DEFAULT_MAX_FRAME + 4096, 0xAA);

        let results = deframer.process(&stream);

        assert!(
            deframer.buffer_len() <= DEFAULT_MAX_FRAME,
            "deframer buffered {} bytes for one unterminated frame",
            deframer.buffer_len()
        );
        assert_eq!(
            results,
            alloc::vec![DeframeResult::Oversized],
            "the discard is reported once, not per excess byte"
        );
    }

    /// The cap is the interface's HW_MTU when it passes one, and the discard is
    /// exact: `max_frame` bytes are accepted, the next one drops the frame.
    #[test]
    fn with_max_frame_discards_at_the_ceiling_and_resyncs() {
        let mut deframer = Deframer::with_max_frame(8);

        // A frame of exactly max_frame bytes still arrives.
        let mut framed = Vec::new();
        frame(b"12345678", &mut framed);
        assert_eq!(
            deframer.process(&framed),
            alloc::vec![DeframeResult::Frame(b"12345678".to_vec())]
        );

        // One byte more and the frame is discarded, not truncated.
        let mut framed = Vec::new();
        frame(b"123456789", &mut framed);
        assert_eq!(
            deframer.process(&framed),
            alloc::vec![DeframeResult::Oversized]
        );
        assert_eq!(deframer.buffer_len(), 0);

        // ...and the next good frame still comes through.
        let mut framed = Vec::new();
        frame(b"ok", &mut framed);
        assert_eq!(
            deframer.process(&framed),
            alloc::vec![DeframeResult::Frame(b"ok".to_vec())]
        );
    }

    /// Escaped bytes count against the ceiling too — a peer must not be able to
    /// buy extra buffer by escaping every octet.
    #[test]
    fn escaped_bytes_count_against_the_ceiling() {
        let mut deframer = Deframer::with_max_frame(4);
        let mut stream = alloc::vec![FLAG];
        for _ in 0..64 {
            stream.push(ESCAPE);
            stream.push(FLAG ^ ESCAPE_XOR);
        }
        let results = deframer.process(&stream);
        assert_eq!(results, alloc::vec![DeframeResult::Oversized]);
        assert!(deframer.buffer_len() <= 4);
    }

    #[test]
    fn test_crc16() {
        // Test vector: "123456789" should give 0x29B1
        let data = b"123456789";
        let crc = crc16(data);
        assert_eq!(crc, 0x29B1);
    }

    #[test]
    fn test_frame_to_slice_simple() {
        let data = b"Hello";
        let mut buf = [0u8; 32];
        let len = frame_to_slice(data, &mut buf).unwrap();

        // Should be FLAG + 5 bytes + FLAG = 7
        assert_eq!(len, 7);
        assert_eq!(buf[0], FLAG);
        assert_eq!(&buf[1..6], b"Hello");
        assert_eq!(buf[6], FLAG);
    }

    #[test]
    fn test_frame_to_slice_with_escape() {
        let data = [0x00, FLAG, 0xFF];
        let mut buf = [0u8; 32];
        let len = frame_to_slice(&data, &mut buf).unwrap();

        // FLAG + 0x00 + ESCAPE + (FLAG^0x20) + 0xFF + FLAG = 6
        assert_eq!(len, 6);
        assert_eq!(buf[0], FLAG);
        assert_eq!(buf[1], 0x00);
        assert_eq!(buf[2], ESCAPE);
        assert_eq!(buf[3], FLAG ^ ESCAPE_XOR);
        assert_eq!(buf[4], 0xFF);
        assert_eq!(buf[5], FLAG);
    }

    #[test]
    fn test_frame_to_slice_buffer_too_small() {
        let data = b"Hello";
        let mut buf = [0u8; 2]; // Too small
        assert!(frame_to_slice(data, &mut buf).is_none());
    }

    #[test]
    fn test_frame_roundtrip() {
        let data = b"Hello, HDLC!";
        let mut framed = Vec::new();
        frame(data, &mut framed);

        // Frame should be: FLAG + escaped_data + FLAG
        assert_eq!(framed[0], FLAG);
        assert_eq!(framed[framed.len() - 1], FLAG);

        let mut deframer = Deframer::new();
        let results = deframer.process(&framed);

        assert_eq!(results.len(), 1);
        match &results[0] {
            DeframeResult::Frame(decoded) => assert_eq!(decoded.as_slice(), data.as_slice()),
            _ => panic!("Expected Frame result"),
        }
    }

    #[test]
    fn test_escape_flag_byte() {
        // Data containing FLAG byte should be escaped
        let data = [0x00, FLAG, 0xFF];
        let mut framed = Vec::new();
        frame(&data, &mut framed);

        // Check that FLAG is escaped (should see ESCAPE followed by FLAG^0x20)
        let escaped_flag = FLAG ^ ESCAPE_XOR;
        assert!(framed.windows(2).any(|w| w == [ESCAPE, escaped_flag]));

        let mut deframer = Deframer::new();
        let results = deframer.process(&framed);

        assert_eq!(results.len(), 1);
        match &results[0] {
            DeframeResult::Frame(decoded) => assert_eq!(decoded.as_slice(), data.as_slice()),
            _ => panic!("Expected Frame result"),
        }
    }

    #[test]
    fn test_escape_escape_byte() {
        // Data containing ESCAPE byte should be escaped
        let data = [0x00, ESCAPE, 0xFF];
        let mut framed = Vec::new();
        frame(&data, &mut framed);

        let mut deframer = Deframer::new();
        let results = deframer.process(&framed);

        assert_eq!(results.len(), 1);
        match &results[0] {
            DeframeResult::Frame(decoded) => assert_eq!(decoded.as_slice(), data.as_slice()),
            _ => panic!("Expected Frame result"),
        }
    }

    #[test]
    fn test_incremental_processing() {
        let data = b"Test data";
        let mut framed = Vec::new();
        frame(data, &mut framed);

        let mut deframer = Deframer::new();

        // Process byte by byte
        for (i, &byte) in framed.iter().enumerate() {
            let results = deframer.process(&[byte]);
            if i < framed.len() - 1 {
                assert!(results.is_empty());
            } else {
                assert_eq!(results.len(), 1);
            }
        }
    }

    #[test]
    fn test_frame_with_crc_roundtrip() {
        // Test the CRC variant still works for internal consistency
        let data = b"Hello, HDLC with CRC!";
        let mut framed = Vec::new();
        frame_with_crc(data, &mut framed);

        // Frame should include data + 2 byte CRC
        // We can't easily verify this without a CRC-aware deframer
        // Just verify it's longer than the no-CRC version
        let mut framed_no_crc = Vec::new();
        frame(data, &mut framed_no_crc);

        // CRC version should be at least 2 bytes longer (might be more due to escaping)
        assert!(framed.len() >= framed_no_crc.len() + 2);
    }

    // Python Reticulum interop test vectors
    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn test_python_vector_simple() {
        // Simple data "Hello" with no escaping needed
        let data = hex_decode("48656c6c6f");
        let expected = hex_decode("7e48656c6c6f7e");

        let mut framed = Vec::new();
        frame(&data, &mut framed);
        assert_eq!(framed, expected, "Simple frame mismatch with Python");

        // Also verify deframe
        let mut deframer = Deframer::new();
        let results = deframer.process(&framed);
        assert_eq!(results.len(), 1);
        match &results[0] {
            DeframeResult::Frame(decoded) => assert_eq!(*decoded, data),
            _ => panic!("Expected Frame result"),
        }
    }

    #[test]
    fn test_python_vector_with_flag() {
        // Data containing FLAG byte (0x7E): [0x00, 0x7E, 0xFF]
        let data = hex_decode("007eff");
        let expected = hex_decode("7e007d5eff7e");

        let mut framed = Vec::new();
        frame(&data, &mut framed);
        assert_eq!(
            framed, expected,
            "Frame with FLAG byte mismatch with Python"
        );

        let mut deframer = Deframer::new();
        let results = deframer.process(&framed);
        assert_eq!(results.len(), 1);
        match &results[0] {
            DeframeResult::Frame(decoded) => assert_eq!(*decoded, data),
            _ => panic!("Expected Frame result"),
        }
    }

    #[test]
    fn test_python_vector_with_escape() {
        // Data containing ESCAPE byte (0x7D): [0x00, 0x7D, 0xFF]
        let data = hex_decode("007dff");
        let expected = hex_decode("7e007d5dff7e");

        let mut framed = Vec::new();
        frame(&data, &mut framed);
        assert_eq!(
            framed, expected,
            "Frame with ESCAPE byte mismatch with Python"
        );

        let mut deframer = Deframer::new();
        let results = deframer.process(&framed);
        assert_eq!(results.len(), 1);
        match &results[0] {
            DeframeResult::Frame(decoded) => assert_eq!(*decoded, data),
            _ => panic!("Expected Frame result"),
        }
    }

    #[test]
    fn test_python_vector_with_both() {
        // Data containing both FLAG and ESCAPE: [0x7E, 0x00, 0x7D, 0xFF]
        let data = hex_decode("7e007dff");
        let expected = hex_decode("7e7d5e007d5dff7e");

        let mut framed = Vec::new();
        frame(&data, &mut framed);
        assert_eq!(
            framed, expected,
            "Frame with both special bytes mismatch with Python"
        );

        let mut deframer = Deframer::new();
        let results = deframer.process(&framed);
        assert_eq!(results.len(), 1);
        match &results[0] {
            DeframeResult::Frame(decoded) => assert_eq!(*decoded, data),
            _ => panic!("Expected Frame result"),
        }
    }

    #[test]
    fn test_python_vector_packet() {
        // A real Reticulum packet
        let data = hex_decode("0000010203040506070809101112131415160048656c6c6f");
        let expected = hex_decode("7e0000010203040506070809101112131415160048656c6c6f7e");

        let mut framed = Vec::new();
        frame(&data, &mut framed);
        assert_eq!(framed, expected, "Real packet frame mismatch with Python");

        let mut deframer = Deframer::new();
        let results = deframer.process(&framed);
        assert_eq!(results.len(), 1);
        match &results[0] {
            DeframeResult::Frame(decoded) => assert_eq!(*decoded, data),
            _ => panic!("Expected Frame result"),
        }
    }
}
