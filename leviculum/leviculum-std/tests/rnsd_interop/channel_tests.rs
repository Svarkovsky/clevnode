//! Channel interoperability tests with Python Reticulum.
//!
//! These tests verify that our Rust Channel implementation produces wire-compatible
//! envelope formats with Python Reticulum's Channel implementation.
//!
//! ## What These Tests Verify
//!
//! 1. **Envelope format** - Wire format matches Python's struct.pack(">HHH", ...)
//! 2. **Sequence numbers** - 16-bit wraparound handling
//! 3. **Message types** - Reserved type validation (>= 0xf000)
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop channel_tests
//! ```

use leviculum_core::constants::{
    CHANNEL_ENVELOPE_HEADER_SIZE, CHANNEL_MSGTYPE_RESERVED, CHANNEL_WINDOW_INITIAL,
};
use leviculum_core::link::channel::{Channel, ChannelError, Envelope, Message, ReceiveOutcome};

// =========================================================================
// Test message types
// =========================================================================

/// Simple test message for interop testing
struct TestMessage {
    data: Vec<u8>,
}

impl TestMessage {
    fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl Message for TestMessage {
    const MSGTYPE: u16 = 0x0001;

    fn pack(&self) -> Vec<u8> {
        self.data.clone()
    }

    fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
        Ok(Self {
            data: data.to_vec(),
        })
    }
}

// =========================================================================
// Test 1: Envelope wire format matches Python
// =========================================================================

/// Verify envelope pack format matches Python's struct.pack(">HHH", msgtype, seq, len)
///
/// Python code from Channel.py:
/// ```python
/// self.raw = struct.pack(">HHH", self.message.MSGTYPE, self.sequence, len(data)) + data
/// ```
#[test]
fn test_envelope_wire_format_matches_python() {
    // Test case: MSGTYPE=0x1234, SEQUENCE=0x5678, DATA=b"\x01\x02\x03\x04"
    let msgtype: u16 = 0x1234;
    let sequence: u16 = 0x5678;
    let data = vec![0x01, 0x02, 0x03, 0x04];

    let envelope = Envelope::new(msgtype, sequence, data.clone());
    let packed = envelope.pack();

    // Expected format: [MSGTYPE BE] [SEQUENCE BE] [LENGTH BE] [DATA]
    // MSGTYPE: 0x1234 -> [0x12, 0x34]
    // SEQUENCE: 0x5678 -> [0x56, 0x78]
    // LENGTH: 4 -> [0x00, 0x04]
    // DATA: [0x01, 0x02, 0x03, 0x04]
    let expected = vec![
        0x12, 0x34, // msgtype big-endian
        0x56, 0x78, // sequence big-endian
        0x00, 0x04, // length big-endian
        0x01, 0x02, 0x03, 0x04, // data
    ];

    assert_eq!(packed, expected, "Envelope wire format should match Python");
}

/// Verify envelope unpack correctly parses Python-format data
#[test]
fn test_envelope_unpack_python_format() {
    // Data that Python would produce
    let python_data = vec![
        0x00, 0x01, // msgtype = 1
        0xFF, 0xFE, // sequence = 65534
        0x00, 0x05, // length = 5
        b'h', b'e', b'l', b'l', b'o', // data = "hello"
    ];

    let envelope = Envelope::unpack(&python_data).expect("Should parse Python format");

    assert_eq!(envelope.msgtype, 0x0001);
    assert_eq!(envelope.sequence, 0xFFFE);
    assert_eq!(envelope.length(), 5);
    assert_eq!(envelope.data, b"hello");
}

// =========================================================================
// Test 2: Sequence number handling
// =========================================================================

