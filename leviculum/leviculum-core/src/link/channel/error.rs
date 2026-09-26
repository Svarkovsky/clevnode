//! Channel error types

/// Errors that can occur during channel operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    /// Invalid message type (>= 0xf000 reserved)
    InvalidMsgType,
    /// Message too large for link MDU
    TooLarge,
    /// Envelope header is incomplete (< 6 bytes)
    EnvelopeTooShort,
    /// Envelope data is shorter than the declared length
    EnvelopeTruncated,
    /// Invalid stream_id (> STREAM_ID_MAX)
    InvalidStreamId,
    /// Send path is occupied, try later
    Busy,
    /// Channel is pacing sends, retry at the given time
    PacingDelay { ready_at_ms: u64 },
    /// Receive ring is full (message dropped)
    RxRingFull,
}

impl core::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ChannelError::InvalidMsgType => write!(f, "invalid message type"),
            ChannelError::TooLarge => write!(f, "message too large for link MDU"),
            ChannelError::EnvelopeTooShort => write!(f, "envelope header too short"),
            ChannelError::EnvelopeTruncated => write!(f, "envelope data truncated"),
            ChannelError::InvalidStreamId => write!(f, "invalid stream ID"),
            ChannelError::Busy => write!(f, "busy"),
            ChannelError::PacingDelay { ready_at_ms } => {
                write!(f, "pacing delay until {}ms", ready_at_ms)
            }
            ChannelError::RxRingFull => write!(f, "receive ring full"),
        }
    }
}
