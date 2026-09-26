//! Property-based tests for cryptographic primitives
//!
//! These tests use proptest to verify that cryptographic operations
//! behave correctly for arbitrary inputs.

use leviculum_core::crypto::{
    aes256_cbc_decrypt, aes256_cbc_encrypt, decrypt_token, derive_key, encrypt_token, hmac_sha256,
    verify_hmac,
};
use leviculum_core::identity::Identity;
use proptest::prelude::*;
use rand_core::OsRng;

// Helper to create identity in tests
fn new_identity() -> Identity {
    Identity::generate(&mut OsRng)
}

// ==================== AES-256-CBC PROPERTY TESTS ====================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn test_aes_roundtrip(plaintext in prop::collection::vec(any::<u8>(), 0..1000)) {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];

        let padded_len = ((plaintext.len() / 16) + 1) * 16;
        let mut encrypted = vec![0u8; padded_len];
        let enc_len = aes256_cbc_encrypt(&key, &iv, &plaintext, &mut encrypted).unwrap();

        let mut decrypted = vec![0u8; enc_len];
        let dec_len = aes256_cbc_decrypt(&key, &iv, &encrypted[..enc_len], &mut decrypted).unwrap();

        prop_assert_eq!(dec_len, plaintext.len());
        prop_assert_eq!(&decrypted[..dec_len], &plaintext[..]);
    }

    #[test]
    fn test_aes_ciphertext_length(plaintext_len in 0usize..1000) {
        let key = [0x42u8; 32];
        let iv = [0x13u8; 16];
        let plaintext = vec![0xab; plaintext_len];

        let padded_len = ((plaintext_len / 16) + 1) * 16;
        let mut encrypted = vec![0u8; padded_len];
        let enc_len = aes256_cbc_encrypt(&key, &iv, &plaintext, &mut encrypted).unwrap();

        // Ciphertext should be exactly the padded length
        prop_assert_eq!(enc_len, padded_len);
    }
}

// ==================== HMAC PROPERTY TESTS ====================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn test_hmac_deterministic(
        key in prop::collection::vec(any::<u8>(), 0..100),
        data in prop::collection::vec(any::<u8>(), 0..1000)
    ) {
        let mac1 = hmac_sha256(&key, &data);
        let mac2 = hmac_sha256(&key, &data);
        prop_assert_eq!(mac1, mac2);
    }

    #[test]
    fn test_hmac_verify_correct(
        key in prop::collection::vec(any::<u8>(), 1..100),
        data in prop::collection::vec(any::<u8>(), 0..1000)
    ) {
        let mac = hmac_sha256(&key, &data);
        prop_assert!(verify_hmac(&key, &data, &mac));
    }

    #[test]
    fn test_hmac_different_data_different_mac(
        key in prop::collection::vec(any::<u8>(), 1..100),
        data1 in prop::collection::vec(any::<u8>(), 1..100),
        data2 in prop::collection::vec(any::<u8>(), 1..100)
    ) {
        prop_assume!(data1 != data2);
        let mac1 = hmac_sha256(&key, &data1);
        let mac2 = hmac_sha256(&key, &data2);
        prop_assert_ne!(mac1, mac2);
    }
}

// ==================== HKDF PROPERTY TESTS ====================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn test_hkdf_deterministic(
        ikm in prop::collection::vec(any::<u8>(), 1..100),
        salt in prop::collection::vec(any::<u8>(), 0..100),
        info in prop::collection::vec(any::<u8>(), 0..100),
        output_len in 1usize..256
    ) {
        let mut out1 = vec![0u8; output_len];
        let mut out2 = vec![0u8; output_len];

        derive_key(&ikm, Some(&salt), Some(&info), &mut out1);
        derive_key(&ikm, Some(&salt), Some(&info), &mut out2);

        prop_assert_eq!(out1, out2);
    }

    #[test]
    fn test_hkdf_prefix_property(
        ikm in prop::collection::vec(any::<u8>(), 1..100),
        short_len in 1usize..64,
        long_len in 64usize..128
    ) {
        prop_assume!(short_len < long_len);

        let mut short = vec![0u8; short_len];
        let mut long = vec![0u8; long_len];

        derive_key(&ikm, None, None, &mut short);
        derive_key(&ikm, None, None, &mut long);

        prop_assert_eq!(&short[..], &long[..short_len]);
    }
}

