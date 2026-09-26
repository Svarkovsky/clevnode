//! Send error types for node operations

/// Error type for send operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// No path to the destination
    NoPath,
    /// Data too large for single packet (use Link/Channel instead)
    TooLarge,
    /// No existing link and couldn't establish one
    NoLink,
    /// Link failed
    LinkFailed,
    /// Send path is occupied, try later (mirrors [`crate::link::channel::ChannelError::Busy`])
    Busy,
    /// Channel is pacing sends, retry at the given time (mirrors [`crate::link::channel::ChannelError::PacingDelay`])
    PacingDelay { ready_at_ms: u64 },
    /// Encryption failed (identity not found in known_identities, or crypto error)
    EncryptionFailed,
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SendError::NoPath => write!(f, "no path to destination"),
            SendError::TooLarge => write!(f, "data too large for single packet"),
            SendError::NoLink => write!(f, "no link available"),
            SendError::LinkFailed => write!(f, "link failed"),
            SendError::Busy => write!(f, "busy"),
            SendError::PacingDelay { ready_at_ms } => {
                write!(f, "pacing delay until {}ms", ready_at_ms)
            }
            SendError::EncryptionFailed => write!(f, "encryption failed"),
        }
    }
}
