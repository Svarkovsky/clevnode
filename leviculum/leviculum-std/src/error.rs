//! Error types for leviculum-std

use thiserror::Error;

use leviculum_core::resource::ResourceError;
use leviculum_core::{AnnounceError, LinkError, RequestError, SendError};

/// Main error type for reticulum operations
#[derive(Error, Debug)]
pub enum Error {
    /// I/O error
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration error
    #[error("configuration error: {0}")]
    Config(String),

    /// The shared-instance name is already served by another daemon.
    ///
    /// Distinguished from a bare [`Error::Io`] because the remedy is
    /// specific and the raw `AddrInUse` hides it. The instance name *is*
    /// the identity of a shared instance, so this is not a transient
    /// bind failure to retry: either the other daemon is the one that
    /// should be running, or this one needs a different name.
    ///
    /// Note that a program wanting to *use* the running daemon does not
    /// belong here at all — that is
    /// [`connect_to_shared_instance`](crate::driver::ReticulumNodeBuilder::connect_to_shared_instance),
    /// the client path.
    #[error(
        "shared-instance name {name:?} is already served by another Reticulum \
         daemon on this host (lnsd or rnsd). Stop that daemon, or give this one \
         a different instance_name in its config."
    )]
    SharedInstanceNameInUse {
        /// The instance name that could not be bound.
        name: String,
    },

    /// Storage error
    #[error("storage error: {0}")]
    Storage(String),

    /// Serialization error
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Event loop is not running (channel closed or node stopped)
    #[error("node event loop is not running")]
    NotRunning,

    /// Announce failed
    #[error("announce error: {0}")]
    Announce(AnnounceError),

    /// Send failed
    #[error("send error: {0}")]
    Send(SendError),

    /// Link operation failed
    #[error("link error: {0}")]
    Link(LinkError),

    /// Resource transfer operation failed
    #[error("resource error: {0}")]
    Resource(ResourceError),

    /// Request/response operation failed
    #[error("request error: {0}")]
    Request(RequestError),
}

impl From<AnnounceError> for Error {
    fn from(e: AnnounceError) -> Self {
        Error::Announce(e)
    }
}

impl From<SendError> for Error {
    fn from(e: SendError) -> Self {
        Error::Send(e)
    }
}

impl From<LinkError> for Error {
    fn from(e: LinkError) -> Self {
        Error::Link(e)
    }
}

impl From<ResourceError> for Error {
    fn from(e: ResourceError) -> Self {
        Error::Resource(e)
    }
}

impl From<RequestError> for Error {
    fn from(e: RequestError) -> Self {
        Error::Request(e)
    }
}

/// Result type alias
pub type Result<T> = std::result::Result<T, Error>;
