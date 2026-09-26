//! Hash functions

use sha2::{Digest, Sha256, Sha512};

use crate::constants::TRUNCATED_HASHBYTES;

/// Compute SHA-256 hash
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Compute SHA-512 hash
pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Compute full SHA-256 hash
/// Returns the complete 32-byte hash
pub fn full_hash(data: &[u8]) -> [u8; 32] {
    sha256(data)
}

/// Compute truncated hash (first 16 bytes of SHA-256)
/// Used for destination addresses
pub fn truncated_hash(data: &[u8]) -> [u8; TRUNCATED_HASHBYTES] {
    let hash = sha256(data);
    let mut result = [0u8; TRUNCATED_HASHBYTES];
    result.copy_from_slice(&hash[..TRUNCATED_HASHBYTES]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_empty() {
        let expected = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(sha256(b""), expected);
    }

    #[test]
    fn test_sha256_hello() {
        let hash = sha256(b"hello");
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn test_truncated_hash() {
        let hash = truncated_hash(b"test data");
        assert_eq!(hash.len(), TRUNCATED_HASHBYTES);
    }
}
