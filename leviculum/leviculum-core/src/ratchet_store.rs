//! Persistence trait for ratchet keys (forward secrecy).
//!
//! Two types of ratchet data:
//! - Known ratchets (receiver-side): ratchet public key + received timestamp
//! - Dest ratchet keys (sender-side): opaque serialized bytes
//!
//! Both are stored per-destination (keyed by truncated hash).
//! On std, each destination is a separate file in `ratchets/` or `ratchetkeys/`.

extern crate alloc;
use alloc::vec::Vec;

use crate::constants::{RATCHET_SIZE, TRUNCATED_HASHBYTES};

/// A receiver-side known ratchet: public key + wall-clock receive time.
pub struct KnownRatchetEntry {
    /// Ratchet public key (32 bytes).
    pub ratchet: [u8; RATCHET_SIZE],
    /// Wall-clock seconds since epoch when this ratchet was received.
    pub received_at_secs: f64,
}

/// Persistent storage for ratchet keys.
///
/// Ratchets use write-through persistence: each update is immediately
/// written to storage. The load methods are called once at startup.
pub trait RatchetStore {
    /// Error type for storage operations.
    type Error: core::fmt::Debug;

    /// Load all known ratchets (receiver-side) from storage.
    fn load_known_ratchets(
        &mut self,
    ) -> Result<Vec<([u8; TRUNCATED_HASHBYTES], KnownRatchetEntry)>, Self::Error>;

    /// Save a single known ratchet (write-through).
    fn save_known_ratchet(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
        entry: &KnownRatchetEntry,
    ) -> Result<(), Self::Error>;

    /// Load all dest ratchet keys (sender-side) from storage.
    #[allow(clippy::type_complexity)]
    fn load_dest_ratchet_keys(
        &mut self,
    ) -> Result<Vec<([u8; TRUNCATED_HASHBYTES], Vec<u8>)>, Self::Error>;

    /// Save a single dest ratchet key set (write-through).
    fn save_dest_ratchet_keys(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
        serialized: &[u8],
    ) -> Result<(), Self::Error>;
}