/// Verify 16-bit sequence number wraparound (SEQ_MODULUS = 0x10000)
#[test]
fn test_sequence_wraparound() {
    let mut channel = Channel::new();

    // Advance to near max
    for _ in 0..0xFFFE {
        let _ = channel.send(&TestMessage::new(vec![1]), 464, 0, 100);
        // Clear tx_ring to allow more sends
        channel.clear_tx();
    }

    // Should be at sequence 0xFFFE
    assert_eq!(channel.next_tx_sequence(), 0xFFFE);

    // Send one more - should use 0xFFFE and advance to 0xFFFF
    let _ = channel.send(&TestMessage::new(vec![1]), 464, 0, 100);
    channel.clear_tx();
    assert_eq!(channel.next_tx_sequence(), 0xFFFF);

    // Send one more - should use 0xFFFF and wrap to 0x0000
    let _ = channel.send(&TestMessage::new(vec![1]), 464, 0, 100);
    channel.clear_tx();
    assert_eq!(channel.next_tx_sequence(), 0x0000);

    // Send one more - should use 0x0000 and advance to 0x0001
    let _ = channel.send(&TestMessage::new(vec![1]), 464, 0, 100);
    channel.clear_tx();
    assert_eq!(channel.next_tx_sequence(), 0x0001);
}

// =========================================================================
// Test 3: Reserved message type validation
// =========================================================================

/// Verify message types >= 0xf000 are rejected (reserved for system use)
#[test]
fn test_reserved_msgtype_rejected() {
    struct ReservedMessage;

    impl Message for ReservedMessage {
        const MSGTYPE: u16 = 0xf000; // First reserved type

        fn pack(&self) -> Vec<u8> {
            Vec::new()
        }

        fn unpack(_: &[u8]) -> Result<Self, ChannelError> {
            Ok(Self)
        }
    }

    let mut channel = Channel::new();
    let result = channel.send(&ReservedMessage, 464, 0, 100);

    assert_eq!(
        result,
        Err(ChannelError::InvalidMsgType),
        "Reserved MSGTYPE should be rejected"
    );
}

/// Verify 0xefff (just below reserved) is allowed
#[test]
fn test_max_user_msgtype_allowed() {
    struct MaxUserMessage;

    impl Message for MaxUserMessage {
        const MSGTYPE: u16 = 0xefff; // Maximum user type

        fn pack(&self) -> Vec<u8> {
            Vec::new()
        }

        fn unpack(_: &[u8]) -> Result<Self, ChannelError> {
            Ok(Self)
        }
    }

    let mut channel = Channel::new();
    let result = channel.send(&MaxUserMessage, 464, 0, 100);

    assert!(result.is_ok(), "MSGTYPE 0xefff should be allowed");
}

// =========================================================================
// Test 4: Channel constants match Python
// =========================================================================

/// Verify our constants match Python's Channel class constants
#[test]
fn test_channel_constants_match_python() {
    // Python: WINDOW = 2
    assert_eq!(CHANNEL_WINDOW_INITIAL, 2, "WINDOW should match Python");

    // Python: MSGTYPE >= 0xf000 is reserved
    assert_eq!(
        CHANNEL_MSGTYPE_RESERVED, 0xf000,
        "Reserved boundary should match Python"
    );

    // Python: Header is 6 bytes (struct.pack(">HHH", ...))
    assert_eq!(
        CHANNEL_ENVELOPE_HEADER_SIZE, 6,
        "Envelope header size should match Python"
    );
}

// =========================================================================
// Test 5: Receive out-of-order handling
// =========================================================================

/// Verify out-of-order reception and draining works correctly
#[test]
fn test_out_of_order_reception() {
    let mut channel = Channel::new();

    // Receive sequence 2 first (out of order)
    let env2 = Envelope::new(0x0001, 2, vec![3]);
    let result = channel.receive(&env2.pack(), [0u8; 32]);
    assert_eq!(result, Ok(ReceiveOutcome::Buffered));

    // Receive sequence 1 (still out of order)
    let env1 = Envelope::new(0x0001, 1, vec![2]);
    let result = channel.receive(&env1.pack(), [0u8; 32]);
    assert_eq!(result, Ok(ReceiveOutcome::Buffered));

    // Receive sequence 0 (in order)
    let env0 = Envelope::new(0x0001, 0, vec![1]);
    let result = channel.receive(&env0.pack(), [0u8; 32]);
    match result.unwrap() {
        ReceiveOutcome::Delivered(env) => assert_eq!(env.sequence, 0),
        other => panic!("expected Delivered, got {:?}", other),
    }

    // Drain should give us 1 and 2 in order
    let drained = channel.drain_received();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].0.sequence, 1);
    assert_eq!(drained[1].0.sequence, 2);
}

