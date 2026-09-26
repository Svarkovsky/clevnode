//! Channel system for reliable, bidirectional message delivery over Links
//!
//! Channels provide:
//! - Reliability (automatic retries)
//! - Ordering (16-bit sequence numbers)
//! - Message framing (typed messages with registration)
//! - Flow control (window-based congestion control)
//!
//! # Wire Format
//!
//! ```text
//! [MSGTYPE: 2 bytes BE] [SEQUENCE: 2 bytes BE] [LENGTH: 2 bytes BE] [DATA: variable]
//! ```
//!
//! # Example
//!
//! ```
//! use leviculum_core::link::channel::{Channel, Message, ChannelError};
//!
//! // Define a custom message type
//! struct MyMessage { data: Vec<u8> }
//!
//! impl Message for MyMessage {
//!     const MSGTYPE: u16 = 0x0001;
//!     fn pack(&self) -> Vec<u8> { self.data.clone() }
//!     fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
//!         Ok(Self { data: data.to_vec() })
//!     }
//! }
//!
//! // Create channel and send message
//! let mut channel = Channel::new();
//! let msg = MyMessage { data: vec![1, 2, 3] };
//! let packet_data = channel.send(&msg, 400, 1000, 100).unwrap();
//! assert!(!packet_data.is_empty());
//! ```

mod buffer;
mod envelope;
mod error;
mod stream;

pub use buffer::{max_data_len, stream_overhead, ReadResult, MAX_CHUNK_LEN};
#[cfg(feature = "compression")]
pub use buffer::{CompressingWriter, COMPRESSION_MIN_SIZE, COMPRESSION_TRIES, MAX_DECOMPRESS_SIZE};
pub use envelope::Envelope;
pub use error::ChannelError;
pub use stream::StreamDataMessage;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::constants::{
    CHANNEL_BACKOFF_BASE, CHANNEL_ENVELOPE_HEADER_SIZE, CHANNEL_MAX_TRIES,
    CHANNEL_MIN_TIMEOUT_BASE_MS, CHANNEL_MSGTYPE_RESERVED, CHANNEL_RTT_FAST_MS,
    CHANNEL_RTT_MEDIUM_MS, CHANNEL_RTT_TIMEOUT_MULTIPLIER, CHANNEL_RTT_VERY_SLOW_MS,
    CHANNEL_RX_RING_MAX, CHANNEL_SEQ_MODULUS, CHANNEL_WINDOW_INITIAL, CHANNEL_WINDOW_MAX_FAST,
    CHANNEL_WINDOW_MAX_MEDIUM, CHANNEL_WINDOW_MAX_SLOW, CHANNEL_WINDOW_MAX_VERY_SLOW,
    CHANNEL_WINDOW_MIN_FAST, CHANNEL_WINDOW_MIN_SLOW, CHANNEL_WINDOW_MIN_VERY_SLOW,
};

/// Calculate f64 power for small non-negative integer exponents
///
/// This is a no_std compatible implementation of `f64::powf()`. In no_std
/// environments, the standard library's `f64::powf()` is not available
/// because it relies on the system's math library (libm).
///
/// For the channel timeout formula, exponents are always small non-negative
/// integers (typically 0-5), so simple iterative multiplication is sufficient
/// and more efficient than a general power function.
///
/// # Arguments
/// * `base` - The base value
/// * `exp` - The exponent (must be a small non-negative integer)
///
/// # Example
/// ```text
/// pow_f64(1.5, 0) == 1.0
/// pow_f64(1.5, 1) == 1.5
/// pow_f64(1.5, 2) == 2.25
/// ```
fn pow_f64(base: f64, exp: u32) -> f64 {
    let mut result = 1.0;
    for _ in 0..exp {
        result *= base;
    }
    result
}

/// Calculate retransmit timeout with additive queue delay.
///
/// Formula: (base_rto + queue_delay) * BACKOFF_BASE^(tries-1)
///   where base_rto = max(rtt * RTT_MULTIPLIER, MIN_TIMEOUT)
///         queue_delay = queue_len * (rtt / window)
///
/// The queue delay estimates how long this packet must wait behind
/// earlier packets in the tx_ring, using SRTT/window as inter-delivery time.
fn calculate_timeout(tries: u8, rtt_ms: u64, queue_len: usize, window: usize) -> u64 {
    let base_rto =
        (rtt_ms as f64 * CHANNEL_RTT_TIMEOUT_MULTIPLIER).max(CHANNEL_MIN_TIMEOUT_BASE_MS);
    let retry_factor = pow_f64(CHANNEL_BACKOFF_BASE, tries.saturating_sub(1) as u32);
    let w = (window as f64).max(1.0);
    let queue_delay = queue_len as f64 * (rtt_ms as f64 / w);

    ((base_rto + queue_delay) * retry_factor) as u64
}

/// Message trait - all channel messages must implement this
pub trait Message: Sized {
    /// Unique message type identifier (< 0xf000)
    const MSGTYPE: u16;

    /// Pack message into bytes
    fn pack(&self) -> Vec<u8>;

    /// Unpack message from bytes
    fn unpack(data: &[u8]) -> Result<Self, ChannelError>;
}

/// Message delivery state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageState {
    /// Message created but not yet sent
    New,
    /// Message sent, awaiting acknowledgment
    Sent,
    /// Message delivery confirmed
    Delivered,
    /// Message delivery failed (max retries exceeded)
    Failed,
}

/// Outbound envelope tracking
#[derive(Debug)]
struct OutboundEnvelope {
    /// The envelope data
    envelope: Envelope,
    /// Current delivery state
    state: MessageState,
    /// Number of transmission attempts
    tries: u8,
    /// When the envelope was last sent (milliseconds)
    sent_at_ms: u64,
    /// tx_ring length at the time this envelope was first sent.
    /// Used for timeout calculation to avoid death-spiral where growing
    /// queue inflates timeouts during drain.
    queue_len_at_send: usize,
}

impl OutboundEnvelope {
    fn new(envelope: Envelope) -> Self {
        Self {
            envelope,
            state: MessageState::New,
            tries: 0,
            sent_at_ms: 0,
            queue_len_at_send: 0,
        }
    }
}

/// Actions that may need to be taken after polling
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelAction {
    /// Retransmit this packet data
    Retransmit {
        /// Sequence number of the envelope
        sequence: u16,
        /// Packed envelope data to transmit
        data: Vec<u8>,
        /// Which attempt this is (2 = first retry, 3 = second, etc.)
        tries: u8,
    },
    /// Link should be torn down (max retries exceeded)
    TearDownLink,
}

/// Outcome of receiving a channel message, determines proof behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiveOutcome {
    /// Message delivered in-order. Caller should send proof and emit event.
    Delivered(Envelope),
    /// Out-of-order, buffered in rx_ring. Do NOT send proof yet.
    Buffered,
    /// Duplicate of already-delivered message (seq < next_expected).
    /// Caller should send proof so sender stops retransmitting.
    AlreadyDelivered,
    /// Duplicate of message already in rx_ring buffer.
    /// Do NOT send proof, the gap is still open.
    DuplicateBuffered,
}

