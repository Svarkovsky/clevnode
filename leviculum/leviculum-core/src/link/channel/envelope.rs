//! Channel envelope serialization
//!
//! Wire format (must match Python exactly):
//! ```text
//! [MSGTYPE: 2 bytes BE] [SEQUENCE: 2 bytes BE] [LENGTH: 2 bytes BE] [DATA: variable]
//! ```

use alloc::vec::Vec;

use super::error::ChannelError;
use crate::constants::CHANNEL_ENVELOPE_HEADER_SIZE;

/// Envelope wraps a message for transmission over a channel
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Message type identifier (< 0xf000 for user types)
    pub msgtype: u16,
    /// 16-bit sequence number with wraparound
    pub sequence: u16,
    /// Serialized message data
    pub data: Vec<u8>,
}

impl Envelope {
    /// Create a new envelope with the given message type and data
    ///
    /// # Arguments
    /// * `msgtype` - Message type identifier
    /// * `sequence` - Sequence number
    /// * `data` - Serialized message data
    ///
    /// # Panics
    /// Panics if `data.len()` exceeds 65535 bytes (u16::MAX) — the wire length
    /// field is `u16`. The channel send path never constructs through here (it
    /// uses [`try_new`](Self::try_new), and its MDU is capped at the same
    /// ceiling — #242); the assert guards direct construction in tests
    /// and future callers.
    pub fn new(msgtype: u16, sequence: u16, data: Vec<u8>) -> Self {
        Self::try_new(msgtype, sequence, data).expect("envelope data length exceeds u16::MAX")
    }

    /// Fallible constructor: `None` when `data` exceeds the `u16` wire length
    /// field (#242 — a caller-caused, recoverable condition must be
    /// reachable as a value, not only as a panic from a lock-holding thread).
    pub fn try_new(msgtype: u16, sequence: u16, data: Vec<u8>) -> Option<Self> {
        if data.len() > u16::MAX as usize {
            return None;
        }
        Some(Self {
            msgtype,
            sequence,
            data,
        })
    }

