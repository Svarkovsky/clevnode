//! Token encryption (Fernet-like scheme with AES-256-CBC + HMAC-SHA256)
//!
//! Token format:
//! [IV (16 bytes)] [Ciphertext (variable)] [HMAC (32 bytes)]
//!
//! The key is split:
//! - First 32 bytes: HMAC key
//! - Last 32 bytes: AES key

use crate::constants::{AES_BLOCK_SIZE, HMAC_SIZE, TOKEN_HMAC_KEY_SIZE, TOKEN_KEY_SIZE};

use super::aes_cbc::{aes256_cbc_decrypt, aes256_cbc_encrypt, AesError};
use super::hmac_impl::{hmac_sha256, verify_hmac};

/// Token encryption/decryption error
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// Invalid key length (must be 64 bytes: 32 HMAC + 32 AES)
    InvalidKeyLength,
    /// Token too short to contain required fields
    TokenTooShort,
    /// HMAC verification failed
    HmacVerificationFailed,
    /// AES decryption failed
    DecryptionFailed,
    /// Buffer too small
    BufferTooSmall,
}

impl core::fmt::Display for TokenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TokenError::InvalidKeyLength => write!(f, "invalid key length"),
            TokenError::TokenTooShort => write!(f, "token too short"),
            TokenError::HmacVerificationFailed => write!(f, "HMAC verification failed"),
            TokenError::DecryptionFailed => write!(f, "decryption failed"),
            TokenError::BufferTooSmall => write!(f, "buffer too small"),
        }
    }
}

/// Minimum token size: IV (16) + at least one block (16) + HMAC (32) = 64 bytes
pub(crate) const MIN_TOKEN_SIZE: usize = AES_BLOCK_SIZE + AES_BLOCK_SIZE + HMAC_SIZE;

/// Encrypt data to a token
///
/// The key must be 64 bytes (32 HMAC + 32 AES).
/// The IV must be 16 bytes of random data.
///
/// Returns the token length written to output.
pub fn encrypt_token(
    key: &[u8],
    iv: &[u8],
    plaintext: &[u8],
    output: &mut [u8],
) -> Result<usize, TokenError> {
    if key.len() != TOKEN_KEY_SIZE {
        return Err(TokenError::InvalidKeyLength);
    }
    if iv.len() != AES_BLOCK_SIZE {
        // Invalid IV treated as decryption failure to avoid leaking information
        return Err(TokenError::DecryptionFailed);
    }

    let hmac_key = &key[..TOKEN_HMAC_KEY_SIZE];
    let aes_key = &key[TOKEN_HMAC_KEY_SIZE..];

    // Calculate required output size
    let padded_len = ((plaintext.len() / AES_BLOCK_SIZE) + 1) * AES_BLOCK_SIZE;
    let token_len = AES_BLOCK_SIZE + padded_len + HMAC_SIZE;

    if output.len() < token_len {
        return Err(TokenError::BufferTooSmall);
    }

    // Write IV
    output[..AES_BLOCK_SIZE].copy_from_slice(iv);

    // Encrypt plaintext
    let enc_len = aes256_cbc_encrypt(aes_key, iv, plaintext, &mut output[AES_BLOCK_SIZE..])
        .map_err(|e| match e {
            AesError::InvalidKeyLength => TokenError::InvalidKeyLength,
            AesError::BufferTooSmall => TokenError::BufferTooSmall,
            AesError::InvalidIvLength | AesError::DecryptionFailed => TokenError::DecryptionFailed,
        })?;

    // Calculate HMAC over IV + ciphertext
    let hmac_input_len = AES_BLOCK_SIZE + enc_len;
    let hmac = hmac_sha256(hmac_key, &output[..hmac_input_len]);

    // Append HMAC
    output[hmac_input_len..hmac_input_len + HMAC_SIZE].copy_from_slice(&hmac);

    Ok(hmac_input_len + HMAC_SIZE)
}

