//! Persistent storage for a node's own identity.
//!
//! Each target implements [`IdentityStore`] with its own backend (NVMC, SPI flash,
//! filesystem). The wire format is shared: all targets encode and decode
//! identity records identically.

use crate::constants::IDENTITY_KEY_SIZE;
use crate::identity::Identity;

// Wire format
const MAGIC: [u8; 4] = [0x52, 0x54, 0x49, 0x43]; // "RTIC"
const FORMAT_VERSION: u8 = 0x01;
const HEADER_SIZE: usize = 5; // magic(4) + version(1)
const CHECKSUM_SIZE: usize = 2;

/// Total encoded identity size: 5 header + 64 key + 2 checksum = 71 bytes.
pub const ENCODED_SIZE: usize = HEADER_SIZE + IDENTITY_KEY_SIZE + CHECKSUM_SIZE;

/// Encoded size rounded up to 4-byte alignment (for flash write requirements).
pub const ENCODED_SIZE_ALIGNED: usize = (ENCODED_SIZE + 3) & !3; // 72

fn checksum(data: &[u8]) -> [u8; 2] {
    let mut a: u8 = 0;
    let mut b: u8 = 0;
    for (i, &byte) in data.iter().enumerate() {
        if i % 2 == 0 {
            a ^= byte;
        } else {
            b ^= byte;
        }
    }
    [a, b]
}

/// Encode an identity into a fixed-size buffer for persistent storage.
///
/// Returns `None` if the identity has no private keys.
pub fn encode_identity(identity: &Identity) -> Option<[u8; ENCODED_SIZE_ALIGNED]> {
    let key = identity.private_key_bytes().ok()?;
    let mut buf = [0u8; ENCODED_SIZE_ALIGNED];
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4] = FORMAT_VERSION;
    buf[HEADER_SIZE..HEADER_SIZE + IDENTITY_KEY_SIZE].copy_from_slice(&key);
    let cs = checksum(&buf[..HEADER_SIZE + IDENTITY_KEY_SIZE]);
    buf[HEADER_SIZE + IDENTITY_KEY_SIZE] = cs[0];
    buf[HEADER_SIZE + IDENTITY_KEY_SIZE + 1] = cs[1];
    Some(buf)
}

/// Decode an identity from a persistent storage buffer.
///
/// Returns `None` if magic, version, or checksum don't match,
/// or if the buffer contains erased flash (0xFF).
pub fn decode_identity(buf: &[u8]) -> Option<Identity> {
    if buf.len() < ENCODED_SIZE {
        return None;
    }
    if buf[0] == 0xFF {
        return None;
    } // erased flash
    if buf[0..4] != MAGIC {
        return None;
    }
    if buf[4] != FORMAT_VERSION {
        return None;
    }

    let key = &buf[HEADER_SIZE..HEADER_SIZE + IDENTITY_KEY_SIZE];
    let stored = [
        buf[HEADER_SIZE + IDENTITY_KEY_SIZE],
        buf[HEADER_SIZE + IDENTITY_KEY_SIZE + 1],
    ];
    if stored != checksum(&buf[..HEADER_SIZE + IDENTITY_KEY_SIZE]) {
        return None;
    }

    Identity::from_private_key_bytes(key).ok()
}

// Trait
/// Persistent storage for a node's own Reticulum identity.
///
/// Implemented per target:
/// - nRF52840: internal flash via NVMC
/// - ESP32: SPI flash partition
/// - std: file on disk
pub trait IdentityStore {
    /// Error type for storage operations.
    type Error: core::fmt::Debug;
    /// Load the stored identity, or `None` if no valid identity exists.
    fn load(&mut self) -> Result<Option<Identity>, Self::Error>;
    /// Save an identity to persistent storage.
    fn save(&mut self, identity: &Identity) -> Result<(), Self::Error>;
}

// Tests
#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    #[test]
    fn roundtrip() {
        let identity = Identity::generate(&mut OsRng);
        let buf = encode_identity(&identity).unwrap();
        let decoded = decode_identity(&buf).unwrap();
        assert_eq!(decoded.hash(), identity.hash());
    }

    #[test]
    fn erased_flash_returns_none() {
        let buf = [0xFF; ENCODED_SIZE_ALIGNED];
        assert!(decode_identity(&buf).is_none());
    }

    #[test]
    fn bad_magic_returns_none() {
        let identity = Identity::generate(&mut OsRng);
        let mut buf = encode_identity(&identity).unwrap();
        buf[0] = 0x00;
        assert!(decode_identity(&buf).is_none());
    }

    #[test]
    fn bad_checksum_returns_none() {
        let identity = Identity::generate(&mut OsRng);
        let mut buf = encode_identity(&identity).unwrap();
        buf[ENCODED_SIZE - 1] ^= 0xFF;
        assert!(decode_identity(&buf).is_none());
    }

    #[test]
    fn wrong_version_returns_none() {
        let identity = Identity::generate(&mut OsRng);
        let mut buf = encode_identity(&identity).unwrap();
        buf[4] = 0x99;
        assert!(decode_identity(&buf).is_none());
    }

    #[test]
    fn short_buffer_returns_none() {
        assert!(decode_identity(&[0x52, 0x54]).is_none());
    }

    #[test]
    fn encoded_size_is_correct() {
        assert_eq!(ENCODED_SIZE, 71);
        assert_eq!(ENCODED_SIZE_ALIGNED, 72);
    }
}