// ==================== TOKEN PROPERTY TESTS ====================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn test_token_roundtrip(plaintext in prop::collection::vec(any::<u8>(), 0..500)) {
        let mut key = [0u8; 64];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = i as u8;
        }
        let iv = [0x42u8; 16];

        let padded_len = ((plaintext.len() / 16) + 1) * 16;
        let token_len = 16 + padded_len + 32;
        let mut token = vec![0u8; token_len];

        let enc_len = encrypt_token(&key, &iv, &plaintext, &mut token).unwrap();

        let mut decrypted = vec![0u8; plaintext.len() + 16];
        let dec_len = decrypt_token(&key, &token[..enc_len], &mut decrypted).unwrap();

        prop_assert_eq!(dec_len, plaintext.len());
        prop_assert_eq!(&decrypted[..dec_len], &plaintext[..]);
    }

    #[test]
    fn test_token_tamper_detection(
        plaintext in prop::collection::vec(any::<u8>(), 1..100),
        tamper_pos in 0usize..100
    ) {
        let mut key = [0u8; 64];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = i as u8;
        }
        let iv = [0x42u8; 16];

        let padded_len = ((plaintext.len() / 16) + 1) * 16;
        let token_len = 16 + padded_len + 32;
        let mut token = vec![0u8; token_len];

        let enc_len = encrypt_token(&key, &iv, &plaintext, &mut token).unwrap();

        // Tamper at a valid position
        let actual_pos = tamper_pos % enc_len;
        token[actual_pos] ^= 0x01;

        let mut decrypted = vec![0u8; plaintext.len() + 16];
        let result = decrypt_token(&key, &token[..enc_len], &mut decrypted);

        // Should fail (either HMAC or decryption error)
        prop_assert!(result.is_err());
    }
}

// ==================== IDENTITY PROPERTY TESTS ====================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn test_identity_encrypt_decrypt_roundtrip(
        plaintext in prop::collection::vec(any::<u8>(), 0..500)
    ) {
        let identity = new_identity();

        let ciphertext = identity.encrypt(&plaintext, &mut OsRng).unwrap();
        let decrypted = identity.decrypt(&ciphertext).unwrap();

        prop_assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_identity_sign_verify_roundtrip(
        message in prop::collection::vec(any::<u8>(), 0..1000)
    ) {
        let identity = new_identity();

        let signature = identity.sign(&message).unwrap();
        prop_assert!(identity.verify(&message, &signature).unwrap());
    }

    #[test]
    fn test_identity_different_identities_cannot_decrypt(
        plaintext in prop::collection::vec(any::<u8>(), 1..100)
    ) {
        let alice = new_identity();
        let bob = new_identity();

        let ciphertext = alice.encrypt(&plaintext, &mut OsRng).unwrap();
        let result = bob.decrypt(&ciphertext);

        prop_assert!(result.is_err());
    }

    #[test]
    fn test_identity_different_identities_cannot_verify(
        message in prop::collection::vec(any::<u8>(), 1..100)
    ) {
        let alice = new_identity();
        let bob = new_identity();

        let signature = alice.sign(&message).unwrap();

        // Bob should not be able to verify Alice's signature
        prop_assert!(!bob.verify(&message, &signature).unwrap());
    }

    #[test]
    fn test_identity_encryption_non_deterministic(
        plaintext in prop::collection::vec(any::<u8>(), 1..100)
    ) {
        let identity = new_identity();

        let ct1 = identity.encrypt(&plaintext, &mut OsRng).unwrap();
        let ct2 = identity.encrypt(&plaintext, &mut OsRng).unwrap();

        // Ciphertexts should be different (different ephemeral keys)
        prop_assert!(ct1 != ct2);

        // But both should decrypt to the same plaintext
        prop_assert_eq!(identity.decrypt(&ct1).unwrap(), plaintext.clone());
        prop_assert_eq!(identity.decrypt(&ct2).unwrap(), plaintext);
    }

    #[test]
    fn test_identity_signature_deterministic(
        message in prop::collection::vec(any::<u8>(), 0..100)
    ) {
        let identity = new_identity();

        let sig1 = identity.sign(&message).unwrap();
        let sig2 = identity.sign(&message).unwrap();

        prop_assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_identity_wrong_message_fails_verify(
        message1 in prop::collection::vec(any::<u8>(), 1..100),
        message2 in prop::collection::vec(any::<u8>(), 1..100)
    ) {
        prop_assume!(message1 != message2);

        let identity = new_identity();
        let signature = identity.sign(&message1).unwrap();

        prop_assert!(!identity.verify(&message2, &signature).unwrap());
    }
}

// ==================== KEY SERIALIZATION PROPERTY TESTS ====================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn test_identity_public_key_roundtrip(_seed in any::<u64>()) {
        let identity = new_identity();
        let pub_bytes = identity.public_key_bytes();

        let restored = Identity::from_public_key_bytes(&pub_bytes).unwrap();

        prop_assert_eq!(identity.hash(), restored.hash());
        prop_assert!(!restored.has_private_keys());
    }

    #[test]
    fn test_identity_private_key_roundtrip(_seed in any::<u64>()) {
        let identity = new_identity();
        let prv_bytes = identity.private_key_bytes().unwrap();

        let restored = Identity::from_private_key_bytes(&prv_bytes).unwrap();

        prop_assert_eq!(identity.hash(), restored.hash());
        prop_assert!(restored.has_private_keys());

        // Should be able to sign with restored identity
        let message = b"test message";
        let sig = restored.sign(message).unwrap();
        prop_assert!(identity.verify(message, &sig).unwrap());
    }
}