/// Decrypt a token
///
/// The key must be 64 bytes (32 HMAC + 32 AES).
///
/// Returns the plaintext length written to output.
pub fn decrypt_token(key: &[u8], token: &[u8], output: &mut [u8]) -> Result<usize, TokenError> {
    if key.len() != TOKEN_KEY_SIZE {
        return Err(TokenError::InvalidKeyLength);
    }
    if token.len() < MIN_TOKEN_SIZE {
        return Err(TokenError::TokenTooShort);
    }

    let hmac_key = &key[..TOKEN_HMAC_KEY_SIZE];
    let aes_key = &key[TOKEN_HMAC_KEY_SIZE..];

    // Extract components
    let iv = &token[..AES_BLOCK_SIZE];
    let hmac_start = token.len() - HMAC_SIZE;
    let ciphertext = &token[AES_BLOCK_SIZE..hmac_start];
    let received_hmac = &token[hmac_start..];

    // Verify HMAC first (authenticate-then-decrypt)
    let hmac_input = &token[..hmac_start];
    if !verify_hmac(hmac_key, hmac_input, received_hmac) {
        return Err(TokenError::HmacVerificationFailed);
    }

    // Decrypt
    let plaintext_len = aes256_cbc_decrypt(aes_key, iv, ciphertext, output)
        .map_err(|_| TokenError::DecryptionFailed)?;

    Ok(plaintext_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn make_key() -> [u8; 64] {
        let mut key = [0u8; 64];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = i as u8;
        }
        key
    }

    /// Known-answer test: token layer against a fixed Python-RNS token.
    ///
    /// Pins the full Fernet-style token bytes (IV || AES-256-CBC ciphertext || HMAC)
    /// to output captured from the reference implementation, so a
    /// wire-incompatible-but-self-consistent swap of the AES/HMAC/padding stack is
    /// caught by unit tests independent of the interop harness.
    ///
    /// Source: vendored Python-RNS `RNS.Cryptography.Token` (reference/Reticulum),
    /// mode AES_256_CBC, with os.urandom monkeypatched to emit the fixed IV below so
    /// the encrypt is deterministic. Fixed 64-byte key (bytes 0x00..0x3f), fixed
    /// 16-byte IV (0x00..0x0f), fixed plaintext b"Reticulum KAT vector".
    ///
    /// Asserts both directions against the SAME reference token:
    ///   - our decrypt(reference token) == plaintext, and
    ///   - our encrypt(key, iv, plaintext) == reference token (byte-for-byte).
    #[test]
    fn kat_token_python_rns_vector() {
        // Fixed 64-byte key: first 32 = HMAC key, last 32 = AES key (bytes 0x00..0x3f)
        let key: [u8; 64] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29,
            0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
        ];
        // Fixed IV (bytes 0x00..0x0f)
        let iv: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        // Fixed plaintext: b"Reticulum KAT vector" (20 bytes)
        let plaintext: [u8; 20] = [
            0x52, 0x65, 0x74, 0x69, 0x63, 0x75, 0x6c, 0x75, 0x6d, 0x20, 0x4b, 0x41, 0x54, 0x20,
            0x76, 0x65, 0x63, 0x74, 0x6f, 0x72,
        ];
        // Reference token from Python-RNS: IV(16) || ciphertext(32) || HMAC(32) = 80 bytes
        let reference_token: [u8; 80] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0xc4, 0x82, 0xfe, 0x75, 0xc9, 0xe6, 0xe5, 0x17, 0x27, 0xf3, 0xed, 0x90,
            0xbe, 0xcc, 0xa8, 0x28, 0x2a, 0x48, 0xcf, 0x0a, 0xd1, 0x18, 0xa8, 0x4d, 0xd9, 0xd0,
            0xe9, 0xbe, 0xb3, 0xe3, 0x11, 0xfa, 0x09, 0xca, 0x0e, 0xa1, 0x8e, 0x52, 0x46, 0x1d,
            0xde, 0x6f, 0xee, 0x12, 0xfb, 0x0f, 0x05, 0x58, 0x59, 0xc0, 0xb2, 0xc8, 0x33, 0x87,
            0x36, 0x75, 0x45, 0x24, 0x07, 0x77, 0x0f, 0xb5, 0xd3, 0xa7,
        ];

        // Our decrypt of the fixed Python-RNS token recovers the plaintext.
        let mut dec = [0u8; 64];
        let dec_len = decrypt_token(&key, &reference_token, &mut dec).unwrap();
        assert_eq!(dec_len, plaintext.len());
        assert_eq!(
            &dec[..dec_len],
            &plaintext,
            "our decrypt of Python-RNS token must recover the plaintext"
        );

        // Our encrypt with the same fixed key + IV reproduces the exact token bytes.
        let mut tok = [0u8; 80];
        let tok_len = encrypt_token(&key, &iv, &plaintext, &mut tok).unwrap();
        assert_eq!(tok_len, 80);
        assert_eq!(
            &tok[..tok_len],
            &reference_token,
            "our token bytes must match the Python-RNS reference token"
        );
    }

    #[test]
    fn test_token_roundtrip() {
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = b"Hello, Reticulum token!";

        let mut token = [0u8; 128];
        let token_len = encrypt_token(&key, &iv, plaintext, &mut token).unwrap();

        let mut decrypted = [0u8; 64];
        let dec_len = decrypt_token(&key, &token[..token_len], &mut decrypted).unwrap();

        assert_eq!(dec_len, plaintext.len());
        assert_eq!(&decrypted[..dec_len], plaintext);
    }

    #[test]
    fn test_token_tamper_detection() {
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = b"Secret message";

        let mut token = [0u8; 128];
        let token_len = encrypt_token(&key, &iv, plaintext, &mut token).unwrap();

        // Tamper with ciphertext
        token[20] ^= 0xff;

        let mut decrypted = [0u8; 64];
        let result = decrypt_token(&key, &token[..token_len], &mut decrypted);
        assert_eq!(result, Err(TokenError::HmacVerificationFailed));
    }

    #[test]
    fn test_invalid_key_length() {
        let key = [0u8; 32]; // Wrong size
        let iv = [0x42u8; 16];
        let plaintext = b"test";
        let mut output = [0u8; 128];

        let result = encrypt_token(&key, &iv, plaintext, &mut output);
        assert_eq!(result, Err(TokenError::InvalidKeyLength));
    }

    #[test]
    fn test_token_too_short() {
        let key = make_key();
        let short_token = [0u8; 32]; // Less than MIN_TOKEN_SIZE
        let mut output = [0u8; 64];

        let result = decrypt_token(&key, &short_token, &mut output);
        assert_eq!(result, Err(TokenError::TokenTooShort));
    }

    // ==================== EDGE CASE TESTS ====================

    #[test]
    fn test_token_empty_plaintext() {
        // Empty plaintext should work (produces one padding block)
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext: &[u8] = b"";

        let mut token = [0u8; 64]; // IV (16) + one block (16) + HMAC (32) = 64
        let token_len = encrypt_token(&key, &iv, plaintext, &mut token).unwrap();
        assert_eq!(token_len, 64);

        let mut decrypted = [0u8; 16];
        let dec_len = decrypt_token(&key, &token[..token_len], &mut decrypted).unwrap();
        assert_eq!(dec_len, 0);
    }

    #[test]
    fn test_token_tamper_iv() {
        // Tampering with IV should be detected by HMAC
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = b"Secret message";

        let mut token = [0u8; 128];
        let token_len = encrypt_token(&key, &iv, plaintext, &mut token).unwrap();

        // Tamper with IV (first byte)
        token[0] ^= 0x01;

        let mut decrypted = [0u8; 64];
        let result = decrypt_token(&key, &token[..token_len], &mut decrypted);
        assert_eq!(result, Err(TokenError::HmacVerificationFailed));
    }

    #[test]
    fn test_token_tamper_hmac() {
        // Tampering with HMAC should be detected
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = b"Secret message";

        let mut token = [0u8; 128];
        let token_len = encrypt_token(&key, &iv, plaintext, &mut token).unwrap();

        // Tamper with HMAC (last byte)
        token[token_len - 1] ^= 0x01;

        let mut decrypted = [0u8; 64];
        let result = decrypt_token(&key, &token[..token_len], &mut decrypted);
        assert_eq!(result, Err(TokenError::HmacVerificationFailed));
    }

    #[test]
    fn test_token_wrong_key() {
        let key1 = make_key();
        let mut key2 = make_key();
        key2[0] ^= 0x01; // Different key

        let iv = [0x42u8; 16];
        let plaintext = b"Secret message";

        let mut token = [0u8; 128];
        let token_len = encrypt_token(&key1, &iv, plaintext, &mut token).unwrap();

        // Try to decrypt with different key
        let mut decrypted = [0u8; 64];
        let result = decrypt_token(&key2, &token[..token_len], &mut decrypted);
        assert_eq!(result, Err(TokenError::HmacVerificationFailed));
    }

    #[test]
    fn test_token_buffer_too_small_encrypt() {
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = b"Hello!";

        let mut token = [0u8; 32]; // Too small
        let result = encrypt_token(&key, &iv, plaintext, &mut token);
        assert_eq!(result, Err(TokenError::BufferTooSmall));
    }

    #[test]
    fn test_token_invalid_iv_length() {
        let key = make_key();
        let iv = [0x42u8; 8]; // Wrong IV size
        let plaintext = b"test";
        let mut output = [0u8; 128];

        let result = encrypt_token(&key, &iv, plaintext, &mut output);
        assert_eq!(result, Err(TokenError::DecryptionFailed));
    }

    #[test]
    fn test_token_minimum_size() {
        // Token exactly at minimum size (64 bytes)
        let key = make_key();
        let token = [0u8; 64]; // Exactly MIN_TOKEN_SIZE
        let mut output = [0u8; 64];

        // Should not return TokenTooShort (may fail on HMAC verification)
        let result = decrypt_token(&key, &token, &mut output);
        assert_ne!(result, Err(TokenError::TokenTooShort));
    }

    #[test]
    fn test_token_one_byte_below_minimum() {
        let key = make_key();
        let token = [0u8; 63]; // One byte below minimum
        let mut output = [0u8; 64];

        let result = decrypt_token(&key, &token, &mut output);
        assert_eq!(result, Err(TokenError::TokenTooShort));
    }

    #[test]
    fn test_token_decrypt_invalid_key_length() {
        let key = [0u8; 32]; // Wrong size
        let token = [0u8; 64];
        let mut output = [0u8; 64];

        let result = decrypt_token(&key, &token, &mut output);
        assert_eq!(result, Err(TokenError::InvalidKeyLength));
    }

    #[test]
    fn test_token_large_plaintext() {
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = [0xab; 10000]; // 10KB

        let padded_len = ((plaintext.len() / 16) + 1) * 16;
        let token_len = 16 + padded_len + 32;
        let mut token = vec![0u8; token_len];

        let len = encrypt_token(&key, &iv, &plaintext, &mut token).unwrap();

        let mut decrypted = vec![0u8; plaintext.len() + 16];
        let dec_len = decrypt_token(&key, &token[..len], &mut decrypted).unwrap();

        assert_eq!(dec_len, plaintext.len());
        assert_eq!(&decrypted[..dec_len], &plaintext[..]);
    }

    #[test]
    fn test_token_deterministic() {
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = b"deterministic test";

        let mut token1 = [0u8; 128];
        let mut token2 = [0u8; 128];

        let len1 = encrypt_token(&key, &iv, plaintext, &mut token1).unwrap();
        let len2 = encrypt_token(&key, &iv, plaintext, &mut token2).unwrap();

        assert_eq!(len1, len2);
        assert_eq!(&token1[..len1], &token2[..len2]);
    }

    #[test]
    fn test_token_different_iv_different_output() {
        let key = make_key();
        let iv1 = [0x42u8; 16];
        let iv2 = [0x43u8; 16];
        let plaintext = b"same plaintext";

        let mut token1 = [0u8; 128];
        let mut token2 = [0u8; 128];

        let len1 = encrypt_token(&key, &iv1, plaintext, &mut token1).unwrap();
        let len2 = encrypt_token(&key, &iv2, plaintext, &mut token2).unwrap();

        // Same length but different content
        assert_eq!(len1, len2);
        assert_ne!(&token1[..len1], &token2[..len2]);
    }

    #[test]
    fn test_token_format() {
        // Verify token structure: IV (16) + ciphertext (variable) + HMAC (32)
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = b"Hello!"; // 6 bytes -> 16 bytes ciphertext

        let mut token = [0u8; 128];
        let token_len = encrypt_token(&key, &iv, plaintext, &mut token).unwrap();

        // Token should be IV (16) + ciphertext (16) + HMAC (32) = 64
        assert_eq!(token_len, 64);

        // IV should be at the start
        assert_eq!(&token[..16], &iv);
    }

    #[test]
    fn test_token_ciphertext_corruption_middle() {
        // Corrupt a byte in the middle of ciphertext
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = [0xab; 48]; // 3 blocks of data

        let mut token = [0u8; 128];
        let token_len = encrypt_token(&key, &iv, &plaintext, &mut token).unwrap();

        // Corrupt middle of second block
        token[24] ^= 0xff;

        let mut decrypted = [0u8; 64];
        let result = decrypt_token(&key, &token[..token_len], &mut decrypted);
        assert_eq!(result, Err(TokenError::HmacVerificationFailed));
    }

    #[test]
    fn test_token_single_byte_plaintext() {
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = [0xaa; 1];

        let mut token = [0u8; 64];
        let token_len = encrypt_token(&key, &iv, &plaintext, &mut token).unwrap();

        let mut decrypted = [0u8; 16];
        let dec_len = decrypt_token(&key, &token[..token_len], &mut decrypted).unwrap();

        assert_eq!(dec_len, 1);
        assert_eq!(decrypted[0], 0xaa);
    }

    #[test]
    fn test_token_block_aligned_plaintext() {
        // Exactly 16 bytes -> needs extra padding block
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = [0xab; 16];

        let mut token = [0u8; 80]; // IV (16) + 2 blocks (32) + HMAC (32)
        let token_len = encrypt_token(&key, &iv, &plaintext, &mut token).unwrap();
        assert_eq!(token_len, 80);

        let mut decrypted = [0u8; 32];
        let dec_len = decrypt_token(&key, &token[..token_len], &mut decrypted).unwrap();

        assert_eq!(dec_len, 16);
        assert_eq!(&decrypted[..16], &plaintext);
    }

    #[test]
    fn test_authenticate_then_decrypt() {
        // Verify that HMAC is checked before decryption
        // If we tamper with ciphertext, we should get HMAC error, not decryption error
        let key = make_key();
        let iv = [0x42u8; 16];
        let plaintext = b"test message";

        let mut token = [0u8; 128];
        let token_len = encrypt_token(&key, &iv, plaintext, &mut token).unwrap();

        // Tamper with ciphertext in a way that would cause bad padding
        // (but HMAC check should happen first)
        token[16] = 0xff;
        token[17] = 0xff;

        let mut decrypted = [0u8; 64];
        let result = decrypt_token(&key, &token[..token_len], &mut decrypted);

        // Should be HMAC error, not decryption error
        assert_eq!(result, Err(TokenError::HmacVerificationFailed));
    }
}
