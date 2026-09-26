//! AES-256-CBC encryption/decryption with PKCS7 padding

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use cbc::{Decryptor, Encryptor};

type Aes256CbcEnc = Encryptor<aes::Aes256>;
type Aes256CbcDec = Decryptor<aes::Aes256>;

use crate::constants::{AES256_KEY_SIZE, AES_BLOCK_SIZE};

/// AES-256-CBC encryption error
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AesError {
    /// Invalid key length (must be 32 bytes)
    InvalidKeyLength,
    /// Invalid IV length (must be 16 bytes)
    InvalidIvLength,
    /// Buffer too small for encrypted output
    BufferTooSmall,
    /// Decryption failed (invalid padding or ciphertext)
    DecryptionFailed,
}

impl core::fmt::Display for AesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AesError::InvalidKeyLength => write!(f, "invalid key length"),
            AesError::InvalidIvLength => write!(f, "invalid IV length"),
            AesError::BufferTooSmall => write!(f, "buffer too small"),
            AesError::DecryptionFailed => write!(f, "decryption failed"),
        }
    }
}

/// Apply PKCS7 padding to a buffer
fn pkcs7_pad(data: &[u8], output: &mut [u8]) -> usize {
    let padding_len = AES_BLOCK_SIZE - (data.len() % AES_BLOCK_SIZE);
    let total_len = data.len() + padding_len;
    output[..data.len()].copy_from_slice(data);
    output[data.len()..total_len].fill(padding_len as u8);
    total_len
}

/// Remove Reticulum's PKCS7-style padding and return the valid length.
///
/// Python Reticulum validates only the final padding-length byte before
/// truncating the authenticated plaintext. Mirror that behaviour here. This
/// also accepts packets produced by microReticulum versions which zero-fill
/// the preceding padding bytes instead of repeating the padding length.
fn pkcs7_unpad(data: &[u8]) -> Result<usize, AesError> {
    if data.is_empty() {
        return Err(AesError::DecryptionFailed);
    }
    let padding_len = data[data.len() - 1] as usize;
    if padding_len > AES_BLOCK_SIZE || padding_len > data.len() {
        return Err(AesError::DecryptionFailed);
    }
    Ok(data.len() - padding_len)
}

/// Encrypt data using AES-256-CBC with PKCS7 padding
///
/// Returns ciphertext length written to the output buffer.
/// The output buffer must be large enough to hold plaintext + padding (up to 16 extra bytes).
pub fn aes256_cbc_encrypt(
    key: &[u8],
    iv: &[u8],
    plaintext: &[u8],
    output: &mut [u8],
) -> Result<usize, AesError> {
    if key.len() != AES256_KEY_SIZE {
        return Err(AesError::InvalidKeyLength);
    }
    if iv.len() != AES_BLOCK_SIZE {
        return Err(AesError::InvalidIvLength);
    }

    // Calculate padded length
    let padded_len = ((plaintext.len() / AES_BLOCK_SIZE) + 1) * AES_BLOCK_SIZE;
    if output.len() < padded_len {
        return Err(AesError::BufferTooSmall);
    }

    // Apply PKCS7 padding
    let total_len = pkcs7_pad(plaintext, output);

    // Create cipher and encrypt in place
    let mut cipher =
        Aes256CbcEnc::new_from_slices(key, iv).expect("key/iv lengths validated above");

    // Encrypt block by block
    for chunk in output[..total_len].chunks_exact_mut(AES_BLOCK_SIZE) {
        cipher.encrypt_block_mut(chunk.into());
    }

    Ok(total_len)
}

