//! Ratchet key management for forward secrecy
//!
//! Ratchets provide forward secrecy for packets sent to SINGLE destinations
//! without establishing a Link. Each destination periodically rotates X25519
//! key pairs and announces the current public key.
//!
//! # How it works
//!
//! 1. A destination enables ratchets and generates an initial keypair
//! 2. The destination announces with the ratchet public key
//! 3. Senders use the ratchet public key for encryption instead of identity key
//! 4. The destination rotates ratchets periodically (default: 30 minutes)
//! 5. Old ratchets are retained for decryption (default: 512 ratchets)
//!
//! # Forward Secrecy
//!
//! If a ratchet private key is compromised, only messages encrypted with
//! that specific ratchet can be decrypted. Past and future ratchets remain
//! secure because each ratchet is independently generated.

use crate::constants::{RATCHET_EXPIRY_SECS, RATCHET_INTERVAL_SECS, RATCHET_SIZE};
use crate::crypto::sha256;
use rand_core::CryptoRngCore;

/// Ratchet ID size (first 10 bytes of SHA256(public_key))
pub(crate) const RATCHET_ID_SIZE: usize = 10;

/// Default number of ratchets to retain for decryption
pub(crate) const DEFAULT_RETAINED_RATCHETS: usize = 512;

/// A ratchet key pair (X25519) for forward secrecy
///
/// Ratchets are ephemeral key pairs that are rotated periodically.
/// The private key is used for decryption, the public key is announced.
#[derive(Clone)]
pub(crate) struct Ratchet {
    /// X25519 private key (32 bytes)
    private_key: x25519_dalek::StaticSecret,
    /// X25519 public key (32 bytes)
    public_key: x25519_dalek::PublicKey,
    /// Timestamp when this ratchet was created (milliseconds)
    created_at_ms: u64,
}

impl Ratchet {
    /// Generate a new ratchet key pair
    ///
    /// # Arguments
    /// * `rng` - Random number generator
    /// * `now_ms` - Current timestamp in milliseconds
    pub(crate) fn generate<R: CryptoRngCore>(rng: &mut R, now_ms: u64) -> Self {
        let private_key = x25519_dalek::StaticSecret::random_from_rng(rng);
        let public_key = x25519_dalek::PublicKey::from(&private_key);

        Self {
            private_key,
            public_key,
            created_at_ms: now_ms,
        }
    }

    /// Get the public key bytes (32 bytes)
    pub(crate) fn public_key_bytes(&self) -> [u8; RATCHET_SIZE] {
        *self.public_key.as_bytes()
    }

    /// Get the ratchet ID (first 10 bytes of SHA256(public_key))
    ///
    /// The ratchet ID is used to identify which ratchet was used for encryption.
    pub(crate) fn id(&self) -> [u8; RATCHET_ID_SIZE] {
        ratchet_id(&self.public_key_bytes())
    }

    /// Get the X25519 private key reference (for ECDH operations)
    pub(crate) fn private_key(&self) -> &x25519_dalek::StaticSecret {
        &self.private_key
    }

    /// Get the raw X25519 private key bytes (32 bytes).
    ///
    /// Used for Python-compatible serialization which stores only private keys
    /// (public keys are derived on load).
    pub(crate) fn private_key_bytes(&self) -> [u8; RATCHET_SIZE] {
        *self.private_key.as_bytes()
    }

    /// Reconstruct a Ratchet from raw private key bytes.
    ///
    /// Derives the public key from the private key. Used when loading from
    /// Python-compatible format which stores only private keys, no timestamps.
    pub(crate) fn from_private_key_bytes(key: [u8; RATCHET_SIZE], created_at_ms: u64) -> Self {
        let private_key = x25519_dalek::StaticSecret::from(key);
        let public_key = x25519_dalek::PublicKey::from(&private_key);
        Self {
            private_key,
            public_key,
            created_at_ms,
        }
    }
}

impl core::fmt::Debug for Ratchet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ratchet")
            .field("id", &format_args!("{:02x?}", &self.id()[..4]))
            .field("created_at_ms", &self.created_at_ms)
            .finish()
    }
}

/// Compute ratchet ID from public key bytes
///
/// The ratchet ID is the first 10 bytes of SHA256(public_key).
/// This matches the Python Reticulum implementation.
pub(crate) fn ratchet_id(public_key: &[u8; RATCHET_SIZE]) -> [u8; RATCHET_ID_SIZE] {
    let hash = sha256(public_key);
    let mut id = [0u8; RATCHET_ID_SIZE];
    id.copy_from_slice(&hash[..RATCHET_ID_SIZE]);
    id
}

/// Default rotation interval in milliseconds
pub(crate) const DEFAULT_INTERVAL_MS: u64 = RATCHET_INTERVAL_SECS * 1000;

/// Expiry time in milliseconds
pub(crate) const EXPIRY_MS: u64 = RATCHET_EXPIRY_SECS * 1000;

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    // Ratchet Tests
    #[test]
    fn test_ratchet_generation() {
        let ratchet = Ratchet::generate(&mut OsRng, 1704067200000);

        assert_eq!(ratchet.public_key_bytes().len(), RATCHET_SIZE);
        assert_eq!(ratchet.id().len(), RATCHET_ID_SIZE);
    }

    #[test]
    fn test_ratchet_unique_keys() {
        let ratchet1 = Ratchet::generate(&mut OsRng, 1704067200000);
        let ratchet2 = Ratchet::generate(&mut OsRng, 1704067200000);

        // Each ratchet should have unique keys
        assert_ne!(ratchet1.public_key_bytes(), ratchet2.public_key_bytes());
        assert_ne!(ratchet1.id(), ratchet2.id());
    }

    #[test]
    fn test_ratchet_id_function() {
        let public_key = [0xab; RATCHET_SIZE];
        let id = ratchet_id(&public_key);

        assert_eq!(id.len(), RATCHET_ID_SIZE);

        // Same input should produce same ID
        let id2 = ratchet_id(&public_key);
        assert_eq!(id, id2);

        // Different input should produce different ID
        let other_key = [0xcd; RATCHET_SIZE];
        let other_id = ratchet_id(&other_key);
        assert_ne!(id, other_id);
    }

    #[test]
    fn test_private_key_bytes_roundtrip() {
        let original = Ratchet::generate(&mut OsRng, 1704067200000);
        let private_bytes = original.private_key_bytes();

        let restored = Ratchet::from_private_key_bytes(private_bytes, 5000);

        // Same public key (derived from private key)
        assert_eq!(original.public_key_bytes(), restored.public_key_bytes());
        assert_eq!(original.id(), restored.id());
    }
}