/// Channel for reliable message delivery over a Link
#[derive(Debug)]
pub struct Channel {
    /// Next sequence number to use for outgoing messages
    next_tx_sequence: u16,
    /// Next expected sequence number for incoming messages
    next_rx_sequence: u16,
    /// Outbound envelopes awaiting acknowledgment
    tx_ring: VecDeque<OutboundEnvelope>,
    /// Inbound envelopes received out of order, with proof hash for deferred proving.
    /// The `[u8; 32]` is the packet hash needed to build a proof when the message is
    /// eventually drained and delivered in-order to the application.
    rx_ring: VecDeque<Option<(Envelope, [u8; 32])>>,
    /// Current window size
    window: usize,
    /// Minimum window size
    window_min: usize,
    /// Maximum window size
    window_max: usize,
    /// Maximum transmission attempts
    max_tries: u8,
    /// Current pacing interval between sends (milliseconds).
    /// Base: rtt_ms / window. Adjusted by AIMD.
    pacing_interval_ms: u64,
    /// Earliest allowed time for the next send (milliseconds).
    next_send_at_ms: u64,
    /// Smoothed round-trip time (f64, milliseconds). 0.0 = not yet measured.
    srtt_ms: f64,
    /// RTT variance (f64, milliseconds). 0.0 = not yet measured.
    rttvar_ms: f64,
    /// Minimum RTT observed across Karn-valid delivery samples (milliseconds).
    /// `None` until the first sample. Telemetry only (leviculum#35) — never
    /// consulted by window/pacing logic.
    min_rtt_ms: Option<u64>,
    /// Cumulative bytes confirmed delivered on this channel: the packed size
    /// of every envelope removed from `tx_ring` by a delivery proof. The
    /// BBR-style delivery-rate numerator (leviculum#35); callers sample this
    /// counter periodically and difference it.
    delivered_bytes: u64,
    /// Cumulative sends rejected because the window was full
    /// (`ChannelError::Busy`) — the congestion-limited signal (leviculum#35).
    busy_rejections: u64,
    /// Cumulative sends rejected by the link pacer
    /// (`ChannelError::PacingDelay`) — backpressure, but from pacing rather
    /// than a full window (leviculum#35).
    pacing_rejections: u64,
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel {
    /// Create a new channel with default settings
    pub fn new() -> Self {
        Self {
            next_tx_sequence: 0,
            next_rx_sequence: 0,
            tx_ring: VecDeque::new(),
            rx_ring: VecDeque::new(),
            window: CHANNEL_WINDOW_INITIAL,
            window_min: CHANNEL_WINDOW_MIN_SLOW,
            window_max: CHANNEL_WINDOW_MAX_SLOW,
            max_tries: CHANNEL_MAX_TRIES,
            pacing_interval_ms: 0,
            next_send_at_ms: 0,
            srtt_ms: 0.0,
            rttvar_ms: 0.0,
            min_rtt_ms: None,
            delivered_bytes: 0,
            busy_rejections: 0,
            pacing_rejections: 0,
        }
    }

    /// Check if the channel is ready to send more messages
    ///
    /// Returns false if the send window is full.
    pub fn is_ready_to_send(&self) -> bool {
        self.tx_ring.len() < self.window
    }

    /// Get the maximum data unit for channel messages
    ///
    /// This is the link MDU minus the envelope header size, capped at the
    /// envelope's own hard ceiling: the wire length field is `u16`, so no
    /// channel payload can exceed `u16::MAX` bytes regardless of how large an
    /// MTU the link negotiated (#242 — an uncapped value let an
    /// oversized send pass the `TooLarge` guard and reach the `Envelope`
    /// length assert as a panic).
    pub fn mdu(&self, link_mdu: usize) -> usize {
        link_mdu
            .saturating_sub(CHANNEL_ENVELOPE_HEADER_SIZE)
            .min(u16::MAX as usize)
    }

    /// Get the current window size
    pub fn window(&self) -> usize {
        self.window
    }

    /// Get the maximum window size
    pub fn window_max(&self) -> usize {
        self.window_max
    }

    /// Get the number of outstanding (unacknowledged) messages
    pub fn outstanding(&self) -> usize {
        self.tx_ring.len()
    }

    /// Get the current pacing interval in milliseconds
    pub fn pacing_interval_ms(&self) -> u64 {
        self.pacing_interval_ms
    }

    /// Get the earliest allowed time for the next send
    pub fn next_send_at_ms(&self) -> u64 {
        self.next_send_at_ms
    }

    /// Get the next transmit sequence number (without incrementing)
    pub fn next_tx_sequence(&self) -> u16 {
        self.next_tx_sequence
    }

    /// Get the next expected receive sequence number
    pub fn next_rx_sequence(&self) -> u16 {
        self.next_rx_sequence
    }

    /// Get the sequence number of the most recently sent message
    ///
    /// This wraps around correctly at the sequence modulus boundary.
    pub fn last_sent_sequence(&self) -> u16 {
        if self.next_tx_sequence == 0 {
            (CHANNEL_SEQ_MODULUS - 1) as u16
        } else {
            self.next_tx_sequence - 1
        }
    }

    /// Get the smoothed RTT in milliseconds (0.0 if not yet measured)
    pub fn srtt_ms(&self) -> f64 {
        self.srtt_ms
    }

    /// Get the RTT variance in milliseconds (0.0 if not yet measured)
    pub fn rttvar_ms(&self) -> f64 {
        self.rttvar_ms
    }

    /// Minimum Karn-valid delivery RTT observed, in milliseconds (leviculum#35).
    pub fn min_rtt_ms(&self) -> Option<u64> {
        self.min_rtt_ms
    }

    /// Cumulative confirmed-delivered envelope bytes (leviculum#35).
    pub fn delivered_bytes(&self) -> u64 {
        self.delivered_bytes
    }

    /// Cumulative window-full send rejections (leviculum#35).
    pub fn busy_rejections(&self) -> u64 {
        self.busy_rejections
    }

    /// Cumulative pacing send rejections (leviculum#35).
    pub fn pacing_rejections(&self) -> u64 {
        self.pacing_rejections
    }

    /// Check if the receive ring is empty
    pub fn rx_ring_is_empty(&self) -> bool {
        self.rx_ring.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn set_window_for_test(&mut self, w: usize) {
        self.window = w;
    }

    #[cfg(test)]
    pub(crate) fn set_max_tries_for_test(&mut self, t: u8) {
        self.max_tries = t;
    }

    #[cfg(test)]
    pub(crate) fn set_pacing_for_test(&mut self, interval_ms: u64, next_send_at_ms: u64) {
        self.pacing_interval_ms = interval_ms;
        self.next_send_at_ms = next_send_at_ms;
    }

    #[cfg(test)]
    pub(crate) fn set_window_bounds_for_test(&mut self, window: usize, min: usize, max: usize) {
        self.window = window;
        self.window_min = min;
        self.window_max = max;
    }

    #[cfg(test)]
    pub(crate) fn set_next_rx_sequence_for_test(&mut self, seq: u16) {
        self.next_rx_sequence = seq;
    }

    /// Seed SRTT from the handshake RTT (RFC 6298 §2.2).
    ///
    /// Treats the handshake measurement as the first RTT sample so that
    /// the very first channel timeout uses `handshake_rtt * 2.5` instead
    /// of falling back to the raw handshake value. Subsequent ACK samples
    /// refine through the normal EWMA path in `update_srtt`.
    pub fn seed_srtt(&mut self, rtt_ms: u64) {
        if rtt_ms > 0 {
            self.srtt_ms = rtt_ms as f64;
            self.rttvar_ms = rtt_ms as f64 / 2.0;
        }
    }

    /// Update SRTT from a sample RTT measurement (RFC 6298).
    fn update_srtt(&mut self, sample_ms: u64) {
        let sample = sample_ms as f64;
        if self.srtt_ms == 0.0 {
            // First measurement
            self.srtt_ms = sample;
            self.rttvar_ms = sample / 2.0;
        } else {
            // RTTVAR = (1-beta)*RTTVAR + beta*|SRTT - R|, beta = 1/4
            self.rttvar_ms = 0.75 * self.rttvar_ms + 0.25 * (self.srtt_ms - sample).abs();
            // SRTT = (1-alpha)*SRTT + alpha*R, alpha = 1/8
            self.srtt_ms = 0.875 * self.srtt_ms + 0.125 * sample;
        }
        crate::tracing::debug!(
            sample_ms,
            srtt = self.srtt_ms,
            rttvar = self.rttvar_ms,
            "channel: srtt updated"
        );
    }

    /// Return the effective RTT: SRTT if measured, else fallback.
    fn effective_rtt_ms(&self, fallback_rtt_ms: u64) -> u64 {
        if self.srtt_ms > 0.0 {
            self.srtt_ms as u64
        } else {
            fallback_rtt_ms
        }
    }

    /// Update window limits based on RTT
    ///
    /// Faster links get larger windows for better throughput.
    pub fn update_window_for_rtt(&mut self, rtt_ms: u64) {
        if rtt_ms < CHANNEL_RTT_FAST_MS {
            self.window_min = CHANNEL_WINDOW_MIN_FAST;
            self.window_max = CHANNEL_WINDOW_MAX_FAST;
        } else if rtt_ms < CHANNEL_RTT_MEDIUM_MS {
            self.window_min = CHANNEL_WINDOW_MIN_SLOW;
            self.window_max = CHANNEL_WINDOW_MAX_MEDIUM;
        } else if rtt_ms < CHANNEL_RTT_VERY_SLOW_MS {
            self.window_min = CHANNEL_WINDOW_MIN_SLOW;
            self.window_max = CHANNEL_WINDOW_MAX_SLOW;
        } else {
            self.window_min = CHANNEL_WINDOW_MIN_VERY_SLOW;
            self.window_max = CHANNEL_WINDOW_MAX_VERY_SLOW;
            crate::tracing::debug!(rtt_ms, "channel: VERY_SLOW tier (RTT >= 2s), window_max=3");
        }
        // Clamp current window to new limits
        self.window = self.window.clamp(self.window_min, self.window_max);
    }

    /// Get the next sequence number and increment the counter
    fn next_sequence(&mut self) -> u16 {
        let seq = self.next_tx_sequence;
        self.next_tx_sequence = ((self.next_tx_sequence as u32 + 1) % CHANNEL_SEQ_MODULUS) as u16;
        seq
    }

    /// Validate a message type
    fn validate_msgtype(msgtype: u16) -> Result<(), ChannelError> {
        if msgtype >= CHANNEL_MSGTYPE_RESERVED {
            return Err(ChannelError::InvalidMsgType);
        }
        Ok(())
    }

    /// Send a message on the channel
    ///
    /// Returns the packed envelope data to be encrypted and sent over the link.
    ///
    /// # Arguments
    /// * `message` - The message to send
    /// * `link_mdu` - Maximum data unit for the link
    /// * `now_ms` - Current time in milliseconds
    /// * `rtt_ms` - Round-trip time in milliseconds
    ///
    /// # Errors
    /// - `InvalidMsgType` if the message type is reserved (>= 0xf000)
    /// - `TooLarge` if the message exceeds the channel MDU
    /// - `Busy` if the send window is full
    pub fn send<M: Message>(
        &mut self,
        message: &M,
        link_mdu: usize,
        now_ms: u64,
        rtt_ms: u64,
    ) -> Result<Vec<u8>, ChannelError> {
        Self::validate_msgtype(M::MSGTYPE)?;
        self.send_raw(M::MSGTYPE, &message.pack(), link_mdu, now_ms, rtt_ms)
    }

    /// Send a system message on the channel (bypasses MSGTYPE validation)
    ///
    /// This method is for system messages (MSGTYPE >= 0xf000) like StreamDataMessage.
    /// Unlike `send()`, it does not validate that the message type is in the user range.
    ///
    /// # Arguments
    /// * `message` - The system message to send
    /// * `link_mdu` - Maximum data unit for the link
    /// * `now_ms` - Current time in milliseconds
    /// * `rtt_ms` - Round-trip time in milliseconds
    ///
    /// # Errors
    /// - `TooLarge` if the message exceeds the channel MDU
    /// - `Busy` if the send window is full
    pub fn send_system<M: Message>(
        &mut self,
        message: &M,
        link_mdu: usize,
        now_ms: u64,
        rtt_ms: u64,
    ) -> Result<Vec<u8>, ChannelError> {
        self.send_raw(M::MSGTYPE, &message.pack(), link_mdu, now_ms, rtt_ms)
    }

    /// Low-level send without MSGTYPE validation
    ///
    /// Called by `send()` (which validates MSGTYPE) and `send_system()` (which
    /// skips validation for system messages like StreamDataMessage).
    fn send_raw(
        &mut self,
        msgtype: u16,
        data: &[u8],
        link_mdu: usize,
        now_ms: u64,
        rtt_ms: u64,
    ) -> Result<Vec<u8>, ChannelError> {
        let mdu = self.mdu(link_mdu);

        if data.len() > mdu {
            return Err(ChannelError::TooLarge);
        }

        // 1. Window check FIRST, no point pacing if there's no slot
        if !self.is_ready_to_send() {
            crate::tracing::debug!(
                tx_ring = self.tx_ring.len(),
                window = self.window,
                window_max = self.window_max,
                "channel: busy — send rejected"
            );
            self.busy_rejections = self.busy_rejections.saturating_add(1);
            return Err(ChannelError::Busy);
        }
        // 2. Pacing check SECOND, only when window has room
        if now_ms < self.next_send_at_ms {
            self.pacing_rejections = self.pacing_rejections.saturating_add(1);
            return Err(ChannelError::PacingDelay {
                ready_at_ms: self.next_send_at_ms,
            });
        }

        let sequence = self.next_sequence();
        // Unreachable while the mdu() cap holds (checked above), but a length
        // that slipped through must surface as TooLarge, never as the
        // Envelope assert panicking a lock-holding thread (#242).
        let envelope =
            Envelope::try_new(msgtype, sequence, data.to_vec()).ok_or(ChannelError::TooLarge)?;
        let packed = envelope.pack();

        let mut outbound = OutboundEnvelope::new(envelope);
        outbound.state = MessageState::Sent;
        outbound.tries = 1;
        outbound.sent_at_ms = now_ms;
        outbound.queue_len_at_send = self.tx_ring.len() + 1; // +1 for this entry

        self.tx_ring.push_back(outbound);
        self.next_send_at_ms = now_ms.saturating_add(self.pacing_interval_ms);

        let queue_len = self.tx_ring.len();
        let timeout_ms = calculate_timeout(1, rtt_ms, queue_len, self.window); // for logging only
        crate::tracing::debug!(
            seq = sequence,
            tx_ring = queue_len,
            window = self.window,
            window_max = self.window_max,
            timeout_ms,
            pacing_ms = self.pacing_interval_ms,
            "channel: sent"
        );

        Ok(packed)
    }

    /// Receive decrypted channel data from link
    ///
    /// Returns a `ReceiveOutcome` indicating what happened:
    /// - `Delivered`: in-order message, caller should prove and emit event
    /// - `Buffered`: out-of-order, stored for later delivery, do NOT prove yet
    /// - `AlreadyDelivered`: duplicate of delivered message, re-prove so sender clears retransmit
    /// - `DuplicateBuffered`: duplicate of buffered message, do NOT prove
    ///
    /// # Arguments
    /// * `data` - Decrypted envelope data from the link
    /// * `proof_hash` - 32-byte packet hash for deferred proof generation
    pub fn receive(
        &mut self,
        data: &[u8],
        proof_hash: [u8; 32],
    ) -> Result<ReceiveOutcome, ChannelError> {
        let envelope = Envelope::unpack(data)?;

        // Check if this is the next expected sequence
        if envelope.sequence == self.next_rx_sequence {
            self.next_rx_sequence =
                ((self.next_rx_sequence as u32 + 1) % CHANNEL_SEQ_MODULUS) as u16;

            // Pop placeholder from front of rx_ring if present (we just consumed this slot)
            if !self.rx_ring.is_empty() {
                self.rx_ring.pop_front();
            }

            return Ok(ReceiveOutcome::Delivered(envelope));
        }

        // Calculate the offset from expected sequence (handling wraparound)
        let offset = self.sequence_offset(envelope.sequence);

        // Backward sequence (retransmit of already-delivered message).
        // sequence_offset() wraps around for backward sequences, producing
        // values in the upper half of the sequence space (>= 32768).
        // Return AlreadyDelivered so the caller generates a proof, the original
        // proof was likely lost on the return path.
        if offset >= CHANNEL_SEQ_MODULUS as usize / 2 {
            crate::tracing::debug!(
                seq = envelope.sequence,
                next_rx = self.next_rx_sequence,
                "channel: duplicate/backward sequence, re-proving"
            );
            return Ok(ReceiveOutcome::AlreadyDelivered);
        }

        // If offset is too large, the rx_ring is full
        if offset >= CHANNEL_RX_RING_MAX {
            return Err(ChannelError::RxRingFull);
        }

        // Buffer the out-of-order envelope with proof hash for deferred proving
        while self.rx_ring.len() <= offset {
            self.rx_ring.push_back(None);
        }

        // Check if slot is already occupied (duplicate of buffered message)
        let is_duplicate = self.rx_ring[offset].is_some();
        self.rx_ring[offset] = Some((envelope, proof_hash));

        if is_duplicate {
            Ok(ReceiveOutcome::DuplicateBuffered)
        } else {
            Ok(ReceiveOutcome::Buffered)
        }
    }

    /// Calculate sequence offset accounting for wraparound
    fn sequence_offset(&self, sequence: u16) -> usize {
        let expected = self.next_rx_sequence as u32;
        let received = sequence as u32;

        if received >= expected {
            (received - expected) as usize
        } else {
            // Wraparound case
            (CHANNEL_SEQ_MODULUS - expected + received) as usize
        }
    }

    /// Receive and unpack a typed message from the channel
    ///
    /// This is the symmetric counterpart to `send<M>()`. It receives an envelope
    /// and unpacks it into the specified message type.
    ///
    /// # Arguments
    /// * `data` - Decrypted envelope data from the link
    ///
    /// # Returns
    /// - `Ok(Some(msg))` if a message was received and unpacked in sequence
    /// - `Ok(None)` if the message was buffered (out of order) or invalid type
    /// - `Err(...)` if unpacking failed
    ///
    /// # Example
    ///
    /// ```
    /// use leviculum_core::link::channel::{Channel, Message, ChannelError};
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
    /// // Sender
    /// let mut sender = Channel::new();
    /// let packet_data = sender.send(&MyMessage { data: vec![1, 2, 3] }, 400, 1000, 100).unwrap();
    ///
    /// // Receiver
    /// let mut receiver = Channel::new();
    /// if let Some(msg) = receiver.receive_message::<MyMessage>(&packet_data).unwrap() {
    ///     assert_eq!(msg.data, vec![1, 2, 3]);
    /// }
    /// ```
    pub fn receive_message<M: Message>(&mut self, data: &[u8]) -> Result<Option<M>, ChannelError> {
        // First receive the envelope (test convenience, no real proof hash needed)
        let envelope = match self.receive(data, [0u8; 32])? {
            ReceiveOutcome::Delivered(env) => env,
            _ => return Ok(None), // Buffered, duplicate, or already delivered
        };

        // Check message type
        if envelope.msgtype != M::MSGTYPE {
            // Wrong type - this could be a different message type
            // We've already consumed it from the sequence, so return None
            return Ok(None);
        }

        // Unpack the message
        let msg = M::unpack(&envelope.data)?;
        Ok(Some(msg))
    }

    /// Drain received messages that are ready (in sequence order)
    ///
    /// Call this after `receive()` to get any buffered messages that
    /// are now in sequence. Returns `(Envelope, proof_hash)` tuples so
    /// the caller can build deferred proofs for each delivered message.
    pub fn drain_received(&mut self) -> Vec<(Envelope, [u8; 32])> {
        let mut ready = Vec::new();

        while let Some(front) = self.rx_ring.front() {
            match front {
                Some((envelope, _hash)) if envelope.sequence == self.next_rx_sequence => {
                    if let Some(Some((env, hash))) = self.rx_ring.pop_front() {
                        self.next_rx_sequence =
                            ((self.next_rx_sequence as u32 + 1) % CHANNEL_SEQ_MODULUS) as u16;
                        ready.push((env, hash));
                    }
                }
                None => {
                    // Placeholder for a message we haven't received yet - stop draining
                    break;
                }
                Some(_) => {
                    // Message with wrong sequence (shouldn't happen with proper buffering)
                    break;
                }
            }
        }

        ready
    }

    /// Mark an envelope as delivered (ACK received)
    ///
    /// This removes the envelope from the TX ring, updates SRTT using
    /// Karn's algorithm (non-retransmitted messages only), and adjusts
    /// the window.
    ///
    /// # Arguments
    /// * `sequence` - Sequence number of the delivered envelope
    /// * `now_ms` - Current time in milliseconds (for SRTT measurement)
    /// * `rtt_ms` - Handshake RTT in milliseconds (fallback for window adjustment)
    ///
    /// # Returns
    /// `true` if the ACK was for a known pending envelope, `false` otherwise
    /// (e.g., duplicate ACK or ACK for unknown sequence).
    pub fn mark_delivered(&mut self, sequence: u16, now_ms: u64, rtt_ms: u64) -> bool {
        if let Some(pos) = self
            .tx_ring
            .iter()
            .position(|e| e.envelope.sequence == sequence)
        {
            // Karn's algorithm: update SRTT only from non-retransmitted messages
            let sent_at = self.tx_ring[pos].sent_at_ms;
            let tries = self.tx_ring[pos].tries;
            if tries == 1 {
                let sample_rtt = now_ms.saturating_sub(sent_at);
                if sample_rtt > 0 {
                    self.update_srtt(sample_rtt);
                    // Telemetry (leviculum#35): track the floor of Karn-valid
                    // samples — the conservative propagation-delay estimate a
                    // delivery-rate consumer wants alongside the smoothed RTT.
                    self.min_rtt_ms = Some(match self.min_rtt_ms {
                        Some(m) => m.min(sample_rtt),
                        None => sample_rtt,
                    });
                }
            }

            // Telemetry (leviculum#35): every envelope a delivery proof
            // removes from the ring is confirmed-delivered payload — the
            // delivery-rate numerator.
            self.delivered_bytes = self
                .delivered_bytes
                .saturating_add(self.tx_ring[pos].envelope.packed_size() as u64);
            self.tx_ring.remove(pos);
            // Window tiers use handshake RTT (conservative), not SRTT.
            // SRTT from proof round-trips can be much lower than the true
            // end-to-end RTT, causing spurious promotion to FAST tier.
            self.adjust_window(true, rtt_ms);
            // Overwrite pacing with SRTT (adjust_window used handshake RTT)
            let eff_rtt = self.effective_rtt_ms(rtt_ms);
            self.recalculate_pacing(eff_rtt);
            crate::tracing::debug!(
                seq = sequence,
                tx_ring = self.tx_ring.len(),
                window = self.window,
                srtt = self.srtt_ms,
                "channel: delivered"
            );
            true
        } else {
            crate::tracing::debug!(
                seq = sequence,
                tx_ring = self.tx_ring.len(),
                "channel: delivered — seq not found (duplicate ACK?)"
            );
            false
        }
    }

    /// Poll for timeouts and get actions to take
    ///
    /// Call this periodically to check for timed-out envelopes.
    ///
    /// # Arguments
    /// * `now_ms` - Current time in milliseconds
    /// * `rtt_ms` - Round-trip time in milliseconds
    pub fn poll(&mut self, now_ms: u64, rtt_ms: u64) -> Vec<ChannelAction> {
        let mut actions = Vec::new();
        let mut window_decrements = 0usize;
        let mut pacing_doublings = 0usize;
        let eff_rtt = self.effective_rtt_ms(rtt_ms);

        // First pass: collect indices that need processing and their current tries
        let mut timed_out: Vec<(usize, u8)> = Vec::new();
        for (i, outbound) in self.tx_ring.iter().enumerate() {
            if outbound.state == MessageState::Sent {
                // Use queue_len from when message was sent, not current tx_ring size.
                // Prevents death spiral: growing queue → inflated timeout → no retransmit → queue stays.
                let timeout = calculate_timeout(
                    outbound.tries,
                    eff_rtt,
                    outbound.queue_len_at_send,
                    self.window,
                );
                if now_ms >= outbound.sent_at_ms.saturating_add(timeout) {
                    timed_out.push((i, outbound.tries));
                }
            }
        }

        // Second pass: process timed out envelopes
        for (i, tries) in timed_out {
            // Timeout occurred
            if tries >= self.max_tries {
                let seq = self.tx_ring[i].envelope.sequence;
                crate::tracing::debug!(
                    seq,
                    tries,
                    max_tries = self.max_tries,
                    "channel: max retries exceeded — tearing down link"
                );
                self.tx_ring[i].state = MessageState::Failed;
                actions.push(ChannelAction::TearDownLink);
                window_decrements += 1;
                break; // Link is being torn down, no point continuing
            }

            let new_tries = tries + 1;
            let tx_ring_len = self.tx_ring.len(); // snapshot before mutation

            // Mutate the outbound envelope
            let outbound = &mut self.tx_ring[i];
            outbound.tries = new_tries;
            outbound.sent_at_ms = now_ms;

            let seq = outbound.envelope.sequence;
            actions.push(ChannelAction::Retransmit {
                sequence: seq,
                data: outbound.envelope.pack(),
                tries: new_tries,
            });

            crate::tracing::debug!(
                seq,
                tries = new_tries,
                tx_ring = tx_ring_len,
                timeout_ms = calculate_timeout(new_tries, rtt_ms, tx_ring_len, self.window),
                pacing_ms = self.pacing_interval_ms,
                "channel: retransmit"
            );

            window_decrements += 1; // always — matches Python
            if tries >= 2 {
                pacing_doublings += 1; // only 2nd+ retry triggers pacing MD
            }
        }

        // Apply window adjustments after iteration is complete.
        // Window tiers use handshake RTT (conservative), not SRTT.
        for _ in 0..window_decrements {
            self.adjust_window(false, rtt_ms);
        }
        // Overwrite pacing with SRTT (adjust_window used handshake RTT)
        if window_decrements > 0 {
            self.recalculate_pacing(eff_rtt);
        }

        // Multiplicative decrease: double pacing per retransmit, capped at eff_rtt.
        // adjust_window() already called recalculate_pacing(), so pacing_interval_ms > 0
        // (assuming RTT is known). If still 0 (no RTT yet), skip, no timing data to pace with.
        // Only 2nd+ retransmits trigger MD, first retransmit may be spurious (jitter).
        if pacing_doublings > 0 && self.pacing_interval_ms > 0 {
            let doublings = pacing_doublings.min(10); // safety cap against overflow
            self.pacing_interval_ms = self
                .pacing_interval_ms
                .saturating_mul(1u64 << doublings)
                .min(eff_rtt.max(1));
            crate::tracing::debug!(
                pacing_ms = self.pacing_interval_ms,
                doublings,
                eff_rtt,
                "channel: pacing backoff"
            );
        }

        actions
    }

    /// Adjust window based on transmission success/failure
    fn adjust_window(&mut self, success: bool, rtt_ms: u64) {
        // First update limits based on RTT
        self.update_window_for_rtt(rtt_ms);

        if success {
            // Increase window on success
            if self.window < self.window_max {
                self.window += 1;
            }
        } else {
            // Decrease window on failure
            if self.window > self.window_min {
                self.window = self.window.saturating_sub(1);
            }
        }
        self.recalculate_pacing(rtt_ms);
    }

    /// Recalculate pacing interval from current RTT and window.
    /// Sets pacing to rtt_ms / window, clamped to [1, rtt_ms].
    fn recalculate_pacing(&mut self, rtt_ms: u64) {
        if self.window == 0 || rtt_ms == 0 {
            return;
        }
        let base = rtt_ms / self.window as u64;
        self.pacing_interval_ms = base.clamp(1, rtt_ms);
    }

    /// Clear all pending outbound envelopes
    pub fn clear_tx(&mut self) {
        self.tx_ring.clear();
    }

    /// Clear all pending inbound envelopes
    pub fn clear_rx(&mut self) {
        self.rx_ring.clear();
    }

    /// Set the next TX sequence number for testing purposes.
    ///
    /// Allows tests to create far-future sequence numbers to trigger
    /// RxRingFull on the receiving side.
    #[cfg(test)]
    pub fn force_next_tx_sequence_for_test(&mut self, seq: u16) {
        self.next_tx_sequence = seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::MDU;
    use alloc::vec;

    /// Test message implementation
    struct TestMessage {
        data: Vec<u8>,
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

    #[test]
    fn test_channel_new() {
        let channel = Channel::new();
        assert_eq!(channel.next_tx_sequence, 0);
        assert_eq!(channel.next_rx_sequence(), 0);
        assert_eq!(channel.window(), CHANNEL_WINDOW_INITIAL);
        assert!(channel.is_ready_to_send());
    }

    #[test]
    fn test_sequence_increment() {
        let mut channel = Channel::new();
        assert_eq!(channel.next_sequence(), 0);
        assert_eq!(channel.next_sequence(), 1);
        assert_eq!(channel.next_sequence(), 2);
    }

    #[test]
    fn test_sequence_wraparound() {
        let mut channel = Channel::new();
        channel.next_tx_sequence = 0xFFFF;
        assert_eq!(channel.next_sequence(), 0xFFFF);
        assert_eq!(channel.next_sequence(), 0);
        assert_eq!(channel.next_sequence(), 1);
    }

    #[test]
    fn test_mdu_calculation() {
        let channel = Channel::new();
        let link_mdu = MDU;
        let expected = link_mdu - CHANNEL_ENVELOPE_HEADER_SIZE;
        assert_eq!(channel.mdu(link_mdu), expected);
    }

    #[test]
    fn test_send_basic() {
        let mut channel = Channel::new();
        let msg = TestMessage {
            data: vec![1, 2, 3],
        };
        let result = channel.send(&msg, MDU, 1000, 100);
        assert!(result.is_ok());

        let packed = result.unwrap();
        assert_eq!(packed.len(), CHANNEL_ENVELOPE_HEADER_SIZE + 3);

        // Check envelope contents
        let envelope = Envelope::unpack(&packed).unwrap();
        assert_eq!(envelope.msgtype, TestMessage::MSGTYPE);
        assert_eq!(envelope.sequence, 0);
        assert_eq!(envelope.data, vec![1, 2, 3]);
    }

    #[test]
    fn test_send_too_large() {
        let mut channel = Channel::new();
        let msg = TestMessage {
            data: vec![0; 1000],
        };
        let result = channel.send(&msg, 100, 1000, 100);
        assert_eq!(result, Err(ChannelError::TooLarge));
    }

    /// #242 field regression: on a link that negotiated a large MTU,
    /// an oversized send must be refused as `TooLarge` — the envelope's wire
    /// length field is u16, so the channel MDU is capped at `u16::MAX`
    /// regardless of link MDU. Before the cap, the guard passed and the
    /// `Envelope` length assert panicked a lock-holding delivery thread
    /// ("envelope data length 115764 exceeds maximum 65535" on a live node).
    #[test]
    fn test_send_oversized_on_large_mtu_link_is_too_large_not_panic() {
        let mut channel = Channel::new();
        // The link MDU is far above the envelope ceiling — MTU discovery can
        // negotiate this; it must not raise the channel's payload ceiling.
        let link_mdu = 200_000usize;
        assert_eq!(channel.mdu(link_mdu), u16::MAX as usize);

        // The exact frame length from the field failure.
        let msg = TestMessage {
            data: vec![0; 115_764],
        };
        let result = channel.send(&msg, link_mdu, 1000, 100);
        assert_eq!(result, Err(ChannelError::TooLarge));

        // At the ceiling itself the send is accepted.
        let ok = TestMessage {
            data: vec![0; u16::MAX as usize],
        };
        assert!(channel.send(&ok, link_mdu, 2000, 100).is_ok());
    }

    /// #242 boundary: one byte past the u16 wire ceiling is refused
    /// at the channel guard, on a link whose MDU would otherwise allow it.
    #[test]
    fn test_send_one_past_u16_ceiling_is_too_large() {
        let mut channel = Channel::new();
        let msg = TestMessage {
            data: vec![0; u16::MAX as usize + 1],
        };
        assert_eq!(
            channel.send(&msg, 200_000, 1000, 100),
            Err(ChannelError::TooLarge)
        );
    }

    #[test]
    fn test_send_busy() {
        let mut channel = Channel::new();
        channel.set_window_for_test(1);

        let msg = TestMessage { data: vec![1] };

        // First send should succeed
        assert!(channel.send(&msg, MDU, 1000, 100).is_ok());

        // Second send should fail (busy)
        assert_eq!(channel.send(&msg, MDU, 1000, 100), Err(ChannelError::Busy));
    }

    #[test]
    fn test_invalid_msgtype() {
        struct ReservedMessage;
        impl Message for ReservedMessage {
            const MSGTYPE: u16 = CHANNEL_MSGTYPE_RESERVED;
            fn pack(&self) -> Vec<u8> {
                Vec::new()
            }
            fn unpack(_: &[u8]) -> Result<Self, ChannelError> {
                Ok(Self)
            }
        }

        let mut channel = Channel::new();
        let result = channel.send(&ReservedMessage, MDU, 1000, 100);
        assert_eq!(result, Err(ChannelError::InvalidMsgType));
    }

    #[test]
    fn test_receive_in_order() {
        let mut channel = Channel::new();

        let envelope = Envelope::new(0x0001, 0, vec![1, 2, 3]);
        let packed = envelope.pack();

        let result = channel.receive(&packed, [0u8; 32]);
        assert!(result.is_ok());
        match result.unwrap() {
            ReceiveOutcome::Delivered(env) => assert_eq!(env.sequence, 0),
            other => panic!("expected Delivered, got {:?}", other),
        }
        assert_eq!(channel.next_rx_sequence(), 1);
    }

    #[test]
    fn test_receive_out_of_order() {
        let mut channel = Channel::new();

        // Receive sequence 1 first (out of order)
        let envelope1 = Envelope::new(0x0001, 1, vec![2]);
        let result = channel.receive(&envelope1.pack(), [1u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::Buffered));
        assert_eq!(channel.next_rx_sequence(), 0); // Still expecting 0

        // Now receive sequence 0
        let envelope0 = Envelope::new(0x0001, 0, vec![1]);
        let result = channel.receive(&envelope0.pack(), [0u8; 32]);
        match result.unwrap() {
            ReceiveOutcome::Delivered(env) => assert_eq!(env.sequence, 0),
            other => panic!("expected Delivered, got {:?}", other),
        }
        assert_eq!(channel.next_rx_sequence(), 1);

        // Drain should give us sequence 1 with its proof hash
        let drained = channel.drain_received();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0.sequence, 1);
        assert_eq!(drained[0].1, [1u8; 32]); // proof hash preserved
        assert_eq!(channel.next_rx_sequence(), 2);
    }

    #[test]
    fn test_mark_delivered() {
        let mut channel = Channel::new();
        let msg = TestMessage { data: vec![1] };

        channel.send(&msg, MDU, 1000, 100).unwrap();
        assert_eq!(channel.outstanding(), 1);

        // Should return true for known sequence
        assert!(channel.mark_delivered(0, 1100, 100));
        assert_eq!(channel.outstanding(), 0);

        // Should return false for unknown sequence
        assert!(!channel.mark_delivered(0, 1200, 100));
        assert!(!channel.mark_delivered(999, 1200, 100));
    }

    // leviculum#35: the per-link delivery telemetry counters.

    /// Every proof-confirmed envelope adds its packed size to
    /// `delivered_bytes`; a duplicate/unknown ack adds nothing.
    #[test]
    fn test_delivered_bytes_accumulates_on_proofed_envelopes_only() {
        let mut channel = Channel::new();
        let msg = TestMessage {
            data: vec![1, 2, 3],
        };

        channel.send(&msg, MDU, 1000, 100).unwrap();
        channel.send(&msg, MDU, 1010, 100).unwrap();
        assert_eq!(channel.delivered_bytes(), 0, "nothing proofed yet");

        assert!(channel.mark_delivered(0, 1100, 100));
        let after_one = channel.delivered_bytes();
        assert!(after_one > 0, "proofed envelope must count its bytes");

        // Duplicate ack for the same sequence adds nothing.
        assert!(!channel.mark_delivered(0, 1150, 100));
        assert_eq!(channel.delivered_bytes(), after_one);

        assert!(channel.mark_delivered(1, 1200, 100));
        assert_eq!(
            channel.delivered_bytes(),
            after_one * 2,
            "identical envelopes must count identical bytes"
        );
    }

    /// `min_rtt_ms` tracks the floor of Karn-valid samples: it starts unset,
    /// takes the first sample, keeps the minimum thereafter, and ignores
    /// retransmitted (tries > 1) deliveries entirely.
    #[test]
    fn test_min_rtt_tracks_karn_valid_floor() {
        let mut channel = Channel::new();
        let msg = TestMessage { data: vec![1] };

        assert_eq!(channel.min_rtt_ms(), None);

        // First delivery: 100 ms sample.
        channel.send(&msg, MDU, 1000, 100).unwrap();
        assert!(channel.mark_delivered(0, 1100, 100));
        assert_eq!(channel.min_rtt_ms(), Some(100));

        // Faster delivery lowers the floor.
        channel.send(&msg, MDU, 2000, 100).unwrap();
        assert!(channel.mark_delivered(1, 2040, 100));
        assert_eq!(channel.min_rtt_ms(), Some(40));

        // Slower delivery does not raise it.
        channel.send(&msg, MDU, 3000, 100).unwrap();
        assert!(channel.mark_delivered(2, 3300, 100));
        assert_eq!(channel.min_rtt_ms(), Some(40));

        // A retransmitted envelope (tries > 1 after a timeout retransmit) is
        // Karn-excluded: even an apparently-tiny RTT must not move the floor.
        channel.send(&msg, MDU, 4000, 100).unwrap();
        let actions = channel.poll(100_000, 100);
        assert!(matches!(&actions[0], ChannelAction::Retransmit { .. }));
        assert!(channel.mark_delivered(3, 100_001, 100));
        assert_eq!(channel.min_rtt_ms(), Some(40));
    }

    /// Window-full and pacing rejections increment their distinct counters —
    /// the congestion-limited vs pacing-limited backpressure signals.
    #[test]
    fn test_backpressure_rejection_counters() {
        let mut channel = Channel::new();
        let msg = TestMessage { data: vec![1] };

        assert_eq!(channel.busy_rejections(), 0);
        assert_eq!(channel.pacing_rejections(), 0);

        // Fill the window (initial window = CHANNEL_WINDOW_INITIAL) with
        // sends spaced past any pacing delay, then one more send must be Busy.
        let mut now = 1_000u64;
        while channel.is_ready_to_send() {
            channel.send(&msg, MDU, now, 100).unwrap();
            now += channel.pacing_interval_ms().max(1) + 1;
        }
        assert!(matches!(
            channel.send(&msg, MDU, now, 100),
            Err(ChannelError::Busy)
        ));
        assert_eq!(channel.busy_rejections(), 1);
        assert_eq!(channel.pacing_rejections(), 0);

        // Free a slot, then send again immediately inside the pacing window.
        assert!(channel.mark_delivered(0, now, 100));
        let ready_at = channel.next_send_at_ms();
        if ready_at > 0 {
            assert!(matches!(
                channel.send(&msg, MDU, ready_at - 1, 100),
                Err(ChannelError::PacingDelay { .. })
            ));
            assert_eq!(channel.pacing_rejections(), 1);
            assert_eq!(channel.busy_rejections(), 1, "busy count untouched");
        }
    }

    #[test]
    fn test_poll_timeout() {
        let mut channel = Channel::new();
        let msg = TestMessage { data: vec![1] };

        // Send message at time 1000
        channel.send(&msg, MDU, 1000, 100).unwrap();

        // Poll before timeout - no actions
        let actions = channel.poll(1000, 100);
        assert!(actions.is_empty());

        // Poll after timeout - should get retransmit
        let actions = channel.poll(100_000, 100);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ChannelAction::Retransmit { sequence, .. } => {
                assert_eq!(*sequence, 0);
            }
            _ => panic!("Expected Retransmit action"),
        }
    }

    #[test]
    fn test_poll_max_retries() {
        let mut channel = Channel::new();
        channel.set_max_tries_for_test(2);
        let msg = TestMessage { data: vec![1] };

        channel.send(&msg, MDU, 0, 100).unwrap();

        // First timeout - retransmit
        let actions = channel.poll(10_000, 100);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], ChannelAction::Retransmit { .. }));

