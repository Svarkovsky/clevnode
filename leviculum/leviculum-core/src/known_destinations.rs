//! Data types and trait for known destinations persistence.
//!
//! Known destinations track peer identities discovered via announces.
//! Each target serializes these in its own format:
//! - std: msgpack (Python Reticulum compatible)
//! - embedded: compact binary records (future)

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::constants::{IDENTITY_KEY_SIZE, TRUNCATED_HASHBYTES};

/// Announce packet hash length (SHA-256).
pub const PACKET_HASH_LEN: usize = 32;

/// A known destination entry, a peer identity discovered from an announce.
#[derive(Clone)]
pub struct KnownDestEntry {
    /// Seconds since Unix epoch (Python `time.time()` compatible).
    pub timestamp: f64,
    /// Original announce packet hash (32 bytes).
    pub packet_hash: Vec<u8>,
    /// Combined public key: X25519(32) | Ed25519(32) = 64 bytes.
    pub public_key: [u8; IDENTITY_KEY_SIZE],
    /// Optional application data from the announce.
    pub app_data: Option<Vec<u8>>,
}

/// Persistent storage for known destination identities.
///
/// Implemented per target:
/// - std: msgpack file (Python Reticulum compatible)
/// - embedded: compact binary records in flash (future)
pub trait KnownDestinationsStore {
    /// Error type for storage operations.
    type Error: core::fmt::Debug;

    /// Load all known destination entries from persistent storage.
    fn load_all(
        &mut self,
    ) -> Result<BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>, Self::Error>;

    /// Save all known destination entries to persistent storage.
    fn save_all(
        &mut self,
        entries: &BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>,
    ) -> Result<(), Self::Error>;
}