/// Decrypt data using AES-256-CBC with PKCS7 padding
///
/// Returns plaintext length written to the output buffer.
pub fn aes256_cbc_decrypt(
    key: &[u8],
    iv: &[u8],
    ciphertext: &[u8],
    output: &mut [u8],
) -> Result<usize, AesError> {
    if key.len() != AES256_KEY_SIZE {
        return Err(AesError::InvalidKeyLength);
    }
    if iv.len() != AES_BLOCK_SIZE {
        return Err(AesError::InvalidIvLength);
    }
    if !ciphertext.len().is_multiple_of(AES_BLOCK_SIZE) || ciphertext.is_empty() {
        return Err(AesError::DecryptionFailed);
    }
    if output.len() < ciphertext.len() {
        return Err(AesError::BufferTooSmall);
    }

    // Copy ciphertext to output buffer
    output[..ciphertext.len()].copy_from_slice(ciphertext);

    // Create cipher and decrypt in place
    let mut cipher =
        Aes256CbcDec::new_from_slices(key, iv).expect("key/iv lengths validated above");

    // Decrypt block by block
    for chunk in output[..ciphertext.len()].chunks_exact_mut(AES_BLOCK_SIZE) {
        cipher.decrypt_block_mut(chunk.into());
    }

    // Remove PKCS7 padding
    pkcs7_unpad(&output[..ciphertext.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-answer test: AES-256-CBC against NIST SP 800-38A.
    ///
    /// Pins the AES-256-CBC core to fixed authoritative ciphertext bytes so a
    /// wire-incompatible cipher swap is caught by unit tests alone.
    ///
    /// Source: NIST SP 800-38A, Appendix F.2.5 (CBC-AES256.Encrypt) and F.2.6
    /// (CBC-AES256.Decrypt). Verified independently with the Python `cryptography`
    /// library. The NIST vector is four block-aligned plaintext blocks with NO
    /// padding; our API always applies PKCS7, so our encrypt output is the four NIST
    /// ciphertext blocks followed by one extra 0x10-padding block. We therefore assert
    /// the first 64 bytes equal the NIST ciphertext exactly, and that decrypting the
    /// full padded output recovers the NIST plaintext.
    #[test]
    fn kat_aes256_cbc_nist_sp800_38a() {
        // NIST SP 800-38A F.2.5 key (32 bytes)
        let key: [u8; 32] = [
            0x60, 0x3d, 0xeb, 0x10, 0x15, 0xca, 0x71, 0xbe, 0x2b, 0x73, 0xae, 0xf0, 0x85, 0x7d,
            0x77, 0x81, 0x1f, 0x35, 0x2c, 0x07, 0x3b, 0x61, 0x08, 0xd7, 0x2d, 0x98, 0x10, 0xa3,
            0x09, 0x14, 0xdf, 0xf4,
        ];
        // NIST SP 800-38A F.2.5 IV (16 bytes)
        let iv: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        // NIST SP 800-38A F.2.5 plaintext (4 blocks, 64 bytes)
        let plaintext: [u8; 64] = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a, 0xae, 0x2d, 0x8a, 0x57, 0x1e, 0x03, 0xac, 0x9c, 0x9e, 0xb7, 0x6f, 0xac,
            0x45, 0xaf, 0x8e, 0x51, 0x30, 0xc8, 0x1c, 0x46, 0xa3, 0x5c, 0xe4, 0x11, 0xe5, 0xfb,
            0xc1, 0x19, 0x1a, 0x0a, 0x52, 0xef, 0xf6, 0x9f, 0x24, 0x45, 0xdf, 0x4f, 0x9b, 0x17,
            0xad, 0x2b, 0x41, 0x7b, 0xe6, 0x6c, 0x37, 0x10,
        ];
        // NIST SP 800-38A F.2.5 ciphertext (4 blocks, 64 bytes)
        let expected_ct: [u8; 64] = [
            0xf5, 0x8c, 0x4c, 0x04, 0xd6, 0xe5, 0xf1, 0xba, 0x77, 0x9e, 0xab, 0xfb, 0x5f, 0x7b,
            0xfb, 0xd6, 0x9c, 0xfc, 0x4e, 0x96, 0x7e, 0xdb, 0x80, 0x8d, 0x67, 0x9f, 0x77, 0x7b,
            0xc6, 0x70, 0x2c, 0x7d, 0x39, 0xf2, 0x33, 0x69, 0xa9, 0xd9, 0xba, 0xcf, 0xa5, 0x30,
            0xe2, 0x63, 0x04, 0x23, 0x14, 0x61, 0xb2, 0xeb, 0x05, 0xe2, 0xc3, 0x9b, 0xe9, 0xfc,
            0xda, 0x6c, 0x19, 0x07, 0x8c, 0x6a, 0x9d, 0x1b,
        ];

        // Encrypt: 64 data bytes -> 64 ciphertext bytes + 16 PKCS7 padding block = 80.
        let mut out = [0u8; 80];
        let n = aes256_cbc_encrypt(&key, &iv, &plaintext, &mut out).unwrap();
        assert_eq!(n, 80);
        assert_eq!(
            &out[..64],
            &expected_ct,
            "AES-256-CBC ciphertext must match NIST SP 800-38A F.2.5"
        );

        // Decrypt the full padded output -> recovers the NIST plaintext.
        let mut dec = [0u8; 80];
        let m = aes256_cbc_decrypt(&key, &iv, &out[..n], &mut dec).unwrap();
        assert_eq!(m, 64);
        assert_eq!(
            &dec[..m],
            &plaintext,
            "AES-256-CBC decrypt must recover NIST SP 800-38A plaintext"
        );
    }

    #[test]
    fn test_pkcs7_padding() {
        let data = b"hello";
        let mut output = [0u8; 16];
        let len = pkcs7_pad(data, &mut output);
        assert_eq!(len, 16);
        // "hello" is 5 bytes, padding should be 11 bytes of 0x0b
        assert_eq!(&output[5..16], &[11u8; 11]);
    }

    #[test]
    fn test_pkcs7_unpad() {
        let mut data = [0u8; 16];
        data[..5].copy_from_slice(b"hello");
        data[5..16].fill(11);
        let len = pkcs7_unpad(&data).unwrap();
        assert_eq!(len, 5);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = b"Hello, Reticulum!";

        let mut encrypted = [0u8; 32]; // 17 bytes + padding = 32 bytes
        let enc_len = aes256_cbc_encrypt(&key, &iv, plaintext, &mut encrypted).unwrap();
        assert_eq!(enc_len, 32);

        let mut decrypted = [0u8; 32];
        let dec_len = aes256_cbc_decrypt(&key, &iv, &encrypted[..enc_len], &mut decrypted).unwrap();
        assert_eq!(dec_len, plaintext.len());
        assert_eq!(&decrypted[..dec_len], plaintext);
    }

    #[test]
    fn test_encrypt_block_aligned() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = [0xab; 16]; // Exactly one block

        let mut encrypted = [0u8; 32]; // Will need extra block for padding
        let enc_len = aes256_cbc_encrypt(&key, &iv, &plaintext, &mut encrypted).unwrap();
        assert_eq!(enc_len, 32); // 16 bytes data + 16 bytes padding block
    }

    #[test]
    fn test_invalid_key_length() {
        let key = [0x42u8; 16]; // Wrong size
        let iv = [0x13u8; 16];
        let plaintext = b"test";
        let mut output = [0u8; 32];

        let result = aes256_cbc_encrypt(&key, &iv, plaintext, &mut output);
        assert_eq!(result, Err(AesError::InvalidKeyLength));
    }

    #[test]
    fn test_invalid_iv_length() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 8]; // Wrong size
        let plaintext = b"test";
        let mut output = [0u8; 32];

        let result = aes256_cbc_encrypt(&key, &iv, plaintext, &mut output);
        assert_eq!(result, Err(AesError::InvalidIvLength));
    }

    // ==================== EDGE CASE TESTS ====================

    #[test]
    fn test_empty_plaintext() {
        // Empty plaintext should produce exactly one block (all padding)
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = b"";

        let mut encrypted = [0u8; 16];
        let enc_len = aes256_cbc_encrypt(&key, &iv, plaintext, &mut encrypted).unwrap();
        assert_eq!(enc_len, 16); // One full block of padding (16 bytes of 0x10)

        let mut decrypted = [0u8; 16];
        let dec_len = aes256_cbc_decrypt(&key, &iv, &encrypted[..enc_len], &mut decrypted).unwrap();
        assert_eq!(dec_len, 0); // Empty plaintext
    }

    #[test]
    fn test_buffer_too_small_encrypt() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = b"Hello, World!"; // 13 bytes -> needs 16 bytes output

        let mut output = [0u8; 8]; // Too small
        let result = aes256_cbc_encrypt(&key, &iv, plaintext, &mut output);
        assert_eq!(result, Err(AesError::BufferTooSmall));
    }

    #[test]
    fn test_buffer_too_small_decrypt() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = b"Hello, World!";

        let mut encrypted = [0u8; 16];
        let enc_len = aes256_cbc_encrypt(&key, &iv, plaintext, &mut encrypted).unwrap();

        let mut output = [0u8; 8]; // Too small
        let result = aes256_cbc_decrypt(&key, &iv, &encrypted[..enc_len], &mut output);
        assert_eq!(result, Err(AesError::BufferTooSmall));
    }

    #[test]
    fn test_invalid_ciphertext_length() {
        // Ciphertext must be multiple of block size
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let bad_ciphertext = [0u8; 17]; // Not a multiple of 16

        let mut output = [0u8; 32];
        let result = aes256_cbc_decrypt(&key, &iv, &bad_ciphertext, &mut output);
        assert_eq!(result, Err(AesError::DecryptionFailed));
    }

    #[test]
    fn test_empty_ciphertext() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let empty_ciphertext: [u8; 0] = [];

        let mut output = [0u8; 16];
        let result = aes256_cbc_decrypt(&key, &iv, &empty_ciphertext, &mut output);
        assert_eq!(result, Err(AesError::DecryptionFailed));
    }

    #[test]
    fn test_zero_padding_length_matches_python_reticulum() {
        let mut bad_block = [0u8; 16];
        bad_block[15] = 0;

        assert_eq!(pkcs7_unpad(&bad_block), Ok(16));
    }

    #[test]
    fn test_invalid_padding_too_large() {
        // Padding byte larger than block size is invalid
        let mut bad_block = [0u8; 16];
        bad_block[15] = 17; // Invalid: larger than block size

        let result = pkcs7_unpad(&bad_block);
        assert_eq!(result, Err(AesError::DecryptionFailed));
    }

    #[test]
    fn test_inconsistent_padding_matches_python_reticulum() {
        let mut bad_block = [0u8; 16];
        bad_block[15] = 4;
        bad_block[14] = 4;
        bad_block[13] = 4;
        bad_block[12] = 3;

        assert_eq!(pkcs7_unpad(&bad_block), Ok(12));
    }

    #[test]
    fn test_decrypts_microreticulum_zero_filled_padding() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = [0xabu8; 135];
        let mut padded = [0u8; 144];
        padded[..plaintext.len()].copy_from_slice(&plaintext);
        // microReticulum pads with zero bytes and stores the padding length
        // only in the last byte. For 135 bytes the padding length is 9.
        padded[143] = 9;

        let mut cipher =
            Aes256CbcEnc::new_from_slices(&key, &iv).expect("fixed key and IV are valid");
        for chunk in padded.chunks_exact_mut(AES_BLOCK_SIZE) {
            cipher.encrypt_block_mut(chunk.into());
        }

        let mut decrypted = [0u8; 144];
        let length = aes256_cbc_decrypt(&key, &iv, &padded, &mut decrypted).unwrap();
        assert_eq!(length, plaintext.len());
        assert_eq!(&decrypted[..length], &plaintext);
    }

    #[test]
    fn test_wrong_key_decrypt() {
        let key1 = [0x42u8; 32];
        let key2 = [0x43u8; 32]; // Different key
        let iv = [0x13u8; 16];
        let plaintext = b"Secret message";

        let mut encrypted = [0u8; 16];
        let enc_len = aes256_cbc_encrypt(&key1, &iv, plaintext, &mut encrypted).unwrap();

        // Decrypting with wrong key should fail (bad padding after decryption)
        let mut decrypted = [0u8; 16];
        let result = aes256_cbc_decrypt(&key2, &iv, &encrypted[..enc_len], &mut decrypted);
        // This will likely produce invalid padding
        assert!(
            result.is_err() || {
                // Or if it happens to produce valid-looking padding, the data will be garbage
                let len = result.unwrap();
                &decrypted[..len] != plaintext
            }
        );
    }

    #[test]
    fn test_wrong_iv_decrypt() {
        let key = [0x42u8; 32];
        let iv1 = [0x13u8; 16];
        let iv2 = [0x14u8; 16]; // Different IV
        let plaintext = b"Secret message";

        let mut encrypted = [0u8; 16];
        let enc_len = aes256_cbc_encrypt(&key, &iv1, plaintext, &mut encrypted).unwrap();

        // Decrypting with wrong IV - first block will be corrupted
        let mut decrypted = [0u8; 16];
        let result = aes256_cbc_decrypt(&key, &iv2, &encrypted[..enc_len], &mut decrypted);

        // With wrong IV, first block is XORed with wrong value
        // The padding in last block might still be valid, but data is corrupted
        if let Ok(len) = result {
            assert_ne!(&decrypted[..len], plaintext);
        }
        // Or it might fail due to corrupted padding
    }

    #[test]
    fn test_multi_block_roundtrip() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        // 100 bytes = 6 full blocks + 4 bytes -> 7 blocks total with padding
        let plaintext = [0xab; 100];

        let mut encrypted = [0u8; 112]; // 7 * 16 = 112
        let enc_len = aes256_cbc_encrypt(&key, &iv, &plaintext, &mut encrypted).unwrap();
        assert_eq!(enc_len, 112);

        let mut decrypted = [0u8; 112];
        let dec_len = aes256_cbc_decrypt(&key, &iv, &encrypted[..enc_len], &mut decrypted).unwrap();
        assert_eq!(dec_len, 100);
        assert_eq!(&decrypted[..dec_len], &plaintext);
    }

    #[test]
    fn test_single_byte_plaintext() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = [0xaa; 1];

        let mut encrypted = [0u8; 16];
        let enc_len = aes256_cbc_encrypt(&key, &iv, &plaintext, &mut encrypted).unwrap();
        assert_eq!(enc_len, 16);

        let mut decrypted = [0u8; 16];
        let dec_len = aes256_cbc_decrypt(&key, &iv, &encrypted[..enc_len], &mut decrypted).unwrap();
        assert_eq!(dec_len, 1);
        assert_eq!(decrypted[0], 0xaa);
    }

    #[test]
    fn test_deterministic_encryption() {
        // Same key + IV + plaintext should always produce same ciphertext
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = b"deterministic test";

        let mut enc1 = [0u8; 32];
        let mut enc2 = [0u8; 32];

        let len1 = aes256_cbc_encrypt(&key, &iv, plaintext, &mut enc1).unwrap();
        let len2 = aes256_cbc_encrypt(&key, &iv, plaintext, &mut enc2).unwrap();

        assert_eq!(len1, len2);
        assert_eq!(&enc1[..len1], &enc2[..len2]);
    }

    #[test]
    fn test_decrypt_invalid_key_length() {
        let key = [0x42u8; 16]; // Wrong size
        let iv = [0x13u8; 16];
        let ciphertext = [0u8; 16];
        let mut output = [0u8; 16];

        let result = aes256_cbc_decrypt(&key, &iv, &ciphertext, &mut output);
        assert_eq!(result, Err(AesError::InvalidKeyLength));
    }

    #[test]
    fn test_decrypt_invalid_iv_length() {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 8]; // Wrong size
        let ciphertext = [0u8; 16];
        let mut output = [0u8; 16];

        let result = aes256_cbc_decrypt(&key, &iv, &ciphertext, &mut output);
        assert_eq!(result, Err(AesError::InvalidIvLength));
    }

    #[test]
    fn test_full_padding_block() {
        // When plaintext is exactly n*16 bytes, we need an extra full padding block
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = [0xab; 32]; // Exactly 2 blocks

        let mut encrypted = [0u8; 48]; // Need 3 blocks for output
        let enc_len = aes256_cbc_encrypt(&key, &iv, &plaintext, &mut encrypted).unwrap();
        assert_eq!(enc_len, 48); // 32 bytes data + 16 bytes padding block

        let mut decrypted = [0u8; 48];
        let dec_len = aes256_cbc_decrypt(&key, &iv, &encrypted[..enc_len], &mut decrypted).unwrap();
        assert_eq!(dec_len, 32);
        assert_eq!(&decrypted[..dec_len], &plaintext);
    }
}
