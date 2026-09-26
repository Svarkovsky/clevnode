//! Request/response types for Link packet and Resource transport.
//!
//! Payloads that fit the Link MDU use request/response packets. Larger request
//! and response bodies use request-correlated Resources while preserving the
//! same handler, response, timeout, and failure semantics.

use alloc::string::String;
use alloc::vec::Vec;

use crate::constants::TRUNCATED_HASHBYTES;
use crate::link::LinkId;

/// Policy controlling which identities may send requests to a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestPolicy {
    /// Silently drop all requests (0x00)
    AllowNone,
    /// Allow requests from any identity (0x01)
    AllowAll,
    /// Allow requests only from identities in the list (0x02)
    AllowList(Vec<[u8; TRUNCATED_HASHBYTES]>),
}

/// A registered request handler entry. Keyed by (destination_hash,
/// path_hash) in the storage map, so those live in the key, not here.
///
/// Cleanup: entries removed via `deregister_request_handler()`.
pub(super) struct RequestHandlerEntry {
    pub(super) path: String,
    pub(super) policy: RequestPolicy,
}

/// A pending outgoing request awaiting a response.
///
/// Cleanup: removed when (a) response arrives, (b) timeout fires,
/// (c) a correlated Resource transfer fails, or (d) the link closes
/// (`emit_link_closed` cleans all for that link).
pub(super) struct PendingRequest {
    pub(super) link_id: LinkId,
    pub(super) request_id: [u8; TRUNCATED_HASHBYTES],
    /// Start of the response-wait phase.
    ///
    /// Packet requests enter this phase when sent. Resource requests remain
    /// `None` while their request body is uploading and are armed only after
    /// the sender receives the Resource completion proof. An accepted response
    /// Resource suspends this timer again while its own watchdog owns liveness.
    pub(super) response_started_at_ms: Option<u64>,
    pub(super) timeout_ms: u64,
}

/// Errors that can occur during request/response operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// The specified link was not found
    LinkNotFound,
    /// The link is not in Active state
    LinkNotActive,
    /// The encoded payload exceeds the link MDU
    PayloadTooLarge,
    /// The specified request was not found
    RequestNotFound,
    /// Packet encryption failed
    EncryptionFailed,
    /// Msgpack encoding failed
    EncodingFailed,
}

impl core::fmt::Display for RequestError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RequestError::LinkNotFound => write!(f, "link not found"),
            RequestError::LinkNotActive => write!(f, "link not active"),
            RequestError::PayloadTooLarge => write!(f, "payload exceeds link MDU"),
            RequestError::RequestNotFound => write!(f, "request not found"),
            RequestError::EncryptionFailed => write!(f, "packet encryption failed"),
            RequestError::EncodingFailed => write!(f, "msgpack encoding failed"),
        }
    }
}
