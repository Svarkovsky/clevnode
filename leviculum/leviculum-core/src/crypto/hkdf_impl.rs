//! HKDF (HMAC-based Key Derivation Function) implementation

use hkdf::Hkdf;
use hmac::Mac;
use sha2::Sha256;

use super::hmac_impl::HmacSha256;

const HASH_LEN: usize = 32;

/// Derive a key using HKDF-SHA256
///
/// Extraction follows RFC 5869. Expansion uses the block counter of
/// `RNS/Cryptography/HKDF.py`, which is taken modulo 256 rather than capped at
/// 255 blocks, so outputs longer than 8160 bytes keep going instead of being
/// rejected. Up to 8160 bytes the two are byte-for-byte identical. Reticulum
/// needs the longer outputs: an IFAC mask spans a whole packet, and interfaces
/// negotiate MTUs far above 8160 bytes.
///
/// # Arguments
/// * `ikm` - Input key material
/// * `salt` - Optional salt (use None for zero-length salt)
/// * `info` - Optional context/application-specific info
/// * `output` - Buffer to write derived key material
pub fn derive_key(ikm: &[u8], salt: Option<&[u8]>, info: Option<&[u8]>, output: &mut [u8]) {
    let (prk, _) = Hkdf::<Sha256>::extract(salt, ikm);
    let info = info.unwrap_or(&[]);

    let keyed = HmacSha256::new_from_slice(&prk).expect("HMAC can take key of any size");
    let mut block = [0u8; HASH_LEN];
    let mut counter: u8 = 1;
    let mut written = 0;

    while written < output.len() {
        let mut mac = keyed.clone();
        if written > 0 {
            mac.update(&block);
        }
        mac.update(info);
        mac.update(&[counter]);
        block = mac.finalize().into_bytes().into();

        let take = HASH_LEN.min(output.len() - written);
        output[written..written + take].copy_from_slice(&block[..take]);
        written += take;
        counter = counter.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hkdf_basic() {
        let ikm = b"input key material";
        let salt = b"salt";
        let info = b"context";
        let mut output = [0u8; 32];

        derive_key(ikm, Some(salt), Some(info), &mut output);
        assert_ne!(output, [0u8; 32]);
    }

    #[test]
    fn test_hkdf_no_salt() {
        let ikm = b"input key material";
        let mut output = [0u8; 32];

        derive_key(ikm, None, None, &mut output);
        assert_ne!(output, [0u8; 32]);
    }

    #[test]
    fn test_hkdf_deterministic() {
        let ikm = b"input key material";
        let salt = b"salt";

        let mut out1 = [0u8; 32];
        let mut out2 = [0u8; 32];
        derive_key(ikm, Some(salt), None, &mut out1);
        derive_key(ikm, Some(salt), None, &mut out2);
        assert_eq!(out1, out2);
    }

    // ==================== EDGE CASE TESTS ====================

    #[test]
    fn test_hkdf_empty_ikm() {
        // Empty IKM is valid
        let ikm: &[u8] = b"";
        let mut output = [0u8; 32];
        derive_key(ikm, None, None, &mut output);
        // Output should be deterministic and non-zero (based on empty input)
        assert_ne!(output, [0u8; 32]);
    }

    #[test]
    fn test_hkdf_empty_salt() {
        // Explicit empty salt vs None should behave similarly
        let ikm = b"input key material";
        let mut out1 = [0u8; 32];
        let mut out2 = [0u8; 32];

        derive_key(ikm, None, None, &mut out1);
        derive_key(ikm, Some(&[]), None, &mut out2);

        // None salt uses a zero-filled salt of hash length,
        // while empty salt is actually empty - these may differ
        // The important thing is both work
        assert_ne!(out1, [0u8; 32]);
        assert_ne!(out2, [0u8; 32]);
    }

    #[test]
    fn test_hkdf_empty_info() {
        let ikm = b"input key material";
        let salt = b"salt";
        let mut out1 = [0u8; 32];
        let mut out2 = [0u8; 32];

        derive_key(ikm, Some(salt), None, &mut out1);
        derive_key(ikm, Some(salt), Some(&[]), &mut out2);

        // None info and empty info should produce the same result
        assert_eq!(out1, out2);
    }

    #[test]
    fn test_hkdf_single_byte_output() {
        let ikm = b"input key material";
        let mut output = [0u8; 1];
        derive_key(ikm, None, None, &mut output);
        // Should succeed (minimum output size)
    }

    #[test]
    fn test_hkdf_zero_byte_output() {
        let ikm = b"input key material";
        let mut output: [u8; 0] = [];
        derive_key(ikm, None, None, &mut output);
        // Should succeed (zero output is valid)
    }

    #[test]
    fn test_hkdf_large_output() {
        // Test larger output (e.g., 256 bytes)
        let ikm = b"input key material";
        let mut output = [0u8; 256];
        derive_key(ikm, None, None, &mut output);
        assert_ne!(output[..32], [0u8; 32]);
        assert_ne!(output[224..], [0u8; 32]);
    }

    #[test]
    fn test_hkdf_max_output() {
        // Maximum HKDF-SHA256 output is 255 * 32 = 8160 bytes
        let ikm = b"input key material";
        let mut output = [0u8; 8160];
        derive_key(ikm, None, None, &mut output);
        // Should not panic
    }

    #[test]
    fn test_hkdf_beyond_rfc_limit_matches_python() {
        // Reference: RNS.Cryptography.HKDF.hkdf(length=8192, derive_from=ikm, salt=salt).
        // 8192 bytes is 256 blocks, one past the 255 RFC 5869 allows, so the
        // counter wraps to 0 for the final block.
        let ikm = b"input key material";
        let salt = b"salt";

        let mut output = [0u8; 8192];
        derive_key(ikm, Some(salt), None, &mut output);

        let expected_tail = [
            0xb3, 0x4c, 0x94, 0xd8, 0xe5, 0xfe, 0xa4, 0x1f, 0x87, 0xe5, 0x27, 0x82, 0x23, 0x01,
            0xf0, 0x46, 0xbc, 0x52, 0xc2, 0xca, 0x4a, 0xd2, 0x74, 0xfb, 0x3c, 0x90, 0x5f, 0x7a,
            0x82, 0x24, 0x4c, 0xcb,
        ];
        assert_eq!(&output[8160..], &expected_tail);

        // Everything up to the RFC limit is unchanged.
        let mut capped = [0u8; 8160];
        derive_key(ikm, Some(salt), None, &mut capped);
        assert_eq!(&output[..8160], &capped[..]);
    }

    #[test]
    fn test_hkdf_long_ikm() {
        // Very long IKM
        let ikm = [0xab; 10000];
        let mut output = [0u8; 32];
        derive_key(&ikm, None, None, &mut output);
        assert_ne!(output, [0u8; 32]);
    }

    #[test]
    fn test_hkdf_long_salt() {
        // Very long salt
        let ikm = b"input key material";
        let salt = [0xcd; 10000];
        let mut output = [0u8; 32];
        derive_key(ikm, Some(&salt), None, &mut output);
        assert_ne!(output, [0u8; 32]);
    }

    #[test]
    fn test_hkdf_long_info() {
        // Very long info
        let ikm = b"input key material";
        let info = [0xef; 10000];
        let mut output = [0u8; 32];
        derive_key(ikm, None, Some(&info), &mut output);
        assert_ne!(output, [0u8; 32]);
    }

    #[test]
    fn test_hkdf_different_salt_different_output() {
        let ikm = b"input key material";
        let mut out1 = [0u8; 32];
        let mut out2 = [0u8; 32];

        derive_key(ikm, Some(b"salt1"), None, &mut out1);
        derive_key(ikm, Some(b"salt2"), None, &mut out2);
        assert_ne!(out1, out2);
    }

    #[test]
    fn test_hkdf_different_info_different_output() {
        let ikm = b"input key material";
        let mut out1 = [0u8; 32];
        let mut out2 = [0u8; 32];

        derive_key(ikm, None, Some(b"info1"), &mut out1);
        derive_key(ikm, None, Some(b"info2"), &mut out2);
        assert_ne!(out1, out2);
    }

    #[test]
    fn test_hkdf_rfc5869_test_vector_1() {
        // RFC 5869 Test Case 1 for HKDF-SHA256
        let ikm = [0x0b; 22];
        let salt = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info = [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];

        let mut okm = [0u8; 42];
        derive_key(&ikm, Some(&salt), Some(&info), &mut okm);

        let expected = [
            0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a, 0x90, 0x43, 0x4f, 0x64, 0xd0, 0x36,
            0x2f, 0x2a, 0x2d, 0x2d, 0x0a, 0x90, 0xcf, 0x1a, 0x5a, 0x4c, 0x5d, 0xb0, 0x2d, 0x56,
            0xec, 0xc4, 0xc5, 0xbf, 0x34, 0x00, 0x72, 0x08, 0xd5, 0xb8, 0x87, 0x18, 0x58, 0x65,
        ];
        assert_eq!(okm, expected);
    }

    #[test]
    fn test_hkdf_prefix_property() {
        // Shorter output should be prefix of longer output
        let ikm = b"input key material";
        let salt = b"salt";

        let mut short = [0u8; 32];
        let mut long = [0u8; 64];

        derive_key(ikm, Some(salt), None, &mut short);
        derive_key(ikm, Some(salt), None, &mut long);

        assert_eq!(&short[..], &long[..32]);
    }
}
