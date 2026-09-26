//! Persistence trait for packet deduplication hashes.
//!
//! Packet hashes (SHA-256, 32 bytes) are stored for deduplication.
//! Each target serializes in its own format:
//! - std: msgpack array (Python Reticulum compatible)
//! - embedded: compact binary (future)

extern crate alloc;
use alloc::vec::Vec;

/// Persistent storage for packet deduplication hashes.
pub trait PacketHashStore {
    /// Error type for storage operations.
    type Error: core::fmt::Debug;

    /// Load all stored packet hashes.
    fn load_all(&mut self) -> Result<Vec<[u8; 32]>, Self::Error>;

    /// Save all packet hashes to persistent storage (replaces previous content).
    fn save_all(&mut self, hashes: &[[u8; 32]]) -> Result<(), Self::Error>;
}
