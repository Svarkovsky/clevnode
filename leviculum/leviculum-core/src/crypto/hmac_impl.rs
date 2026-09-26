//! HMAC-SHA256 implementation

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub(super) type HmacSha256 = Hmac<Sha256>;

/// Compute HMAC-SHA256
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Verify HMAC-SHA256
pub fn verify_hmac(key: &[u8], data: &[u8], expected: &[u8]) -> bool {
    let computed = hmac_sha256(key, data);
    // Constant-time comparison
    constant_time_eq(&computed, expected)
}

/// Constant-time comparison to prevent timing attacks
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hmac_sha256() {
        let key = b"secret key";
        let data = b"message";
        let mac = hmac_sha256(key, data);
        assert_eq!(mac.len(), 32);
        assert!(verify_hmac(key, data, &mac));
    }

    #[test]
    fn test_hmac_verify_fail() {
        let key = b"secret key";
        let data = b"message";
        let mac = hmac_sha256(key, data);
        assert!(!verify_hmac(key, b"wrong message", &mac));
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
    }

    // ==================== EDGE CASE TESTS ====================

    #[test]
    fn test_hmac_empty_key() {
        // Empty key is valid for HMAC (padded with zeros internally)
        let key: &[u8] = b"";
        let data = b"message";
        let mac = hmac_sha256(key, data);
        assert_eq!(mac.len(), 32);
        assert!(verify_hmac(key, data, &mac));
    }

    #[test]
    fn test_hmac_empty_data() {
        // Empty data is valid
        let key = b"secret key";
        let data: &[u8] = b"";
        let mac = hmac_sha256(key, data);
        assert_eq!(mac.len(), 32);
        assert!(verify_hmac(key, data, &mac));
    }

    #[test]
    fn test_hmac_empty_key_and_data() {
        // Both empty is valid
        let key: &[u8] = b"";
        let data: &[u8] = b"";
        let mac = hmac_sha256(key, data);
        assert_eq!(mac.len(), 32);
        assert!(verify_hmac(key, data, &mac));
    }

    #[test]
    fn test_hmac_very_long_key() {
        // Keys longer than block size (64 bytes for SHA-256) are hashed first
        let key = [0x42u8; 128]; // Longer than block size
        let data = b"message";
        let mac = hmac_sha256(&key, data);
        assert_eq!(mac.len(), 32);
        assert!(verify_hmac(&key, data, &mac));
    }

    #[test]
    fn test_hmac_exactly_block_size_key() {
        // Key exactly 64 bytes (SHA-256 block size)
        let key = [0x42u8; 64];
        let data = b"message";
        let mac = hmac_sha256(&key, data);
        assert_eq!(mac.len(), 32);
        assert!(verify_hmac(&key, data, &mac));
    }

    #[test]
    fn test_hmac_wrong_key() {
        let key1 = b"correct key";
        let key2 = b"wrong key";
        let data = b"message";
        let mac = hmac_sha256(key1, data);
        // Wrong key should not verify
        assert!(!verify_hmac(key2, data, &mac));
    }

    #[test]
    fn test_hmac_truncated_mac() {
        let key = b"secret key";
        let data = b"message";
        let mac = hmac_sha256(key, data);
        // Truncated MAC should not verify
        assert!(!verify_hmac(key, data, &mac[..16]));
    }

    #[test]
    fn test_hmac_extended_mac() {
        let key = b"secret key";
        let data = b"message";
        let mac = hmac_sha256(key, data);
        // Extended MAC (extra bytes) should not verify
        let mut extended = [0u8; 64];
        extended[..32].copy_from_slice(&mac);
        assert!(!verify_hmac(key, data, &extended));
    }

    #[test]
    fn test_hmac_single_bit_flip() {
        let key = b"secret key";
        let data = b"message";
        let mut mac = hmac_sha256(key, data);
        // Flip a single bit
        mac[0] ^= 0x01;
        assert!(!verify_hmac(key, data, &mac));
    }

    #[test]
    fn test_hmac_deterministic() {
        let key = b"secret key";
        let data = b"message";
        let mac1 = hmac_sha256(key, data);
        let mac2 = hmac_sha256(key, data);
        assert_eq!(mac1, mac2);
    }

    #[test]
    fn test_hmac_different_data_different_mac() {
        let key = b"secret key";
        let mac1 = hmac_sha256(key, b"message1");
        let mac2 = hmac_sha256(key, b"message2");
        assert_ne!(mac1, mac2);
    }

    #[test]
    fn test_hmac_different_key_different_mac() {
        let data = b"message";
        let mac1 = hmac_sha256(b"key1", data);
        let mac2 = hmac_sha256(b"key2", data);
        assert_ne!(mac1, mac2);
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_constant_time_eq_single_byte() {
        assert!(constant_time_eq(&[0x42], &[0x42]));
        assert!(!constant_time_eq(&[0x42], &[0x43]));
    }

    #[test]
    fn test_constant_time_eq_all_differences() {
        // Test that comparison fails regardless of where the difference is
        let a = [1, 2, 3, 4, 5];

        // Difference at start
        let b1 = [0, 2, 3, 4, 5];
        assert!(!constant_time_eq(&a, &b1));

        // Difference in middle
        let b2 = [1, 2, 0, 4, 5];
        assert!(!constant_time_eq(&a, &b2));

        // Difference at end
        let b3 = [1, 2, 3, 4, 0];
        assert!(!constant_time_eq(&a, &b3));
    }

    #[test]
    fn test_hmac_large_data() {
        let key = b"secret key";
        let data = [0xab; 10000]; // 10KB of data
        let mac = hmac_sha256(key, &data);
        assert_eq!(mac.len(), 32);
        assert!(verify_hmac(key, &data, &mac));
    }

    #[test]
    fn test_hmac_rfc4231_test_vector() {
        // RFC 4231 Test Case 1
        let key = [0x0b; 20];
        let data = b"Hi There";
        let mac = hmac_sha256(&key, data);

        // Expected result from RFC 4231
        let expected = [
            0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
            0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
            0x2e, 0x32, 0xcf, 0xf7,
        ];
        assert_eq!(mac, expected);
    }
}
