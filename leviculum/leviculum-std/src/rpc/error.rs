//! RPC error types

/// Errors that can occur during RPC communication.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RpcError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("authentication failed")]
    AuthFailed,

    #[error("invalid message format: {0}")]
    InvalidFormat(String),

    #[error("pickle error: {0}")]
    Pickle(String),
}
