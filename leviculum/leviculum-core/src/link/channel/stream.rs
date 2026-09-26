//! StreamDataMessage for binary stream transfer over channels
//!
//! # Wire Format
//!
//! The complete wire format (must match Python exactly):
//! ```text
//! Envelope: [0xff00:2 BE][seq:2 BE][len:2 BE][stream_header + data]
//! ```
//!
//! ## Stream Header (2 bytes, big-endian)
//!
//! ```text
//! MSB                             LSB
//! ┌─────┬────────┬─────────────────┐
//! │ EOF │ COMP   │    STREAM_ID    │
//! │ b15 │  b14   │   b13 ... b0    │
//! └─────┴────────┴─────────────────┘
//! ```
//!
//! - Bits 0-13 (0x3FFF): stream_id (0-16383)
//! - Bit 14 (0x4000): compressed flag
//! - Bit 15 (0x8000): EOF flag
//!
//! The header is packed as big-endian u16, so the EOF/compressed flags
//! appear in the first byte when serialized.
//!
//! This is a system message type (MSGTYPE >= 0xf000) used by the Buffer system
//! to transfer binary data streams over channels.

use alloc::vec::Vec;

use super::error::ChannelError;
use super::Message;
use crate::constants::{
    STREAM_DATA_HEADER_SIZE, STREAM_DATA_MSGTYPE, STREAM_FLAG_COMPRESSED, STREAM_FLAG_EOF,
    STREAM_ID_MAX,
};

/// Message for binary stream data transfer
///
/// StreamDataMessage is a system message type (0xff00) used by RawChannelReader
/// and RawChannelWriter to transfer binary data streams over channels.
///
/// # Wire Format
///
/// The stream header is 2 bytes big-endian:
/// - bits 0-13: stream_id (0-16383)
/// - bit 14: compressed flag
/// - bit 15: EOF flag
///
/// # Example
///
/// ```
/// use leviculum_core::link::channel::{StreamDataMessage, Message};
///
/// let msg = StreamDataMessage::new(0, vec![1, 2, 3], false, false);
/// let packed = msg.pack();
/// assert!(!packed.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDataMessage {
    /// Stream identifier (0-16383)
    pub stream_id: u16,
    /// Whether the data is BZ2 compressed
    pub compressed: bool,
    /// Whether this is the end of the stream
    pub eof: bool,
    /// Binary data payload
    pub data: Vec<u8>,
}

impl StreamDataMessage {
    /// Create a new StreamDataMessage
    ///
    /// # Arguments
    /// * `stream_id` - Stream identifier (0-16383)
    /// * `data` - Binary data payload
    /// * `eof` - Whether this marks end of stream
    /// * `compressed` - Whether the data is BZ2 compressed
    ///
    /// # Panics
    /// Panics if stream_id > STREAM_ID_MAX (16383).
    /// Use [`try_new`](Self::try_new) for a fallible version.
    pub fn new(stream_id: u16, data: Vec<u8>, eof: bool, compressed: bool) -> Self {
        Self::try_new(stream_id, data, eof, compressed).expect("stream_id exceeds STREAM_ID_MAX")
    }

    /// Create a new StreamDataMessage, returning an error if stream_id is invalid
    ///
    /// # Arguments
    /// * `stream_id` - Stream identifier (0-16383)
    /// * `data` - Binary data payload
    /// * `eof` - Whether this marks end of stream
    /// * `compressed` - Whether the data is BZ2 compressed
    ///
    /// # Errors
    /// Returns `InvalidStreamId` if stream_id > STREAM_ID_MAX (16383)
    pub fn try_new(
        stream_id: u16,
        data: Vec<u8>,
        eof: bool,
        compressed: bool,
    ) -> Result<Self, ChannelError> {
        if stream_id > STREAM_ID_MAX {
            return Err(ChannelError::InvalidStreamId);
        }
        Ok(Self {
            stream_id,
            compressed,
            eof,
            data,
        })
    }

    /// Create a new StreamDataMessage with just data (no EOF, no compression)
    pub fn with_data(stream_id: u16, data: Vec<u8>) -> Self {
        Self::new(stream_id, data, false, false)
    }

    /// Create an EOF marker message
    pub fn eof(stream_id: u16) -> Self {
        Self::new(stream_id, Vec::new(), true, false)
    }

    /// Get the total overhead for a StreamDataMessage (header + envelope)
    ///
    /// This is useful for calculating maximum data size.
    pub const fn overhead() -> usize {
        STREAM_DATA_HEADER_SIZE + crate::constants::CHANNEL_ENVELOPE_HEADER_SIZE
    }
}

