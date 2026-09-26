//! leviculum-std: Standard library extensions for reticulum
//!
//! This crate provides std-dependent functionality:
//! - Network interfaces (TCP, UDP, Local/IPC)
//! - Serial interfaces (KISS, RNode)
//! - Configuration loading and persistence
//! - File-based storage
//! - Async runtime integration (tokio)
//!
//! Use leviculum-core for the no_std compatible core functionality,
//! including the buffer system types (RawChannelReader, RawChannelWriter).

#![warn(unreachable_pub)]

pub mod api;
pub(crate) mod autoconnect;
pub(crate) mod clock;
pub mod config;
pub(crate) mod discovery;
pub mod driver;
pub mod error;
pub mod tokio_serial;
pub mod event_log;
pub mod file_identity_store;
pub(crate) mod file_known_destinations_store;
pub mod file_lxmf_store;
pub(crate) mod file_packet_hash_store;
pub(crate) mod file_ratchet_store;
/// Fuzzing-only entry points exposing crate-internal parsers to the detached
/// cargo-fuzz harness. Compiled only under `--cfg fuzzing` (set by cargo-fuzz).
#[cfg(fuzzing)]
pub mod fuzz;
pub(crate) mod ini_config;
pub mod interfaces;
pub(crate) mod known_destinations;
pub(crate) mod packet_hashlist;
pub mod process;
pub mod remote_status;
pub mod resource_policy;
pub mod reticulum;
#[cfg(feature = "rpc")]
pub(crate) mod rpc;
pub mod socket_hook;
pub(crate) mod storage;
pub(crate) mod sync_ext;
pub mod test_support;
pub mod user;

// Re-export commonly used core types for the high-level API
pub use leviculum_core::node::{DeliveryError, EventClass, LinkStats, NodeEvent};
pub use leviculum_core::transport::{InterfaceId, PathTableExport};
pub use leviculum_core::{
    AnnounceError, Destination, DestinationHash, DestinationType, Direction, Identity,
    LinkCloseReason, LinkError, LinkId, PeerKeys, ProofStrategy, ReceivedAnnounce, SendError,
    TransportStats,
};

/// Generate a new random identity using the system RNG.
///
/// Convenience wrapper around `Identity::generate(&mut OsRng)` for std apps.
/// Embedded code should use `Identity::generate()` with a platform-specific RNG.
pub fn generate_identity() -> Identity {
    Identity::generate(&mut rand_core::OsRng)
}

pub use config::Config;
pub use driver::{
    Completion, CompletionError, EventReceiver, EventTap, FilteredEventTap,
    InterfaceStatusSnapshot, LinkEstablishedFuture, LinkHandle, PacketSender,
    RequestResponseFuture, ResourceSentFuture, ResourceSentInfo, ResponseInfo, ReticulumNode,
    ReticulumNodeBuilder, TapEvent, DEFAULT_EVENT_TAP_CAPACITY,
};
pub use error::{Error, Result};
pub use file_lxmf_store::FileLxmfStorage;
pub use reticulum::Reticulum;
/// Client for the shared-instance RPC socket (`rnstatus`/`rnpath` protocol).
/// Used by `lnstest diag` to query a running `lnsd`/`rnsd`.
#[cfg(feature = "rpc")]
#[cfg(feature = "rpc")]
pub use rpc::{rpc_query, rpc_query_hash_param};