    /// Pack the envelope into bytes for transmission
    ///
    /// Format: [MSGTYPE: 2 BE] [SEQUENCE: 2 BE] [LENGTH: 2 BE] [DATA: variable]
    pub fn pack(&self) -> Vec<u8> {
        let length = self.data.len() as u16;
        let mut buf = Vec::with_capacity(CHANNEL_ENVELOPE_HEADER_SIZE + self.data.len());
        buf.extend_from_slice(&self.msgtype.to_be_bytes());
        buf.extend_from_slice(&self.sequence.to_be_bytes());
        buf.extend_from_slice(&length.to_be_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }

    /// Unpack an envelope from bytes
    ///
    /// # Arguments
    /// * `raw` - Raw bytes containing the envelope
    ///
    /// # Errors
    /// - `EnvelopeTooShort` if the header is incomplete (< 6 bytes)
    /// - `EnvelopeTruncated` if the data is shorter than the declared length
    pub fn unpack(raw: &[u8]) -> Result<Self, ChannelError> {
        if raw.len() < CHANNEL_ENVELOPE_HEADER_SIZE {
            return Err(ChannelError::EnvelopeTooShort);
        }

        let msgtype = u16::from_be_bytes([raw[0], raw[1]]);
        let sequence = u16::from_be_bytes([raw[2], raw[3]]);
        let length = u16::from_be_bytes([raw[4], raw[5]]);

        let expected_len = CHANNEL_ENVELOPE_HEADER_SIZE + length as usize;
        if raw.len() < expected_len {
            return Err(ChannelError::EnvelopeTruncated);
        }

        let data = raw[CHANNEL_ENVELOPE_HEADER_SIZE..expected_len].to_vec();

        Ok(Self {
            msgtype,
            sequence,
            data,
        })
    }

    /// Get the total packed size of this envelope
    pub fn packed_size(&self) -> usize {
        CHANNEL_ENVELOPE_HEADER_SIZE + self.data.len()
    }

    /// Get the data payload length
    ///
    /// This is equivalent to `self.data.len()` but provided for API consistency.
    pub fn length(&self) -> usize {
        self.data.len()
    }

    /// Unpack the data as a typed message
    ///
    /// # Type Parameters
    /// * `M` - The message type to unpack
    ///
    /// # Returns
    /// The unpacked message if the message type matches and unpacking succeeds.
    ///
    /// # Errors
    /// - `InvalidMsgType` if the envelope's msgtype doesn't match `M::MSGTYPE`
    /// - Any error from `M::unpack()`
    ///
    /// # Example
    ///
    /// ```
    /// use leviculum_core::link::channel::{
    ///     Channel, ChannelError, Envelope, Message, ReceiveOutcome,
    /// };
    ///
    /// struct MyMessage { data: Vec<u8> }
    /// impl Message for MyMessage {
    ///     const MSGTYPE: u16 = 0x0001;
    ///     fn pack(&self) -> Vec<u8> { self.data.clone() }
    ///     fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
    ///         Ok(Self { data: data.to_vec() })
    ///     }
    /// }
    ///
    /// // Build envelope data via Channel
    /// let mut channel = Channel::new();
    /// let packet_data = channel.send(&MyMessage { data: vec![42] }, 400, 1000, 100).unwrap();
    ///
    /// // Receive envelope and unpack
    /// let mut receiver = Channel::new();
    /// let envelope = match receiver.receive(&packet_data, [0u8; 32]).unwrap() {
    ///     ReceiveOutcome::Delivered(env) => env,
    ///     _ => panic!("expected delivery"),
    /// };
    /// let msg: MyMessage = envelope.unpack_message().unwrap();
    /// assert_eq!(msg.data, vec![42]);
    /// ```
    pub fn unpack_message<M: super::Message>(&self) -> Result<M, ChannelError> {
        if self.msgtype != M::MSGTYPE {
            return Err(ChannelError::InvalidMsgType);
        }
        M::unpack(&self.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::channel::{ChannelError, Message};
    use alloc::vec;

    /// Test message type
    #[derive(Debug, PartialEq)]
    struct TestMessage {
        value: u32,
    }

    impl Message for TestMessage {
        const MSGTYPE: u16 = 0x1234;

        fn pack(&self) -> Vec<u8> {
            self.value.to_be_bytes().to_vec()
        }

        fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
            if data.len() < 4 {
                return Err(ChannelError::EnvelopeTruncated);
            }
            let value = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            Ok(Self { value })
        }
    }

    /// Different message type for mismatch testing
    struct OtherMessage;

    impl Message for OtherMessage {
        const MSGTYPE: u16 = 0x5678;

        fn pack(&self) -> Vec<u8> {
            Vec::new()
        }

        fn unpack(_data: &[u8]) -> Result<Self, ChannelError> {
            Ok(Self)
        }
    }

    #[test]
    fn test_envelope_pack_unpack_roundtrip() {
        let data = vec![0x01, 0x02, 0x03, 0x04];
        let envelope = Envelope::new(0x1234, 0x5678, data.clone());

        let packed = envelope.pack();
        assert_eq!(packed.len(), CHANNEL_ENVELOPE_HEADER_SIZE + 4);

        // Check header bytes
        assert_eq!(packed[0..2], [0x12, 0x34]); // msgtype BE
        assert_eq!(packed[2..4], [0x56, 0x78]); // sequence BE
        assert_eq!(packed[4..6], [0x00, 0x04]); // length BE
        assert_eq!(&packed[6..], &data); // data

        let unpacked = Envelope::unpack(&packed).unwrap();
        assert_eq!(unpacked.msgtype, 0x1234);
        assert_eq!(unpacked.sequence, 0x5678);
        assert_eq!(unpacked.data, data);
    }

    #[test]
    fn test_envelope_empty_data() {
        let envelope = Envelope::new(0x0001, 0x0000, Vec::new());
        let packed = envelope.pack();
        assert_eq!(packed.len(), CHANNEL_ENVELOPE_HEADER_SIZE);

        let unpacked = Envelope::unpack(&packed).unwrap();
        assert_eq!(unpacked.msgtype, 0x0001);
        assert_eq!(unpacked.sequence, 0x0000);
        assert!(unpacked.data.is_empty());
    }

    #[test]
    fn test_envelope_max_sequence() {
        let envelope = Envelope::new(0xefff, 0xffff, vec![0xAA]);
        let packed = envelope.pack();

        let unpacked = Envelope::unpack(&packed).unwrap();
        assert_eq!(unpacked.msgtype, 0xefff);
        assert_eq!(unpacked.sequence, 0xffff);
    }

    #[test]
    fn test_envelope_unpack_too_short() {
        let short = [0x00, 0x01, 0x00];
        assert_eq!(
            Envelope::unpack(&short),
            Err(ChannelError::EnvelopeTooShort)
        );
    }

    #[test]
    fn test_envelope_unpack_header_only() {
        // Valid header but declares 4 bytes of data with none present
        let header_only = [0x00, 0x01, 0x00, 0x02, 0x00, 0x04];
        assert_eq!(
            Envelope::unpack(&header_only),
            Err(ChannelError::EnvelopeTruncated)
        );
    }

    #[test]
    fn test_envelope_unpack_truncated_data() {
        // Header says 4 bytes but only 2 present
        let truncated = [0x00, 0x01, 0x00, 0x02, 0x00, 0x04, 0xAA, 0xBB];
        assert_eq!(
            Envelope::unpack(&truncated),
            Err(ChannelError::EnvelopeTruncated)
        );
    }

    #[test]
    #[should_panic(expected = "envelope data length")]
    fn test_envelope_data_too_large() {
        // Creating envelope with data > u16::MAX should panic
        let large_data = vec![0; 65536];
        Envelope::new(0x0001, 0x0000, large_data);
    }

    #[test]
    fn test_envelope_packed_size() {
        let envelope = Envelope::new(0x0001, 0x0000, vec![0; 100]);
        assert_eq!(envelope.packed_size(), CHANNEL_ENVELOPE_HEADER_SIZE + 100);
    }

    #[test]
    fn test_envelope_unpack_message_success() {
        // Create an envelope with TestMessage data
        let msg = TestMessage { value: 0x12345678 };
        let envelope = Envelope::new(TestMessage::MSGTYPE, 0, msg.pack());

        // Unpack the message
        let unpacked: TestMessage = envelope.unpack_message().unwrap();
        assert_eq!(unpacked.value, 0x12345678);
    }

    #[test]
    fn test_envelope_unpack_message_type_mismatch() {
        // Create an envelope with TestMessage data but wrong msgtype
        let msg = TestMessage { value: 42 };
        let envelope = Envelope::new(OtherMessage::MSGTYPE, 0, msg.pack());

        // Trying to unpack as TestMessage should fail
        let result: Result<TestMessage, _> = envelope.unpack_message();
        assert_eq!(result, Err(ChannelError::InvalidMsgType));
    }

    #[test]
    fn test_envelope_unpack_message_invalid_data() {
        // Create an envelope with too-short data for TestMessage
        let envelope = Envelope::new(TestMessage::MSGTYPE, 0, vec![0x01, 0x02]);

        // Trying to unpack should fail due to insufficient data
        let result: Result<TestMessage, _> = envelope.unpack_message();
        assert_eq!(result, Err(ChannelError::EnvelopeTruncated));
    }

    #[test]
    fn test_envelope_unpack_message_empty_message() {
        // OtherMessage accepts any data including empty
        let envelope = Envelope::new(OtherMessage::MSGTYPE, 0, Vec::new());

        let result: Result<OtherMessage, _> = envelope.unpack_message();
        assert!(result.is_ok());
    }

    /// #242: the fallible constructor refuses what the wire length
    /// field cannot carry; `new` remains available (and panics) for direct use.
    #[test]
    fn try_new_refuses_oversized_data() {
        assert!(Envelope::try_new(1, 0, vec![0u8; u16::MAX as usize]).is_some());
        assert!(Envelope::try_new(1, 0, vec![0u8; u16::MAX as usize + 1]).is_none());
    }
}