impl Message for StreamDataMessage {
    const MSGTYPE: u16 = STREAM_DATA_MSGTYPE;

    fn pack(&self) -> Vec<u8> {
        // Build header: stream_id (14 bits) | compressed (1 bit) | eof (1 bit)
        let mut header: u16 = self.stream_id & STREAM_ID_MAX;
        if self.compressed {
            header |= STREAM_FLAG_COMPRESSED;
        }
        if self.eof {
            header |= STREAM_FLAG_EOF;
        }

        let mut buf = Vec::with_capacity(STREAM_DATA_HEADER_SIZE + self.data.len());
        buf.extend_from_slice(&header.to_be_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }

    fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
        if data.len() < STREAM_DATA_HEADER_SIZE {
            return Err(ChannelError::EnvelopeTooShort);
        }

        let header = u16::from_be_bytes([data[0], data[1]]);
        let stream_id = header & STREAM_ID_MAX;
        let compressed = (header & STREAM_FLAG_COMPRESSED) != 0;
        let eof = (header & STREAM_FLAG_EOF) != 0;
        let payload = data[STREAM_DATA_HEADER_SIZE..].to_vec();

        Ok(Self {
            stream_id,
            compressed,
            eof,
            data: payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn test_stream_data_message_new() {
        let msg = StreamDataMessage::new(123, vec![1, 2, 3], false, false);
        assert_eq!(msg.stream_id, 123);
        assert_eq!(msg.data, vec![1, 2, 3]);
        assert!(!msg.eof);
        assert!(!msg.compressed);
    }

    #[test]
    fn test_stream_data_message_with_data() {
        let msg = StreamDataMessage::with_data(42, vec![4, 5, 6]);
        assert_eq!(msg.stream_id, 42);
        assert_eq!(msg.data, vec![4, 5, 6]);
        assert!(!msg.eof);
        assert!(!msg.compressed);
    }

    #[test]
    fn test_stream_data_message_eof() {
        let msg = StreamDataMessage::eof(99);
        assert_eq!(msg.stream_id, 99);
        assert!(msg.data.is_empty());
        assert!(msg.eof);
        assert!(!msg.compressed);
    }

    #[test]
    fn test_stream_data_message_max_stream_id() {
        let msg = StreamDataMessage::new(STREAM_ID_MAX, vec![], false, false);
        assert_eq!(msg.stream_id, STREAM_ID_MAX);
    }

    #[test]
    #[should_panic(expected = "stream_id exceeds")]
    fn test_stream_data_message_invalid_stream_id() {
        StreamDataMessage::new(STREAM_ID_MAX + 1, vec![], false, false);
    }

    #[test]
    fn test_stream_data_message_try_new_valid() {
        let msg = StreamDataMessage::try_new(123, vec![1, 2, 3], false, false);
        assert!(msg.is_ok());
        let msg = msg.unwrap();
        assert_eq!(msg.stream_id, 123);
    }

    #[test]
    fn test_stream_data_message_try_new_invalid() {
        let msg = StreamDataMessage::try_new(STREAM_ID_MAX + 1, vec![], false, false);
        assert_eq!(msg, Err(ChannelError::InvalidStreamId));
    }

    #[test]
    fn test_stream_data_message_pack_basic() {
        let msg = StreamDataMessage::new(0x1234, vec![0xAA, 0xBB], false, false);
        let packed = msg.pack();

        // Header: 0x1234 (stream_id only, no flags)
        // Data: [0xAA, 0xBB]
        assert_eq!(packed, vec![0x12, 0x34, 0xAA, 0xBB]);
    }

    #[test]
    fn test_stream_data_message_pack_with_eof() {
        let msg = StreamDataMessage::new(0x0001, vec![], true, false);
        let packed = msg.pack();

        // Header: 0x0001 | 0x8000 = 0x8001
        assert_eq!(packed, vec![0x80, 0x01]);
    }

    #[test]
    fn test_stream_data_message_pack_with_compressed() {
        let msg = StreamDataMessage::new(0x0002, vec![0xFF], false, true);
        let packed = msg.pack();

        // Header: 0x0002 | 0x4000 = 0x4002
        assert_eq!(packed, vec![0x40, 0x02, 0xFF]);
    }

    #[test]
    fn test_stream_data_message_pack_with_all_flags() {
        let msg = StreamDataMessage::new(0x0003, vec![0x11], true, true);
        let packed = msg.pack();

        // Header: 0x0003 | 0x8000 | 0x4000 = 0xC003
        assert_eq!(packed, vec![0xC0, 0x03, 0x11]);
    }

    #[test]
    fn test_stream_data_message_unpack_basic() {
        let data = vec![0x12, 0x34, 0xAA, 0xBB];
        let msg = StreamDataMessage::unpack(&data).unwrap();

        assert_eq!(msg.stream_id, 0x1234);
        assert!(!msg.eof);
        assert!(!msg.compressed);
        assert_eq!(msg.data, vec![0xAA, 0xBB]);
    }

    #[test]
    fn test_stream_data_message_unpack_with_eof() {
        let data = vec![0x80, 0x01]; // 0x8001 = eof + stream_id 1
        let msg = StreamDataMessage::unpack(&data).unwrap();

        assert_eq!(msg.stream_id, 1);
        assert!(msg.eof);
        assert!(!msg.compressed);
        assert!(msg.data.is_empty());
    }

    #[test]
    fn test_stream_data_message_unpack_with_compressed() {
        let data = vec![0x40, 0x02, 0xFF]; // 0x4002 = compressed + stream_id 2
        let msg = StreamDataMessage::unpack(&data).unwrap();

        assert_eq!(msg.stream_id, 2);
        assert!(!msg.eof);
        assert!(msg.compressed);
        assert_eq!(msg.data, vec![0xFF]);
    }

    #[test]
    fn test_stream_data_message_unpack_with_all_flags() {
        let data = vec![0xC0, 0x03, 0x11]; // 0xC003 = eof + compressed + stream_id 3
        let msg = StreamDataMessage::unpack(&data).unwrap();

        assert_eq!(msg.stream_id, 3);
        assert!(msg.eof);
        assert!(msg.compressed);
        assert_eq!(msg.data, vec![0x11]);
    }

    #[test]
    fn test_stream_data_message_roundtrip() {
        let original = StreamDataMessage::new(0x2ABC, vec![1, 2, 3, 4, 5], true, true);
        let packed = original.pack();
        let unpacked = StreamDataMessage::unpack(&packed).unwrap();

        assert_eq!(unpacked.stream_id, original.stream_id);
        assert_eq!(unpacked.eof, original.eof);
        assert_eq!(unpacked.compressed, original.compressed);
        assert_eq!(unpacked.data, original.data);
    }

    #[test]
    fn test_stream_data_message_unpack_too_short() {
        let data = vec![0x00]; // Only 1 byte, need 2
        let result = StreamDataMessage::unpack(&data);
        assert_eq!(result, Err(ChannelError::EnvelopeTooShort));
    }

    #[test]
    fn test_stream_data_message_unpack_empty() {
        let data: Vec<u8> = vec![];
        let result = StreamDataMessage::unpack(&data);
        assert_eq!(result, Err(ChannelError::EnvelopeTooShort));
    }

    #[test]
    fn test_stream_data_message_msgtype() {
        assert_eq!(StreamDataMessage::MSGTYPE, 0xff00);
    }

    #[test]
    fn test_stream_data_message_overhead() {
        // 2 bytes header + 6 bytes envelope = 8 bytes total overhead
        assert_eq!(StreamDataMessage::overhead(), 8);
    }

    /// Test wire format matches Python's struct.pack behavior
    ///
    /// Python: header_val = (0x3fff & stream_id) | (0x8000 if eof else 0) | (0x4000 if compressed else 0)
    ///         struct.pack(">H", header_val) + data
    #[test]
    fn test_wire_format_matches_python() {
        // Case 1: Basic message
        let msg = StreamDataMessage::new(100, vec![b'h', b'e', b'l', b'l', b'o'], false, false);
        let packed = msg.pack();
        // Python: struct.pack(">H", 100) + b"hello" = b"\x00\x64hello"
        assert_eq!(packed[0], 0x00);
        assert_eq!(packed[1], 0x64);
        assert_eq!(&packed[2..], b"hello");

        // Case 2: EOF message
        let msg = StreamDataMessage::new(1, vec![], true, false);
        let packed = msg.pack();
        // Python: struct.pack(">H", 0x8001) = b"\x80\x01"
        assert_eq!(packed, vec![0x80, 0x01]);

        // Case 3: Compressed message
        let msg = StreamDataMessage::new(2, vec![0xAB], false, true);
        let packed = msg.pack();
        // Python: struct.pack(">H", 0x4002) + b"\xab" = b"\x40\x02\xab"
        assert_eq!(packed, vec![0x40, 0x02, 0xAB]);

        // Case 4: Max stream_id
        let msg = StreamDataMessage::new(0x3fff, vec![], false, false);
        let packed = msg.pack();
        // Python: struct.pack(">H", 0x3fff) = b"\x3f\xff"
        assert_eq!(packed, vec![0x3f, 0xff]);
    }
}
