//! Resource hashmap computation for part verification.
//!
//! Each resource data part has a 4-byte "map hash" that the receiver uses
//! to verify it received the correct part. The hash is computed over the
//! part data concatenated with a random hash unique to the resource.

use crate::constants::RESOURCE_HASHMAP_LEN;
use crate::crypto::full_hash;
use alloc::vec::Vec;

/// Compute the 4-byte map hash for a resource part.
///
/// Python equivalent: `RNS.Identity.full_hash(data + random_hash)[:MAPHASH_LEN]`
///
/// # Arguments
/// * `part_data` - The raw bytes of this resource part
/// * `random_hash` - The 4-byte random hash unique to this resource transfer
pub(crate) fn map_hash(part_data: &[u8], random_hash: &[u8]) -> [u8; RESOURCE_HASHMAP_LEN] {
    let mut input = Vec::with_capacity(part_data.len() + random_hash.len());
    input.extend_from_slice(part_data);
    input.extend_from_slice(random_hash);
    let hash = full_hash(&input);
    let mut result = [0u8; RESOURCE_HASHMAP_LEN];
    result.copy_from_slice(&hash[..RESOURCE_HASHMAP_LEN]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_hash_deterministic() {
        let hash1 = map_hash(b"test_data", &[0x01, 0x02, 0x03, 0x04]);
        let hash2 = map_hash(b"test_data", &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_map_hash_matches_full_hash() {
        let part_data = b"test_data";
        let random_hash = [0x01, 0x02, 0x03, 0x04];

        // Compute expected: SHA-256("test_data" + "\x01\x02\x03\x04")[:4]
        let mut combined = Vec::new();
        combined.extend_from_slice(part_data);
        combined.extend_from_slice(&random_hash);
        let expected_full = full_hash(&combined);

        let result = map_hash(part_data, &random_hash);
        assert_eq!(result, expected_full[..RESOURCE_HASHMAP_LEN]);
    }

    #[test]
    fn test_map_hash_different_data_different_hash() {
        let random_hash = [0x01, 0x02, 0x03, 0x04];
        let hash1 = map_hash(b"data_a", &random_hash);
        let hash2 = map_hash(b"data_b", &random_hash);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_map_hash_different_random_different_hash() {
        let hash1 = map_hash(b"same_data", &[0x01, 0x02, 0x03, 0x04]);
        let hash2 = map_hash(b"same_data", &[0x05, 0x06, 0x07, 0x08]);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_map_hash_length() {
        let result = map_hash(b"any data", &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(result.len(), RESOURCE_HASHMAP_LEN);
        assert_eq!(result.len(), 4);
    }
}