        // Second timeout - tear down
        let actions = channel.poll(100_000, 100);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], ChannelAction::TearDownLink));
    }

    #[test]
    fn test_window_adjustment_on_rtt() {
        let mut channel = Channel::new();

        // Fast RTT should increase max window
        channel.update_window_for_rtt(100);
        assert_eq!(channel.window_max(), CHANNEL_WINDOW_MAX_FAST);

        // Medium RTT
        channel.update_window_for_rtt(500);
        assert_eq!(channel.window_max(), CHANNEL_WINDOW_MAX_MEDIUM);

        // Slow RTT (just below VERY_SLOW threshold)
        channel.update_window_for_rtt(1999);
        assert_eq!(channel.window_max(), CHANNEL_WINDOW_MAX_SLOW);

        // Very slow RTT (>= 2000ms, LoRa-class)
        channel.update_window_for_rtt(2000);
        assert_eq!(channel.window_max(), CHANNEL_WINDOW_MAX_VERY_SLOW);
    }

    #[test]
    fn test_timeout_formula() {
        // Basic timeout: (max(100*2.5, 25) + 0) * 1.5^0 = 250
        let timeout = calculate_timeout(1, 100, 0, 2);
        assert_eq!(timeout, 250);

        // With retries: (250 + 0) * 1.5^1 = 375
        let timeout = calculate_timeout(2, 100, 0, 2);
        assert_eq!(timeout, 375);
    }

    #[test]
    fn test_timeout_additive_queue_delay() {
        // Verify additive formula at key operating points
        // (base_rto + queue_delay) * backoff where queue_delay = qlen * (rtt / window)

        // LoRa failure scenario: SRTT=6421, queue=12, window=12
        let t = calculate_timeout(1, 6421, 12, 12);
        // base_rto = 6421*2.5 = 16052.5, queue_delay = 12*(6421/12) = 6421
        assert_eq!(t, 22473);

        // Same SRTT, smaller window (AIMD reduced)
        let t = calculate_timeout(1, 6421, 12, 5);
        // queue_delay = 12*(6421/5) = 15410.4
        assert_eq!(t, 31462);

        // Fast link: rtt=500, queue=1, window=2
        let t = calculate_timeout(1, 500, 1, 2);
        // base_rto=1250, queue_delay=250
        assert_eq!(t, 1500);

        // Empty queue: pure base_rto
        let t = calculate_timeout(1, 500, 0, 2);
        assert_eq!(t, 1250);
    }

    #[test]
    fn test_timeout_window_floor() {
        // window=0 must not panic or produce infinity, clamped to 1
        let t = calculate_timeout(1, 1000, 5, 0);
        // base_rto=2500, queue_delay=5*1000=5000
        assert_eq!(t, 7500);
    }

    #[test]
    fn test_timeout_backoff_on_full_sum() {
        // Backoff applies to (base_rto + queue_delay), not just base_rto
        let t1 = calculate_timeout(1, 6421, 12, 12);
        let t2 = calculate_timeout(2, 6421, 12, 12);
        let t3 = calculate_timeout(3, 6421, 12, 12);

        // t2 ≈ t1 * 1.5, t3 ≈ t1 * 2.25 (allow ±1 for f64 truncation)
        assert!((t2 as i64 - (t1 as f64 * 1.5) as i64).abs() <= 1);
        assert!((t3 as i64 - (t1 as f64 * 2.25) as i64).abs() <= 1);
    }

    #[test]
    fn test_timeout_vs_old_formula_regression() {
        // At the operating point that caused lora_link_rust failure:
        // Old multiplicative: 16053 * 1.0 * 13.5 = 216,716ms (217s!)
        // New additive: (16053 + 6421) * 1.0 = 22,474ms
        let timeout = calculate_timeout(1, 6421, 12, 12);
        assert!(
            timeout < 30_000,
            "timeout {} must be well under 30s (old formula gave 217s)",
            timeout
        );
        assert!(
            timeout > 15_000,
            "timeout {} must include meaningful queue delay",
            timeout
        );
    }

    #[test]
    fn test_send_system_allows_reserved_msgtype() {
        // send_system should allow system message types (>= 0xf000)
        let msg = StreamDataMessage::new(0, vec![1, 2, 3], false, false);

        let mut channel = Channel::new();
        let result = channel.send_system(&msg, MDU, 1000, 100);
        assert!(result.is_ok(), "send_system should allow reserved MSGTYPE");

        let packed = result.unwrap();
        let envelope = Envelope::unpack(&packed).unwrap();
        assert_eq!(envelope.msgtype, StreamDataMessage::MSGTYPE);
        assert_eq!(envelope.msgtype, 0xff00);
    }

    #[test]
    fn test_send_rejects_system_msgtype() {
        // Regular send() should reject system message types
        let msg = StreamDataMessage::new(0, vec![1, 2, 3], false, false);

        let mut channel = Channel::new();
        let result = channel.send(&msg, MDU, 1000, 100);
        assert_eq!(
            result,
            Err(ChannelError::InvalidMsgType),
            "send should reject reserved MSGTYPE"
        );
    }

    #[test]
    fn test_send_system_respects_window() {
        let mut channel = Channel::new();
        channel.set_window_for_test(1);

        let msg = StreamDataMessage::new(0, vec![1], false, false);

        // First send should succeed
        assert!(channel.send_system(&msg, MDU, 1000, 100).is_ok());

        // Second send should fail (busy)
        assert_eq!(
            channel.send_system(&msg, MDU, 1000, 100),
            Err(ChannelError::Busy)
        );
    }

    #[test]
    fn test_window_max_accessor() {
        let mut channel = Channel::new();
        assert_eq!(channel.window_max(), CHANNEL_WINDOW_MAX_SLOW);

        channel.update_window_for_rtt(100);
        assert_eq!(channel.window_max(), CHANNEL_WINDOW_MAX_FAST);

        channel.update_window_for_rtt(500);
        assert_eq!(channel.window_max(), CHANNEL_WINDOW_MAX_MEDIUM);
    }

    #[test]
    fn test_send_system_respects_mdu() {
        let mut channel = Channel::new();
        let msg = StreamDataMessage::new(0, vec![0; 1000], false, false);

        // With small MDU, should fail
        let result = channel.send_system(&msg, 100, 1000, 100);
        assert_eq!(result, Err(ChannelError::TooLarge));
    }

    #[test]
    fn test_channel_receive_message_typed() {
        let mut sender = Channel::new();
        let mut receiver = Channel::new();

        let msg = TestMessage {
            data: vec![1, 2, 3, 4],
        };

        // Send message
        let packed = sender.send(&msg, MDU, 1000, 100).unwrap();

        // Receive using typed API
        let received: Option<TestMessage> = receiver.receive_message(&packed).unwrap();
        assert!(received.is_some());
        assert_eq!(received.unwrap().data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_channel_send_receive_symmetric() {
        // Test the complete symmetric workflow: send<M> -> receive_message<M>
        let mut sender = Channel::new();
        let mut receiver = Channel::new();

        // Send multiple messages
        let msg1 = TestMessage {
            data: vec![10, 20, 30],
        };
        let msg2 = TestMessage {
            data: vec![40, 50, 60],
        };

        let packed1 = sender.send(&msg1, MDU, 1000, 100).unwrap();
        let packed2 = sender.send(&msg2, MDU, 2000, 100).unwrap();

        // Receive in order
        let r1: Option<TestMessage> = receiver.receive_message(&packed1).unwrap();
        assert!(r1.is_some());
        assert_eq!(r1.unwrap().data, vec![10, 20, 30]);

        let r2: Option<TestMessage> = receiver.receive_message(&packed2).unwrap();
        assert!(r2.is_some());
        assert_eq!(r2.unwrap().data, vec![40, 50, 60]);
    }

    #[test]
    fn test_channel_receive_message_wrong_type() {
        // Define a different message type
        struct OtherMessage {
            value: u8,
        }

        impl Message for OtherMessage {
            const MSGTYPE: u16 = 0x9999;

            fn pack(&self) -> Vec<u8> {
                vec![self.value]
            }

            fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
                if data.is_empty() {
                    return Err(ChannelError::EnvelopeTruncated);
                }
                Ok(Self { value: data[0] })
            }
        }

        let mut sender = Channel::new();
        let mut receiver = Channel::new();

        // Send OtherMessage
        let msg = OtherMessage { value: 42 };
        let packed = sender.send(&msg, MDU, 1000, 100).unwrap();

        // Try to receive as TestMessage - should return None (type mismatch)
        let result: Option<TestMessage> = receiver.receive_message(&packed).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_channel_receive_message_out_of_order() {
        let mut sender = Channel::new();
        let mut receiver = Channel::new();

        let msg0 = TestMessage { data: vec![0] };
        let msg1 = TestMessage { data: vec![1] };

        let packed0 = sender.send(&msg0, MDU, 1000, 100).unwrap();
        let packed1 = sender.send(&msg1, MDU, 2000, 100).unwrap();

        // Receive out of order (msg1 first)
        let r1: Option<TestMessage> = receiver.receive_message(&packed1).unwrap();
        assert!(r1.is_none()); // Should be buffered

        // Now receive msg0
        let r0: Option<TestMessage> = receiver.receive_message(&packed0).unwrap();
        assert!(r0.is_some());
        assert_eq!(r0.unwrap().data, vec![0]);

        // Drain should give us msg1
        let drained = receiver.drain_received();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0.data, vec![1]);
    }

    #[test]
    fn test_receive_rx_ring_full() {
        use super::envelope::Envelope;

        let mut channel = Channel::new();
        // next_rx_sequence is 0; send a packet with offset == CHANNEL_RX_RING_MAX
        let seq = CHANNEL_RX_RING_MAX as u16;
        let envelope = Envelope::new(0x0001, seq, vec![42]);
        let packed = envelope.pack();

        let result = channel.receive(&packed, [0u8; 32]);
        assert_eq!(result, Err(ChannelError::RxRingFull));
    }

    #[test]
    fn test_receive_within_rx_ring_max() {
        use super::envelope::Envelope;

        let mut channel = Channel::new();
        // next_rx_sequence is 0; send a packet with offset == CHANNEL_RX_RING_MAX - 1
        let seq = (CHANNEL_RX_RING_MAX - 1) as u16;
        let envelope = Envelope::new(0x0001, seq, vec![42]);
        let packed = envelope.pack();

        let result = channel.receive(&packed, [0u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::Buffered));
    }

    #[test]
    fn test_receive_backward_sequence_reproving() {
        use super::envelope::Envelope;

        let mut channel = Channel::new();

        // Receive seq 0 in-order
        let env0 = Envelope::new(0x0001, 0, vec![1]);
        let result = channel.receive(&env0.pack(), [0u8; 32]);
        match result.unwrap() {
            ReceiveOutcome::Delivered(env) => assert_eq!(env.sequence, 0),
            other => panic!("expected Delivered, got {:?}", other),
        }
        // next_rx_sequence is now 1

        // Drain (no buffered messages)
        assert!(channel.drain_received().is_empty());

        // Simulate retransmit: receive seq 0 again (proof was lost)
        let env0_retx = Envelope::new(0x0001, 0, vec![1]);
        let result = channel.receive(&env0_retx.pack(), [0u8; 32]);
        // Must be AlreadyDelivered, NOT Err(RxRingFull)
        assert_eq!(result, Ok(ReceiveOutcome::AlreadyDelivered));

        // rx_ring must not grow (duplicate was not buffered)
        assert!(channel.rx_ring_is_empty());
    }

    #[test]
    fn test_receive_backward_sequence_many_ahead() {
        use super::envelope::Envelope;

        let mut channel = Channel::new();

        // Advance next_rx_sequence to 141 by receiving 0..140 in order
        for seq in 0..141u16 {
            let env = Envelope::new(0x0001, seq, vec![seq as u8]);
            let result = channel.receive(&env.pack(), [0u8; 32]);
            assert_eq!(
                result,
                Ok(ReceiveOutcome::Delivered(Envelope::new(
                    0x0001,
                    seq,
                    vec![seq as u8]
                )))
            );
        }
        assert_eq!(channel.next_rx_sequence(), 141);

        // Retransmit seq=128 (the scenario from the bug report)
        let env_retx = Envelope::new(0x0001, 128, vec![128]);
        let result = channel.receive(&env_retx.pack(), [0u8; 32]);
        // Must be AlreadyDelivered, backward sequence re-proved
        assert_eq!(result, Ok(ReceiveOutcome::AlreadyDelivered));
    }

    #[test]
    fn test_receive_backward_sequence_wraparound() {
        use super::envelope::Envelope;

        let mut channel = Channel::new();

        // Set next_rx_sequence near the modulus boundary
        channel.set_next_rx_sequence_for_test(5);

        // Receive seq=65534, behind next_rx=5 across the wraparound boundary
        // offset = (65536 - 5 + 65534) % 65536 = 65529, which is >= 32768
        let env = Envelope::new(0x0001, 65534, vec![1]);
        let result = channel.receive(&env.pack(), [0u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::AlreadyDelivered));
    }

    // Pacing Tests
    #[test]
    fn test_pacing_delay_returned() {
        let mut channel = Channel::new();
        channel.set_pacing_for_test(100, 1100);

        let msg = TestMessage { data: vec![1] };

        // Send at t=1000, before next_send_at (1100) → PacingDelay
        let result = channel.send(&msg, MDU, 1000, 100);
        assert_eq!(result, Err(ChannelError::PacingDelay { ready_at_ms: 1100 }));

        // Send at t=1100, at next_send_at → Ok
        let result = channel.send(&msg, MDU, 1100, 100);
        assert!(result.is_ok());
    }

    #[test]
    fn test_pacing_next_send_set_on_send() {
        let mut channel = Channel::new();
        channel.set_pacing_for_test(50, 0);

        let msg = TestMessage { data: vec![1] };
        channel.send(&msg, MDU, 1000, 100).unwrap();

        assert_eq!(channel.next_send_at_ms(), 1050);
    }

    #[test]
    fn test_busy_before_pacing() {
        // Window=1 with pacing active. After one send, the second should
        // return Busy (not PacingDelay), because there's no window slot.
        let mut channel = Channel::new();
        channel.set_window_for_test(1);
        channel.set_pacing_for_test(10, 0);

        let msg = TestMessage { data: vec![1] };
        channel.send(&msg, MDU, 1000, 100).unwrap();

        // Second send: busy takes priority over pacing
        let result = channel.send(&msg, MDU, 1000, 100);
        assert_eq!(result, Err(ChannelError::Busy));
    }

    #[test]
    fn test_retransmit_doubles_pacing() {
        let mut channel = Channel::new();
        // Use slow RTT (below VERY_SLOW threshold) to avoid window_min clamping overriding pacing
        let rtt_ms = 1999u64;
        let msg = TestMessage { data: vec![1] };

        channel.send(&msg, MDU, 0, rtt_ms).unwrap();

        // First retransmit (tries=1→2): window shrinks but pacing does NOT
        // double (Change D: first retransmit may be spurious)
        let actions = channel.poll(100_000, rtt_ms);
        assert!(!actions.is_empty());
        assert!(matches!(&actions[0], ChannelAction::Retransmit { .. }));
        let pacing_after_first = channel.pacing_interval_ms();

        // Second retransmit (tries=2→3): now pacing DOES double
        let actions = channel.poll(200_000, rtt_ms);
        assert!(!actions.is_empty());
        assert!(matches!(&actions[0], ChannelAction::Retransmit { .. }));

        // After recalculate_pacing + MD: pacing > base pacing
        let base_pacing = rtt_ms / channel.window() as u64;
        assert!(
            channel.pacing_interval_ms() > base_pacing,
            "MD should push pacing {} above recalculated base {}",
            channel.pacing_interval_ms(),
            base_pacing
        );
        // Pacing should have increased from the first retransmit's level
        assert!(
            channel.pacing_interval_ms() > pacing_after_first,
            "second retransmit pacing {} should exceed first retransmit pacing {}",
            channel.pacing_interval_ms(),
            pacing_after_first
        );
        // And pacing should not exceed rtt_ms
        assert!(
            channel.pacing_interval_ms() <= rtt_ms,
            "pacing {} should not exceed rtt {}",
            channel.pacing_interval_ms(),
            rtt_ms
        );
    }

    #[test]
    fn test_pacing_ceiling_on_retransmit() {
        let mut channel = Channel::new();
        let rtt_ms = 100u64;

        let msg = TestMessage { data: vec![1] };
        channel.send(&msg, MDU, 0, rtt_ms).unwrap();

        // First retransmit (tries=1→2): no pacing MD (Change D)
        channel.poll(100_000, rtt_ms);

        // Set pacing near the ceiling before second retransmit
        channel.set_pacing_for_test(80, 0);

        // Second retransmit (tries=2→3): MD doubles to 160, capped at rtt_ms=100
        channel.poll(200_000, rtt_ms);

        assert!(
            channel.pacing_interval_ms() <= rtt_ms,
            "pacing {} should not exceed rtt {}",
            channel.pacing_interval_ms(),
            rtt_ms
        );
    }

    #[test]
    fn test_recalculate_pacing() {
        let mut channel = Channel::new();

        // rtt=100, window=5 → 20ms
        channel.set_window_for_test(5);
        channel.recalculate_pacing(100);
        assert_eq!(channel.pacing_interval_ms(), 20);

        // rtt=1000, window=2 → 500ms
        channel.set_window_for_test(2);
        channel.recalculate_pacing(1000);
        assert_eq!(channel.pacing_interval_ms(), 500);

        // window=0 → no change (stays at 500)
        channel.set_window_for_test(0);
        channel.recalculate_pacing(100);
        assert_eq!(channel.pacing_interval_ms(), 500);

        // rtt=0 → no change (stays at 500)
        channel.set_window_for_test(5);
        channel.recalculate_pacing(0);
        assert_eq!(channel.pacing_interval_ms(), 500);
    }

    #[test]
    fn test_delivery_recalculates_pacing() {
        let mut channel = Channel::new();
        // Use medium RTT (180..750ms): window_min=2, window_max=12
        let rtt_ms = 500u64;
        channel.set_window_bounds_for_test(5, CHANNEL_WINDOW_MIN_SLOW, CHANNEL_WINDOW_MAX_MEDIUM);
        channel.set_pacing_for_test(rtt_ms / 5, 0); // 100ms

        let msg = TestMessage { data: vec![1] };
        channel.send(&msg, MDU, 1000, rtt_ms).unwrap();

        // Deliver → window goes to 6, pacing = 500/6 = 83
        channel.mark_delivered(0, 1500, rtt_ms);

        assert_eq!(channel.window(), 6);
        assert_eq!(channel.pacing_interval_ms(), 83); // 500/6 = 83
    }

    #[test]
    fn test_no_pacing_before_first_rtt() {
        // Fresh channel: pacing_interval=0, next_send_at=0
        // Should allow immediate sends at any time
        let mut channel = Channel::new();
        assert_eq!(channel.pacing_interval_ms(), 0);
        assert_eq!(channel.next_send_at_ms(), 0);

        let msg = TestMessage { data: vec![1] };

        // Send at t=5000 → should succeed (no PacingDelay)
        let result = channel.send(&msg, MDU, 5000, 0);
        assert!(
            result.is_ok(),
            "fresh channel should not pace: {:?}",
            result
        );

        // next_send_at should be 5000 + 0 = 5000
        assert_eq!(channel.next_send_at_ms(), 5000);
    }

    // Live Timeout Tests
    #[test]
    fn test_timeout_uses_send_time_queue_len() {
        // Send 5 messages. Deliver 3. Remaining 2 still use their
        // send-time queue_len for timeout (not the shrunk tx_ring).
        // This prevents the opposite problem: queue shrinking shouldn't
        // speed up retransmit for messages sent when queue was large.
        let mut channel = Channel::new();
        channel.set_window_for_test(10);
        // Use slow RTT (>750ms) so window stays at max=5 after adjust_window
        let rtt_ms = 1000;

        // Send 5 messages at t=0. queue_len_at_send: 1,2,3,4,5
        for i in 0..5u8 {
            let msg = TestMessage { data: vec![i] };
            channel.send(&msg, MDU, 0, rtt_ms).unwrap();
        }
        assert_eq!(channel.outstanding(), 5);

        // Mark seq=0,1,2 delivered. Window clamped to max=5.
        // Remaining: seq 3 (qlen=4), seq 4 (qlen=5).
        channel.mark_delivered(0, 0, rtt_ms);
        channel.mark_delivered(1, 0, rtt_ms);
        channel.mark_delivered(2, 0, rtt_ms);
        assert_eq!(channel.outstanding(), 2);

        // Timeout for seq 3: (2500 + 4*(1000/5)) = 3300ms
        // Timeout for seq 4: (2500 + 5*(1000/5)) = 3500ms
        // Poll at t=2600: neither triggers (send-time queue_len is preserved)
        let actions = channel.poll(2600, rtt_ms);
        assert!(
            actions.is_empty(),
            "send-time queue_len preserved: seq3 timeout=3300, seq4 timeout=3500"
        );

        // Poll at t=3301: seq 3 triggers (3300 < 3301), seq 4 does not (3500 > 3301)
        let actions = channel.poll(3301, rtt_ms);
        assert_eq!(actions.len(), 1, "only seq 3 should retransmit at t=3301");
        assert!(matches!(
            &actions[0],
            ChannelAction::Retransmit { sequence: 3, .. }
        ));
    }

    #[test]
    fn test_timeout_not_inflated_by_later_sends() {
        // Send 1 message (queue_len_at_send=1). Send 4 more.
        // First message's timeout stays based on queue_len=1, not 5.
        let mut channel = Channel::new();
        channel.set_window_for_test(10);
        // Use slow RTT (>750ms) so window stays at max=5
        let rtt_ms = 1000;

        // Send 1 message at t=0 → queue_len_at_send=1
        let msg = TestMessage { data: vec![0] };
        channel.send(&msg, MDU, 0, rtt_ms).unwrap();

        // Send 4 more → queue_len_at_send: 2,3,4,5
        for i in 1..5u8 {
            let msg = TestMessage { data: vec![i] };
            channel.send(&msg, MDU, 0, rtt_ms).unwrap();
        }
        assert_eq!(channel.outstanding(), 5);

        // Window is still 10 (no mark_delivered, so no adjust_window call).
        // Timeout for seq 0: (2500 + 1*(1000/10)) = 2600ms
        // Timeout for seq 1: (2500 + 2*(1000/10)) = 2700ms
        // Poll at t=2601: seq 0 triggers (2600 < 2601), but not seq 1 (2700 > 2601)
        let actions = channel.poll(2601, rtt_ms);
        assert_eq!(
            actions.len(),
            1,
            "seq 0 should retransmit: send-time queue_len=1 → timeout=2600ms"
        );
        assert!(matches!(
            &actions[0],
            ChannelAction::Retransmit { sequence: 0, .. }
        ));
    }

    // SRTT Tests
    #[test]
    fn test_srtt_first_measurement() {
        // Send message at t=1000, deliver at t=1100. SRTT = 100, RTTVAR = 50.
        let mut channel = Channel::new();
        channel.set_window_for_test(10);
        let rtt_ms = 500; // handshake RTT (fallback)

        let msg = TestMessage { data: vec![1] };
        channel.send(&msg, MDU, 1000, rtt_ms).unwrap();

        assert_eq!(channel.srtt_ms(), 0.0, "SRTT should start unmeasured");

        channel.mark_delivered(0, 1100, rtt_ms);

        assert!(
            (channel.srtt_ms() - 100.0).abs() < 0.001,
            "SRTT should be 100ms, got {}",
            channel.srtt_ms()
        );
        assert!(
            (channel.rttvar_ms() - 50.0).abs() < 0.001,
            "RTTVAR should be 50ms, got {}",
            channel.rttvar_ms()
        );
    }

    #[test]
    fn test_srtt_converges() {
        // Send and deliver multiple messages. Verify SRTT converges
        // toward the average and reflects the samples.
        let mut channel = Channel::new();
        channel.set_window_for_test(10);
        let rtt_ms = 500;

        // Sample RTTs: 100, 120, 80, 110, 90
        let samples = [100u64, 120, 80, 110, 90];
        for (i, &sample) in samples.iter().enumerate() {
            let msg = TestMessage {
                data: vec![i as u8],
            };
            let t_send = (i as u64) * 1000;
            channel.send(&msg, MDU, t_send, rtt_ms).unwrap();
            channel.mark_delivered(i as u16, t_send + sample, rtt_ms);
        }

        // After 5 samples around 100ms, SRTT should be close to 100
        assert!(
            (channel.srtt_ms() - 100.0).abs() < 15.0,
            "SRTT should converge near 100ms, got {}",
            channel.srtt_ms()
        );
    }

    #[test]
    fn test_srtt_karn_skips_retransmits() {
        // Send, timeout, retransmit, deliver. SRTT should NOT be updated
        // (Karn's algorithm: ambiguous RTT for retransmitted messages).
        let mut channel = Channel::new();
        channel.set_window_for_test(10);
        let rtt_ms = 100;

        let msg = TestMessage { data: vec![1] };
        channel.send(&msg, MDU, 0, rtt_ms).unwrap();

        // Force a retransmit by polling after timeout
        let actions = channel.poll(100_000, rtt_ms);
        assert!(!actions.is_empty());
        assert!(matches!(&actions[0], ChannelAction::Retransmit { .. }));

        // Now deliver, tries > 1, so Karn's algorithm skips SRTT update
        channel.mark_delivered(0, 200_000, rtt_ms);

        assert_eq!(
            channel.srtt_ms(),
            0.0,
            "SRTT should NOT be updated for retransmitted messages"
        );
    }

    #[test]
    fn test_srtt_effective_rtt_ms_fallback() {
        // Before any deliveries, effective_rtt_ms returns fallback.
        // After delivery with sample, it returns SRTT.
        let mut channel = Channel::new();
        channel.set_window_for_test(10);

        assert_eq!(
            channel.effective_rtt_ms(500),
            500,
            "should return fallback before any measurement"
        );

        // Send and deliver to establish SRTT
        let msg = TestMessage { data: vec![1] };
        channel.send(&msg, MDU, 1000, 500).unwrap();
        channel.mark_delivered(0, 1200, 500);

        assert_eq!(
            channel.effective_rtt_ms(500),
            200,
            "should return SRTT (200ms) after measurement"
        );
    }

    // MAX_TRIES and Pacing MD Tests
    #[test]
    fn test_max_tries_8_survives_longer() {
        // With max_tries=8, 7 retransmits trigger Retransmit (not TearDownLink),
        // and the 8th triggers TearDownLink.
        let mut channel = Channel::new();
        assert_eq!(channel.max_tries, 8);
        let rtt_ms = 100;

        let msg = TestMessage { data: vec![1] };
        channel.send(&msg, MDU, 0, rtt_ms).unwrap();

        // 7 retransmits (tries 1→2, 2→3, ..., 7→8)
        let mut t = 0u64;
        for i in 0..7 {
            t += 1_000_000; // far enough in the future to trigger timeout
            let actions = channel.poll(t, rtt_ms);
            assert_eq!(
                actions.len(),
                1,
                "retry {} should produce exactly one action",
                i + 1
            );
            assert!(
                matches!(&actions[0], ChannelAction::Retransmit { tries, .. } if *tries == (i as u8 + 2)),
                "retry {} should be Retransmit with tries={}, got {:?}",
                i + 1,
                i + 2,
                actions[0]
            );
        }

        // 8th timeout, tries=8 >= max_tries=8 → TearDownLink
        t += 1_000_000;
        let actions = channel.poll(t, rtt_ms);
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], ChannelAction::TearDownLink),
            "8th timeout should tear down link, got {:?}",
            actions[0]
        );
    }

    #[test]
    fn test_first_retransmit_no_pacing_md() {
        // First retransmit (tries=1→2): window decreases but pacing does NOT
        // double. Second retransmit (tries=2→3): window decreases AND pacing
        // doubles.
        let mut channel = Channel::new();
        let rtt_ms = 1999u64; // slow RTT (below VERY_SLOW threshold) for manageable window
        let msg = TestMessage { data: vec![1] };

        channel.send(&msg, MDU, 0, rtt_ms).unwrap();

        // First retransmit
        let actions = channel.poll(100_000, rtt_ms);
        assert!(!actions.is_empty());
        assert!(matches!(
            &actions[0],
            ChannelAction::Retransmit { tries: 2, .. }
        ));

        // Pacing should be recalculate_pacing result only (no MD doubling)
        let pacing_after_first = channel.pacing_interval_ms();
        let base_pacing = rtt_ms / channel.window() as u64;
        assert_eq!(
            pacing_after_first, base_pacing,
            "first retransmit: pacing {} should equal base {} (no MD)",
            pacing_after_first, base_pacing
        );

        // Second retransmit
        let actions = channel.poll(200_000, rtt_ms);
        assert!(!actions.is_empty());
        assert!(matches!(
            &actions[0],
            ChannelAction::Retransmit { tries: 3, .. }
        ));

        // Now pacing SHOULD be doubled (MD kicks in for tries >= 2)
        assert!(
            channel.pacing_interval_ms() > base_pacing,
            "second retransmit: pacing {} should exceed base {} (MD applied)",
            channel.pacing_interval_ms(),
            base_pacing
        );
    }

    // T8: FAST Tier Guard
    #[test]
    fn test_fast_tier_guard_handshake_rtt_prevents_promotion() {
        // Guards the v0.5.19 fix: mark_delivered() calls adjust_window(true, rtt_ms)
        // where rtt_ms is the handshake RTT, not the SRTT. This prevents spurious
        // promotion to FAST tier when proof round-trips are fast but the link is slow.
        let mut channel = Channel::new();

        // Default window bounds: SLOW tier (min=2, max=5)
        assert_eq!(channel.window_max(), CHANNEL_WINDOW_MAX_SLOW);

        // Send a message at t=1000 with a slow handshake RTT (1200ms)
        let msg = TestMessage {
            data: vec![1, 2, 3],
        };
        channel.send(&msg, MDU, 1000, 1200).unwrap();

        // Deliver it: handshake RTT=1200ms, sample RTT=(1200-1000)=200ms
        channel.mark_delivered(0, 1200, 1200);

        // Window tier should remain SLOW because handshake RTT (1200ms) >= CHANNEL_RTT_MEDIUM_MS (750ms)
        assert_eq!(
            channel.window_max(),
            CHANNEL_WINDOW_MAX_SLOW,
            "window_max should stay SLOW (5) because handshake RTT 1200ms > 750ms threshold"
        );

        // SRTT should be ~200ms (fast), but that doesn't affect the tier
        assert!(
            (channel.srtt_ms() - 200.0).abs() < 1.0,
            "SRTT should be ~200ms from sample, got {}",
            channel.srtt_ms()
        );
    }

    // T9: Channel Edge Cases
    #[test]
    fn test_poll_empty_tx_ring() {
        let mut channel = Channel::new();
        // Poll on empty channel, should return no actions
        let actions = channel.poll(1000, 500);
        assert!(actions.is_empty());
    }

    #[test]
    fn test_poll_after_delivery_no_retransmit() {
        let mut channel = Channel::new();
        let msg = TestMessage { data: vec![1] };

        // Send a message, then mark it delivered
        channel.send(&msg, MDU, 1000, 100).unwrap();
        assert_eq!(channel.outstanding(), 1);
        channel.mark_delivered(0, 1100, 100);
        assert_eq!(channel.outstanding(), 0);

        // Poll should return no actions (tx_ring is empty)
        let actions = channel.poll(100_000, 100);
        assert!(actions.is_empty());
    }

    #[test]
    fn test_window_floor_enforcement() {
        let mut channel = Channel::new();
        channel.set_window_bounds_for_test(3, 2, 5);

        // Decrement window: 3 → 2 (use rtt < 2000 to stay in SLOW tier)
        channel.adjust_window(false, 1999);
        assert_eq!(channel.window(), 2);

        // Decrement again: should stay at 2 (floor = window_min)
        channel.adjust_window(false, 1999);
        assert_eq!(channel.window(), 2, "window should not go below window_min");
    }

    #[test]
    fn test_adjust_window_increment_to_max() {
        let mut channel = Channel::new();
        channel.set_window_bounds_for_test(3, 2, 5);

        // Increment: 3 → 4 → 5 (use rtt < 2000 to stay in SLOW tier)
        channel.adjust_window(true, 1999);
        assert_eq!(channel.window(), 4);
        channel.adjust_window(true, 1999);
        assert_eq!(channel.window(), 5);

        // At max, should not go higher
        channel.adjust_window(true, 1999);
        assert_eq!(channel.window(), 5, "window should not exceed window_max");
    }

    #[test]
    fn test_pacing_uses_srtt_not_handshake_rtt() {
        // Handshake RTT is high (1200ms), but actual RTT is much faster (200ms).
        // After SRTT is measured, pacing should use SRTT, not handshake RTT.
        let mut channel = Channel::new();
        let handshake_rtt = 1200u64;

        // Send first message at t=1000
        let msg1 = TestMessage {
            data: vec![1, 2, 3],
        };
        channel.send(&msg1, MDU, 1000, handshake_rtt).unwrap();

        // Deliver at t=1200 → sample RTT = 200ms, SRTT = 200.0 (first sample)
        channel.mark_delivered(0, 1200, handshake_rtt);
        assert!(
            (channel.srtt_ms() - 200.0).abs() < 1.0,
            "SRTT should be ~200ms"
        );

        // Send and deliver a second message to trigger adjust_window + recalculate_pacing
        let msg2 = TestMessage {
            data: vec![4, 5, 6],
        };
        channel.send(&msg2, MDU, 1300, handshake_rtt).unwrap();
        channel.mark_delivered(1, 1500, handshake_rtt);

        // SRTT should still be ~200ms (second sample also 200ms)
        let srtt = channel.srtt_ms() as u64;
        assert!(
            (180..=220).contains(&srtt),
            "SRTT should be ~200ms, got {}",
            srtt
        );

        // Pacing should be based on SRTT (~200ms), NOT handshake RTT (1200ms)
        let expected_pacing = srtt / channel.window() as u64;
        assert_eq!(
            channel.pacing_interval_ms(),
            expected_pacing,
            "pacing should be SRTT/window = {}/{} = {}, got {}",
            srtt,
            channel.window(),
            expected_pacing,
            channel.pacing_interval_ms()
        );

        // Specifically: pacing must NOT be based on handshake RTT
        let handshake_pacing = handshake_rtt / channel.window() as u64;
        assert!(
            channel.pacing_interval_ms() < handshake_pacing,
            "pacing {} should be less than handshake-based pacing {}",
            channel.pacing_interval_ms(),
            handshake_pacing
        );
    }

    // Deferred Proof Tests
    #[test]
    fn test_receive_buffered_no_proof() {
        let mut channel = Channel::new();

        // Send seq 1 (skip 0), out of order
        let env1 = Envelope::new(0x0001, 1, vec![42]);
        let result = channel.receive(&env1.pack(), [1u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::Buffered));
        // next_rx_sequence unchanged
        assert_eq!(channel.next_rx_sequence(), 0);
    }

    #[test]
    fn test_receive_in_order_delivered_outcome() {
        let mut channel = Channel::new();

        let env0 = Envelope::new(0x0001, 0, vec![10, 20]);
        let result = channel.receive(&env0.pack(), [0u8; 32]);
        match result.unwrap() {
            ReceiveOutcome::Delivered(env) => {
                assert_eq!(env.sequence, 0);
                assert_eq!(env.data, vec![10, 20]);
            }
            other => panic!("expected Delivered, got {:?}", other),
        }
        assert_eq!(channel.next_rx_sequence(), 1);
    }

    #[test]
    fn test_receive_already_delivered_reprove() {
        let mut channel = Channel::new();

        // Deliver seq 0
        let env0 = Envelope::new(0x0001, 0, vec![1]);
        let result = channel.receive(&env0.pack(), [0u8; 32]);
        assert!(matches!(result, Ok(ReceiveOutcome::Delivered(_))));

        // Send seq 0 again, already delivered, should re-prove
        let env0_dup = Envelope::new(0x0001, 0, vec![1]);
        let result = channel.receive(&env0_dup.pack(), [0u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::AlreadyDelivered));
    }

    #[test]
    fn test_receive_duplicate_buffered() {
        let mut channel = Channel::new();

        // Buffer seq 2 (out of order)
        let env2 = Envelope::new(0x0001, 2, vec![42]);
        let result = channel.receive(&env2.pack(), [2u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::Buffered));

        // Send seq 2 again, duplicate of buffered
        let env2_dup = Envelope::new(0x0001, 2, vec![42]);
        let result = channel.receive(&env2_dup.pack(), [22u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::DuplicateBuffered));
    }

    #[test]
    fn test_drain_returns_proof_hashes() {
        let mut channel = Channel::new();

        // Buffer seq 1, 2, 3 with distinct proof hashes
        let hash1 = [1u8; 32];
        let hash2 = [2u8; 32];
        let hash3 = [3u8; 32];
        let env1 = Envelope::new(0x0001, 1, vec![10]);
        let env2 = Envelope::new(0x0001, 2, vec![20]);
        let env3 = Envelope::new(0x0001, 3, vec![30]);
        assert_eq!(
            channel.receive(&env1.pack(), hash1),
            Ok(ReceiveOutcome::Buffered)
        );
        assert_eq!(
            channel.receive(&env2.pack(), hash2),
            Ok(ReceiveOutcome::Buffered)
        );
        assert_eq!(
            channel.receive(&env3.pack(), hash3),
            Ok(ReceiveOutcome::Buffered)
        );

        // Deliver seq 0 to unblock the drain
        let env0 = Envelope::new(0x0001, 0, vec![0]);
        assert!(matches!(
            channel.receive(&env0.pack(), [0u8; 32]),
            Ok(ReceiveOutcome::Delivered(_))
        ));

        // Drain returns 3 entries with correct proof hashes
        let drained = channel.drain_received();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].0.sequence, 1);
        assert_eq!(drained[0].1, hash1);
        assert_eq!(drained[1].0.sequence, 2);
        assert_eq!(drained[1].1, hash2);
        assert_eq!(drained[2].0.sequence, 3);
        assert_eq!(drained[2].1, hash3);
        assert_eq!(channel.next_rx_sequence(), 4);
    }

    #[test]
    fn test_retransmit_fires_for_unproved() {
        // Sender sends seq 0 and 1. Only seq 0 gets proved (mark_delivered).
        // After timeout, poll() should retransmit seq 1.
        let mut channel = Channel::new();
        let msg0 = TestMessage { data: vec![0] };
        let msg1 = TestMessage { data: vec![1] };

        channel.send(&msg0, MDU, 1000, 100).unwrap();
        channel.send(&msg1, MDU, 1000, 100).unwrap();
        assert_eq!(channel.outstanding(), 2);

        // Prove seq 0 only
        assert!(channel.mark_delivered(0, 1100, 100));
        assert_eq!(channel.outstanding(), 1);

        // Advance well past timeout for seq 1
        let actions = channel.poll(10000, 100);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, ChannelAction::Retransmit { sequence: 1, .. })),
            "expected retransmit for seq 1, got {:?}",
            actions
        );
    }
}