// =========================================================================
// Test 6: MDU calculation
// =========================================================================

/// Verify MDU calculation matches Python (link_mdu - 6)
#[test]
fn test_mdu_calculation() {
    let channel = Channel::new();

    // Python: channel.mdu = link.mdu - 6 (envelope header)
    let link_mdu = 464;
    let expected_channel_mdu = 464 - 6;

    assert_eq!(
        channel.mdu(link_mdu),
        expected_channel_mdu,
        "Channel MDU should be link MDU minus envelope header"
    );
}

// =========================================================================
// Test 7: Window management
// =========================================================================

/// Verify initial window state
#[test]
fn test_initial_window_state() {
    let channel = Channel::new();

    assert_eq!(channel.window(), 2, "Initial window should be 2");
    assert!(
        channel.is_ready_to_send(),
        "Should be ready to send initially"
    );
    assert_eq!(
        channel.outstanding(),
        0,
        "No outstanding messages initially"
    );
}

/// Verify window blocks sends when full
#[test]
fn test_window_blocking() {
    let mut channel = Channel::new();
    // Set small window for testing
    channel.update_window_for_rtt(2000); // Slow link -> window_max = 5

    // Fill the window
    for i in 0..channel.window() {
        let msg = TestMessage::new(vec![i as u8]);
        let result = channel.send(&msg, 464, 0, 100);
        assert!(result.is_ok(), "Send {} should succeed", i);
    }

    // Window should be full
    assert!(
        !channel.is_ready_to_send(),
        "Should not be ready when window full"
    );

    // Next send should fail
    let msg = TestMessage::new(vec![99]);
    let result = channel.send(&msg, 464, 0, 100);
    assert_eq!(result, Err(ChannelError::Busy));
}

// =========================================================================
// Test 8: Symmetric send/receive API
// =========================================================================

/// Verify symmetric send<M>/receive_message<M> API works correctly
#[test]
fn test_symmetric_send_receive_api() {
    let mut sender = Channel::new();
    let mut receiver = Channel::new();

    // Send a typed message
    let msg = TestMessage::new(vec![10, 20, 30, 40]);
    let packed = sender
        .send(&msg, 464, 1000, 100)
        .expect("Send should succeed");

    // Receive using symmetric typed API
    let received: Option<TestMessage> = receiver
        .receive_message(&packed)
        .expect("Receive should succeed");

    assert!(received.is_some(), "Should receive the message");
    let received_msg = received.unwrap();
    assert_eq!(received_msg.data, vec![10, 20, 30, 40]);
}

/// Verify receive_message returns None for type mismatch
#[test]
fn test_receive_message_type_mismatch() {
    struct OtherMessage {
        value: u16,
    }

    impl Message for OtherMessage {
        const MSGTYPE: u16 = 0x9999;

        fn pack(&self) -> Vec<u8> {
            self.value.to_be_bytes().to_vec()
        }

        fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
            if data.len() < 2 {
                return Err(ChannelError::EnvelopeTruncated);
            }
            Ok(Self {
                value: u16::from_be_bytes([data[0], data[1]]),
            })
        }
    }

    let mut sender = Channel::new();
    let mut receiver = Channel::new();

    // Send OtherMessage
    let msg = OtherMessage { value: 1234 };
    let packed = sender
        .send(&msg, 464, 1000, 100)
        .expect("Send should succeed");

    // Try to receive as TestMessage - should return None
    let result: Option<TestMessage> = receiver
        .receive_message(&packed)
        .expect("Receive should not error");

    assert!(result.is_none(), "Should return None for type mismatch");
}

/// Verify envelope unpack_message works correctly
#[test]
fn test_envelope_unpack_message() {
    // Create an envelope with TestMessage data
    let msg = TestMessage::new(vec![1, 2, 3]);
    let envelope = Envelope::new(TestMessage::MSGTYPE, 0, msg.pack());

    // Unpack using typed API
    let unpacked: TestMessage = envelope
        .unpack_message()
        .expect("Should unpack successfully");
    assert_eq!(unpacked.data, vec![1, 2, 3]);
}
