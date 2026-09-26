//! Cryptographic identity management
//!
//! An Identity represents a cryptographic key pair used for:
//! - Encryption (X25519 ECDH)
//! - Digital signatures (Ed25519)
//!
//! The identity hash is derived from the public keys and serves as a unique
//! identifier in the network.

use crate::constants::{
    AES_BLOCK_SIZE, ED25519_KEY_SIZE, ED25519_SIGNATURE_SIZE, IDENTITY_HASHBYTES,
    IDENTITY_KEY_SIZE, X25519_KEY_SIZE,
};
use crate::crypto::{decrypt_token, derive_key, encrypt_token, truncated_hash};

use alloc::vec::Vec;

/// Error types for identity operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    /// Invalid key length
    InvalidKeyLength,
    /// Invalid signature
    InvalidSignature,
    /// Decryption failed
    DecryptionFailed,
    /// No private key available (public-only identity)
    NoPrivateKey,
    /// Encryption failed (internal token encryption error)
    EncryptionFailed,
}

impl core::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IdentityError::InvalidKeyLength => write!(f, "Invalid key length"),
            IdentityError::InvalidSignature => write!(f, "Invalid signature"),
            IdentityError::DecryptionFailed => write!(f, "Decryption failed"),
            IdentityError::NoPrivateKey => write!(f, "No private key available"),
            IdentityError::EncryptionFailed => write!(f, "Encryption failed"),
        }
    }
}

/// A cryptographic identity with X25519 and Ed25519 key pairs
#[derive(Clone)]
pub struct Identity {
    /// X25519 private key for ECDH (None for public-only identities)
    x25519_private: Option<x25519_dalek::StaticSecret>,
    /// X25519 public key
    x25519_public: x25519_dalek::PublicKey,
    /// Ed25519 signing key (None for public-only identities)
    ed25519_signing: Option<ed25519_dalek::SigningKey>,
    /// Ed25519 verifying key
    ed25519_verifying: ed25519_dalek::VerifyingKey,
    /// Cached identity hash
    hash: [u8; IDENTITY_HASHBYTES],
}

impl Identity {
    /// Create a new identity with random keys
    ///
    /// # Example
    /// ```
    /// use leviculum_core::identity::Identity;
    /// use rand_core::OsRng;
    ///
    /// let identity = Identity::generate(&mut OsRng);
    /// assert_eq!(identity.hash().len(), 16);
    /// ```
    pub fn generate<R: rand_core::CryptoRngCore>(rng: &mut R) -> Self {
        let x25519_private = x25519_dalek::StaticSecret::random_from_rng(&mut *rng);
        let x25519_public = x25519_dalek::PublicKey::from(&x25519_private);

        // Generate Ed25519 key from random bytes
        let mut ed25519_seed = [0u8; 32];
        rng.fill_bytes(&mut ed25519_seed);
        let ed25519_signing = ed25519_dalek::SigningKey::from_bytes(&ed25519_seed);
        let ed25519_verifying = ed25519_signing.verifying_key();

        let hash = Self::compute_hash(&x25519_public, &ed25519_verifying);

        Self {
            x25519_private: Some(x25519_private),
            x25519_public,
            ed25519_signing: Some(ed25519_signing),
            ed25519_verifying,
            hash,
        }
    }

    /// Create a public-only identity from public keys
    pub fn from_public_keys(
        x25519_pub: &[u8; X25519_KEY_SIZE],
        ed25519_pub: &[u8; ED25519_KEY_SIZE],
    ) -> Result<Self, IdentityError> {
        let x25519_public = x25519_dalek::PublicKey::from(*x25519_pub);
        let ed25519_verifying = ed25519_dalek::VerifyingKey::from_bytes(ed25519_pub)
            .map_err(|_| IdentityError::InvalidKeyLength)?;

        let hash = Self::compute_hash(&x25519_public, &ed25519_verifying);

        Ok(Self {
            x25519_private: None,
            x25519_public,
            ed25519_signing: None,
            ed25519_verifying,
            hash,
        })
    }

    /// Create an identity from combined public key bytes (64 bytes)
    pub fn from_public_key_bytes(bytes: &[u8]) -> Result<Self, IdentityError> {
        if bytes.len() != IDENTITY_KEY_SIZE {
            return Err(IdentityError::InvalidKeyLength);
        }

        let mut x25519_pub = [0u8; X25519_KEY_SIZE];
        let mut ed25519_pub = [0u8; ED25519_KEY_SIZE];
        x25519_pub.copy_from_slice(&bytes[..X25519_KEY_SIZE]);
        ed25519_pub.copy_from_slice(&bytes[X25519_KEY_SIZE..]);

        Self::from_public_keys(&x25519_pub, &ed25519_pub)
    }

    /// Load identity from private key bytes (64 bytes: 32 X25519 + 32 Ed25519)
    pub fn from_private_key_bytes(bytes: &[u8]) -> Result<Self, IdentityError> {
        if bytes.len() != IDENTITY_KEY_SIZE {
            return Err(IdentityError::InvalidKeyLength);
        }

        let mut x25519_prv = [0u8; X25519_KEY_SIZE];
        let mut ed25519_prv = [0u8; ED25519_KEY_SIZE];
        x25519_prv.copy_from_slice(&bytes[..X25519_KEY_SIZE]);
        ed25519_prv.copy_from_slice(&bytes[X25519_KEY_SIZE..]);

        let x25519_private = x25519_dalek::StaticSecret::from(x25519_prv);
        let x25519_public = x25519_dalek::PublicKey::from(&x25519_private);

        let ed25519_signing = ed25519_dalek::SigningKey::from_bytes(&ed25519_prv);
        let ed25519_verifying = ed25519_signing.verifying_key();

        let hash = Self::compute_hash(&x25519_public, &ed25519_verifying);

        Ok(Self {
            x25519_private: Some(x25519_private),
            x25519_public,
            ed25519_signing: Some(ed25519_signing),
            ed25519_verifying,
            hash,
        })
    }

    /// Get the identity hash (truncated hash of public keys)
    pub fn hash(&self) -> &[u8; IDENTITY_HASHBYTES] {
        &self.hash
    }

    /// Get the combined public key bytes (64 bytes)
    pub fn public_key_bytes(&self) -> [u8; IDENTITY_KEY_SIZE] {
        let mut bytes = [0u8; IDENTITY_KEY_SIZE];
        bytes[..X25519_KEY_SIZE].copy_from_slice(self.x25519_public.as_bytes());
        bytes[X25519_KEY_SIZE..].copy_from_slice(self.ed25519_verifying.as_bytes());
        bytes
    }

    /// Get the private key bytes (64 bytes) if available
    pub fn private_key_bytes(&self) -> Result<[u8; IDENTITY_KEY_SIZE], IdentityError> {
        let x25519_prv = self
            .x25519_private
            .as_ref()
            .ok_or(IdentityError::NoPrivateKey)?;
        let ed25519_prv = self
            .ed25519_signing
            .as_ref()
            .ok_or(IdentityError::NoPrivateKey)?;

        let mut bytes = [0u8; IDENTITY_KEY_SIZE];
        bytes[..X25519_KEY_SIZE].copy_from_slice(x25519_prv.as_bytes());
        bytes[X25519_KEY_SIZE..].copy_from_slice(ed25519_prv.as_bytes());
        Ok(bytes)
    }

    /// Check if this identity has private keys
    pub fn has_private_keys(&self) -> bool {
        self.x25519_private.is_some() && self.ed25519_signing.is_some()
    }

    /// Sign a message with the Ed25519 key
    pub fn sign(&self, message: &[u8]) -> Result<[u8; ED25519_SIGNATURE_SIZE], IdentityError> {
        use ed25519_dalek::Signer;

        let signing_key = self
            .ed25519_signing
            .as_ref()
            .ok_or(IdentityError::NoPrivateKey)?;
        let signature = signing_key.sign(message);
        Ok(signature.to_bytes())
    }

    /// Verify a signature
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> Result<bool, IdentityError> {
        use ed25519_dalek::Verifier;

        if signature.len() != ED25519_SIGNATURE_SIZE {
            return Ok(false);
        }

        let sig_bytes: [u8; ED25519_SIGNATURE_SIZE] = signature
            .try_into()
            .map_err(|_| IdentityError::InvalidSignature)?;
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);

        Ok(self.ed25519_verifying.verify(message, &sig).is_ok())
    }

    /// Create proof data by signing a packet hash
    ///
    /// A proof is cryptographic evidence that a packet was received. It consists
    /// of the packet hash followed by a signature of that hash.
    ///
    /// # Arguments
    /// * `packet_hash` - The SHA256 hash of the received packet (32 bytes)
    ///
    /// # Returns
    /// Explicit format proof data: `[packet_hash (32)] + [signature (64)]` = 96 bytes
    ///
    /// # Errors
    /// Returns `NoPrivateKey` if this identity has no signing key.
    ///
    /// # Example
    /// ```
    /// use leviculum_core::identity::Identity;
    /// use rand_core::OsRng;
    ///
    /// let identity = Identity::generate(&mut OsRng);
    /// let packet_hash = [0x42u8; 32];
    /// let proof = identity.create_proof(&packet_hash).unwrap();
    /// assert_eq!(proof.len(), 96);
    /// ```
    pub fn create_proof(
        &self,
        packet_hash: &[u8; 32],
    ) -> Result<[u8; crate::constants::PROOF_DATA_SIZE], IdentityError> {
        let signature = self.sign(packet_hash)?;
        let mut proof = [0u8; crate::constants::PROOF_DATA_SIZE];
        proof[..32].copy_from_slice(packet_hash);
        proof[32..].copy_from_slice(&signature);
        Ok(proof)
    }

    /// Verify a proof against a packet hash
    ///
    /// # Arguments
    /// * `proof_data` - The proof data (96 bytes: hash + signature)
    /// * `expected_hash` - The expected packet hash
    ///
    /// # Returns
    /// `true` if the proof is valid (hash matches and signature verifies)
    pub fn verify_proof(&self, proof_data: &[u8], expected_hash: &[u8; 32]) -> bool {
        if proof_data.len() != crate::constants::PROOF_DATA_SIZE {
            return false;
        }

        // Check that the hash in the proof matches the expected hash
        if &proof_data[..32] != expected_hash {
            return false;
        }

        // Verify the signature
        self.verify(&proof_data[..32], &proof_data[32..])
            .unwrap_or(false)
    }

    /// Compute identity hash from public keys
    fn compute_hash(
        x25519_pub: &x25519_dalek::PublicKey,
        ed25519_pub: &ed25519_dalek::VerifyingKey,
    ) -> [u8; IDENTITY_HASHBYTES] {
        let mut combined = [0u8; IDENTITY_KEY_SIZE];
        combined[..X25519_KEY_SIZE].copy_from_slice(x25519_pub.as_bytes());
        combined[X25519_KEY_SIZE..].copy_from_slice(ed25519_pub.as_bytes());
        truncated_hash(&combined)
    }

    /// Get the Ed25519 verifying key
    pub fn ed25519_verifying(&self) -> &ed25519_dalek::VerifyingKey {
        &self.ed25519_verifying
    }

    /// Get the Ed25519 signing key (if this is a full identity with private keys)
    pub(crate) fn ed25519_signing_key(&self) -> Option<&ed25519_dalek::SigningKey> {
        self.ed25519_signing.as_ref()
    }

    /// Derived key length for encryption (64 bytes: 32 signing + 32 encryption)
    const DERIVED_KEY_LENGTH: usize = 64;

    /// Encrypt data for this identity
    ///
    /// Uses ephemeral ECDH key exchange followed by token encryption.
    /// Format: [ephemeral_pub (32)] [token (variable)]
    ///
    /// # Arguments
    /// * `plaintext` - Data to encrypt
    /// * `rng` - Random number generator
    ///
    /// # Returns
    /// Ciphertext that can be decrypted by the holder of this identity's private key
    pub fn encrypt<R: rand_core::CryptoRngCore>(
        &self,
        plaintext: &[u8],
        rng: &mut R,
    ) -> Result<Vec<u8>, IdentityError> {
        // Generate ephemeral X25519 key pair
        let ephemeral_private = x25519_dalek::StaticSecret::random_from_rng(&mut *rng);

        // Generate random IV
        let mut iv = [0u8; AES_BLOCK_SIZE];
        rng.fill_bytes(&mut iv);

        self.encrypt_impl(&ephemeral_private, plaintext, &iv)
    }

    /// Encrypt data for this identity with a specific ephemeral key and IV (for testing)
    pub fn encrypt_with_keys(
        &self,
        plaintext: &[u8],
        ephemeral_private_bytes: &[u8; X25519_KEY_SIZE],
        iv: &[u8; AES_BLOCK_SIZE],
    ) -> Result<Vec<u8>, IdentityError> {
        let ephemeral_private = x25519_dalek::StaticSecret::from(*ephemeral_private_bytes);
        self.encrypt_impl(&ephemeral_private, plaintext, iv)
    }

    /// Internal encryption implementation used by both encrypt and encrypt_with_keys
    fn encrypt_impl(
        &self,
        ephemeral_private: &x25519_dalek::StaticSecret,
        plaintext: &[u8],
        iv: &[u8; AES_BLOCK_SIZE],
    ) -> Result<Vec<u8>, IdentityError> {
        let ephemeral_public = x25519_dalek::PublicKey::from(ephemeral_private);

        // Perform ECDH
        let shared_key = ephemeral_private.diffie_hellman(&self.x25519_public);

        // Derive encryption key using HKDF (salt = identity hash, context = None)
        let mut derived_key = [0u8; Self::DERIVED_KEY_LENGTH];
        derive_key(
            shared_key.as_bytes(),
            Some(&self.hash),
            None,
            &mut derived_key,
        );

        // Calculate token size: IV + padded ciphertext + HMAC
        let padded_len = ((plaintext.len() / AES_BLOCK_SIZE) + 1) * AES_BLOCK_SIZE;
        let token_len = AES_BLOCK_SIZE + padded_len + 32; // IV + ciphertext + HMAC

        // Allocate output: ephemeral_pub + token
        let mut output = alloc::vec![0u8; X25519_KEY_SIZE + token_len];

        // Write ephemeral public key
        output[..X25519_KEY_SIZE].copy_from_slice(ephemeral_public.as_bytes());

        // Encrypt with token
        let token_size = encrypt_token(&derived_key, iv, plaintext, &mut output[X25519_KEY_SIZE..])
            .map_err(|_| IdentityError::EncryptionFailed)?;

        output.truncate(X25519_KEY_SIZE + token_size);
        Ok(output)
    }

    /// Decrypt data encrypted for this identity
    ///
    /// # Arguments
    /// * `ciphertext` - Data encrypted with `encrypt()`
    ///
    /// # Returns
    /// Decrypted plaintext, or error if decryption fails
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, IdentityError> {
        let x25519_prv = self
            .x25519_private
            .as_ref()
            .ok_or(IdentityError::NoPrivateKey)?;

        // Minimum size: ephemeral_pub (32) + IV (16) + one block (16) + HMAC (32) = 96
        if ciphertext.len() < X25519_KEY_SIZE + AES_BLOCK_SIZE + AES_BLOCK_SIZE + 32 {
            return Err(IdentityError::DecryptionFailed);
        }

        // Extract ephemeral public key
        let mut peer_pub_bytes = [0u8; X25519_KEY_SIZE];
        peer_pub_bytes.copy_from_slice(&ciphertext[..X25519_KEY_SIZE]);
        let peer_public = x25519_dalek::PublicKey::from(peer_pub_bytes);

        // Perform ECDH
        let shared_key = x25519_prv.diffie_hellman(&peer_public);

        // Derive decryption key
        let mut derived_key = [0u8; Self::DERIVED_KEY_LENGTH];
        derive_key(
            shared_key.as_bytes(),
            Some(&self.hash),
            None,
            &mut derived_key,
        );

        // Decrypt token
        let token = &ciphertext[X25519_KEY_SIZE..];
        let mut plaintext = alloc::vec![0u8; token.len()];

        let plaintext_len = decrypt_token(&derived_key, token, &mut plaintext)
            .map_err(|_| IdentityError::DecryptionFailed)?;

        plaintext.truncate(plaintext_len);
        Ok(plaintext)
    }

    // Ratchet Encryption Support
    /// Encrypt data for a destination using an optional ratchet for forward secrecy
    ///
    /// If a ratchet public key is provided, ECDH is performed with the ratchet
    /// instead of the identity's X25519 public key, providing forward secrecy.
    ///
    /// # Arguments
    /// * `plaintext` - Data to encrypt
    /// * `ratchet_public` - Optional ratchet public key (32 bytes)
    /// * `rng` - Random number generator
    ///
    /// # Returns
    /// Ciphertext that can be decrypted by the destination
    ///
    /// # Format
    /// `[ephemeral_pub (32)] [token (variable)]`
    ///
    /// The token contains: `[IV (16)] [ciphertext (variable)] [HMAC (32)]`
    pub(crate) fn encrypt_for_destination<R: rand_core::CryptoRngCore>(
        &self,
        plaintext: &[u8],
        ratchet_public: Option<&[u8; crate::constants::RATCHET_SIZE]>,
        rng: &mut R,
    ) -> Result<Vec<u8>, IdentityError> {
        // Generate ephemeral X25519 key pair
        let ephemeral_private = x25519_dalek::StaticSecret::random_from_rng(&mut *rng);

        // Generate random IV
        let mut iv = [0u8; AES_BLOCK_SIZE];
        rng.fill_bytes(&mut iv);

        self.encrypt_for_destination_impl(&ephemeral_private, plaintext, ratchet_public, &iv)
    }

    /// Internal implementation for ratchet encryption
    fn encrypt_for_destination_impl(
        &self,
        ephemeral_private: &x25519_dalek::StaticSecret,
        plaintext: &[u8],
        ratchet_public: Option<&[u8; crate::constants::RATCHET_SIZE]>,
        iv: &[u8; AES_BLOCK_SIZE],
    ) -> Result<Vec<u8>, IdentityError> {
        let ephemeral_public = x25519_dalek::PublicKey::from(ephemeral_private);

        // Perform ECDH with ratchet or identity key
        let target_public = match ratchet_public {
            Some(ratchet) => x25519_dalek::PublicKey::from(*ratchet),
            None => self.x25519_public,
        };
        let shared_key = ephemeral_private.diffie_hellman(&target_public);

        // Derive encryption key using HKDF (salt = identity hash, context = None)
        let mut derived_key = [0u8; Self::DERIVED_KEY_LENGTH];
        derive_key(
            shared_key.as_bytes(),
            Some(&self.hash),
            None,
            &mut derived_key,
        );

        // Calculate token size: IV + padded ciphertext + HMAC
        let padded_len = ((plaintext.len() / AES_BLOCK_SIZE) + 1) * AES_BLOCK_SIZE;
        let token_len = AES_BLOCK_SIZE + padded_len + 32; // IV + ciphertext + HMAC

        // Allocate output: ephemeral_pub + token
        let mut output = alloc::vec![0u8; X25519_KEY_SIZE + token_len];

        // Write ephemeral public key
        output[..X25519_KEY_SIZE].copy_from_slice(ephemeral_public.as_bytes());

        // Encrypt with token
        let token_size = encrypt_token(&derived_key, iv, plaintext, &mut output[X25519_KEY_SIZE..])
            .map_err(|_| IdentityError::EncryptionFailed)?;

        output.truncate(X25519_KEY_SIZE + token_size);
        Ok(output)
    }

    /// Decrypt data that may have been encrypted with a ratchet
    ///
    /// Tries decryption with each provided ratchet in order, falling back
    /// to the identity's X25519 key if no ratchet succeeds (and fallback is enabled).
    ///
    /// # Arguments
    /// * `ciphertext` - Data to decrypt
    /// * `ratchets` - List of ratchets to try (newest first recommended)
    /// * `allow_identity_fallback` - If true, try identity key after all ratchets fail
    ///
    /// # Returns
    /// Tuple of (plaintext, ratchet_id_used) on success.
    /// `ratchet_id_used` is None if decrypted with identity key.
    ///
    /// # Errors
    /// Returns `DecryptionFailed` if no ratchet or identity key can decrypt.
    pub(crate) fn decrypt_with_ratchets(
        &self,
        ciphertext: &[u8],
        ratchets: &[crate::ratchet::Ratchet],
        allow_identity_fallback: bool,
    ) -> Result<(Vec<u8>, Option<[u8; crate::ratchet::RATCHET_ID_SIZE]>), IdentityError> {
        // Minimum size: ephemeral_pub (32) + IV (16) + one block (16) + HMAC (32) = 96
        if ciphertext.len() < X25519_KEY_SIZE + AES_BLOCK_SIZE + AES_BLOCK_SIZE + 32 {
            return Err(IdentityError::DecryptionFailed);
        }

        // Extract ephemeral public key
        let mut peer_pub_bytes = [0u8; X25519_KEY_SIZE];
        peer_pub_bytes.copy_from_slice(&ciphertext[..X25519_KEY_SIZE]);
        let peer_public = x25519_dalek::PublicKey::from(peer_pub_bytes);

        let token = &ciphertext[X25519_KEY_SIZE..];

        // Try each ratchet
        for ratchet in ratchets {
            if let Ok(plaintext) =
                self.try_decrypt_with_key(ratchet.private_key(), &peer_public, token)
            {
                return Ok((plaintext, Some(ratchet.id())));
            }
        }

        // Try identity key as fallback
        if allow_identity_fallback {
            if let Some(x25519_prv) = &self.x25519_private {
                if let Ok(plaintext) = self.try_decrypt_with_key(x25519_prv, &peer_public, token) {
                    return Ok((plaintext, None));
                }
            }
        }

        Err(IdentityError::DecryptionFailed)
    }

    /// Try decryption with a specific private key
    fn try_decrypt_with_key(
        &self,
        private_key: &x25519_dalek::StaticSecret,
        peer_public: &x25519_dalek::PublicKey,
        token: &[u8],
    ) -> Result<Vec<u8>, IdentityError> {
        // Perform ECDH
        let shared_key = private_key.diffie_hellman(peer_public);

        // Derive decryption key
        let mut derived_key = [0u8; Self::DERIVED_KEY_LENGTH];
        derive_key(
            shared_key.as_bytes(),
            Some(&self.hash),
            None,
            &mut derived_key,
        );

        // Decrypt token
        let mut plaintext = alloc::vec![0u8; token.len()];

        let plaintext_len = decrypt_token(&derived_key, token, &mut plaintext)
            .map_err(|_| IdentityError::DecryptionFailed)?;

        plaintext.truncate(plaintext_len);
        Ok(plaintext)
    }
}

// No Default impl - use Identity::generate(ctx) instead

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    // Helper to create identity in tests (uses OsRng directly)
    fn new_identity() -> Identity {
        Identity::generate(&mut OsRng)
    }

    #[test]
    fn test_identity_creation() {
        let identity = new_identity();
        assert!(identity.has_private_keys());
        assert_eq!(identity.hash().len(), IDENTITY_HASHBYTES);
    }

    #[test]
    fn test_sign_verify() {
        let identity = new_identity();
        let message = b"Test message for signing";

        let signature = identity.sign(message).unwrap();
        assert_eq!(signature.len(), ED25519_SIGNATURE_SIZE);

        assert!(identity.verify(message, &signature).unwrap());
        assert!(!identity.verify(b"Wrong message", &signature).unwrap());
    }

    /// Known-answer test: Ed25519 signature against RFC 8032 Section 7.1 TEST 1.
    ///
    /// This pins our signature OUTPUT to fixed authoritative bytes so a
    /// wire-incompatible-but-self-consistent crypto swap is caught independent
    /// of the Python interop harness.
    ///
    /// Level pinned: Identity::sign (our public signing API). Identity derives
    /// the Ed25519 key from the 32-byte seed via ed25519_dalek::SigningKey::from_bytes
    /// (see from_private_key_bytes) and signs the RAW message with PureEdDSA. This is
    /// bit-identical to a raw ed25519-dalek keypair built from the same seed, so the
    /// RFC 8032 seed -> signature vector applies directly at the Identity level. The
    /// X25519 half of the private key is independent of Ed25519 signing and is set to
    /// the same seed here purely to satisfy the 64-byte constructor.
    ///
    /// Source: RFC 8032 Section 7.1, TEST 1 (empty message). Verified independently
    /// with the Python `cryptography` library: the seed reproduces the RFC public key
    /// d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a and the
    /// deterministic signature below.
    #[test]
    fn kat_ed25519_sign_rfc8032() {
        // RFC 8032 Section 7.1 TEST 1 seed (32 bytes)
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        // RFC 8032 Section 7.1 TEST 1 message is the empty string.
        let message: &[u8] = b"";
        // RFC 8032 Section 7.1 TEST 1 signature (64 bytes)
        let expected_sig: [u8; ED25519_SIGNATURE_SIZE] = [
            0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e,
            0x82, 0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65,
            0x22, 0x49, 0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e,
            0x39, 0x70, 0x1c, 0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24,
            0x65, 0x51, 0x41, 0x43, 0x8e, 0x7a, 0x10, 0x0b,
        ];

        // Build a private-key identity whose Ed25519 half is the RFC seed. The
        // X25519 half is irrelevant to signing; reuse the seed to fill 64 bytes.
        let mut priv_bytes = [0u8; IDENTITY_KEY_SIZE];
        priv_bytes[..X25519_KEY_SIZE].copy_from_slice(&seed);
        priv_bytes[X25519_KEY_SIZE..].copy_from_slice(&seed);
        let identity = Identity::from_private_key_bytes(&priv_bytes).unwrap();

        let sig = identity.sign(message).unwrap();
        assert_eq!(
            sig, expected_sig,
            "Ed25519 signature must match RFC 8032 7.1 TEST 1"
        );
        // The signature verifies under the RFC public key as well.
        assert!(identity.verify(message, &sig).unwrap());
    }

    /// Known-answer test: X25519 scalar multiplication against RFC 7748 Section 5.2.
    ///
    /// Pins the X25519 ECDH primitive (x25519_dalek) that Identity uses for key
    /// agreement to fixed authoritative output bytes. The RFC input scalar is fed
    /// raw; X25519 performs the mandated clamping internally, matching the RFC's
    /// X25519(k, u) definition.
    ///
    /// Source: RFC 7748 Section 5.2, first X25519 test vector. Verified independently
    /// with the Python `cryptography` library (X25519PrivateKey.exchange).
    #[test]
    fn kat_x25519_ecdh_rfc7748() {
        // RFC 7748 Section 5.2 input scalar (32 bytes)
        let scalar: [u8; 32] = [
            0xa5, 0x46, 0xe3, 0x6b, 0xf0, 0x52, 0x7c, 0x9d, 0x3b, 0x16, 0x15, 0x4b, 0x82, 0x46,
            0x5e, 0xdd, 0x62, 0x14, 0x4c, 0x0a, 0xc1, 0xfc, 0x5a, 0x18, 0x50, 0x6a, 0x22, 0x44,
            0xba, 0x44, 0x9a, 0xc4,
        ];
        // RFC 7748 Section 5.2 input u-coordinate (32 bytes)
        let u: [u8; 32] = [
            0xe6, 0xdb, 0x68, 0x67, 0x58, 0x30, 0x30, 0xdb, 0x35, 0x94, 0xc1, 0xa4, 0x24, 0xb1,
            0x5f, 0x7c, 0x72, 0x66, 0x24, 0xec, 0x26, 0xb3, 0x35, 0x3b, 0x10, 0xa9, 0x03, 0xa6,
            0xd0, 0xab, 0x1c, 0x4c,
        ];
        // RFC 7748 Section 5.2 expected output (32 bytes)
        let expected: [u8; 32] = [
            0xc3, 0xda, 0x55, 0x37, 0x9d, 0xe9, 0xc6, 0x90, 0x8e, 0x94, 0xea, 0x4d, 0xf2, 0x8d,
            0x08, 0x4f, 0x32, 0xec, 0xcf, 0x03, 0x49, 0x1c, 0x71, 0xf7, 0x54, 0xb4, 0x07, 0x55,
            0x77, 0xa2, 0x85, 0x52,
        ];

        let secret = x25519_dalek::StaticSecret::from(scalar);
        let point = x25519_dalek::PublicKey::from(u);
        let shared = secret.diffie_hellman(&point);
        assert_eq!(
            shared.to_bytes(),
            expected,
            "X25519 output must match RFC 7748 5.2"
        );
    }

    #[test]
    fn test_key_serialization() {
        let identity = new_identity();

        // Test public key roundtrip
        let pub_bytes = identity.public_key_bytes();
        let pub_only = Identity::from_public_key_bytes(&pub_bytes).unwrap();
        assert_eq!(pub_only.hash(), identity.hash());
        assert!(!pub_only.has_private_keys());

        // Test private key roundtrip
        let prv_bytes = identity.private_key_bytes().unwrap();
        let restored = Identity::from_private_key_bytes(&prv_bytes).unwrap();
        assert_eq!(restored.hash(), identity.hash());
        assert!(restored.has_private_keys());
    }

    #[test]
    fn test_public_only_cannot_sign() {
        let identity = new_identity();
        let pub_bytes = identity.public_key_bytes();
        let pub_only = Identity::from_public_key_bytes(&pub_bytes).unwrap();

        let result = pub_only.sign(b"test");
        assert_eq!(result, Err(IdentityError::NoPrivateKey));
    }

    // ==================== EDGE CASE TESTS ====================

    #[test]
    fn test_public_only_cannot_decrypt() {
        let identity = new_identity();
        let pub_bytes = identity.public_key_bytes();
        let pub_only = Identity::from_public_key_bytes(&pub_bytes).unwrap();

        // Encrypt something for this identity
        let plaintext = b"Secret message";
        let ciphertext = identity.encrypt(plaintext, &mut OsRng).unwrap();

        // Public-only identity should not be able to decrypt
        let result = pub_only.decrypt(&ciphertext);
        assert_eq!(result, Err(IdentityError::NoPrivateKey));
    }

    #[test]
    fn test_public_only_cannot_get_private_key() {
        let identity = new_identity();
        let pub_bytes = identity.public_key_bytes();
        let pub_only = Identity::from_public_key_bytes(&pub_bytes).unwrap();

        let result = pub_only.private_key_bytes();
        assert_eq!(result, Err(IdentityError::NoPrivateKey));
    }

    #[test]
    fn test_decrypt_with_wrong_identity() {
        let alice = new_identity();
        let bob = new_identity();

        // Encrypt for Alice
        let plaintext = b"Secret for Alice";
        let ciphertext = alice.encrypt(plaintext, &mut OsRng).unwrap();

        // Bob should not be able to decrypt
        let result = bob.decrypt(&ciphertext);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_signature_wrong_identity() {
        let alice = new_identity();
        let bob = new_identity();

        let message = b"Signed by Alice";
        let signature = alice.sign(message).unwrap();

        // Bob should not verify Alice's signature
        assert!(!bob.verify(message, &signature).unwrap());
    }

    #[test]
    fn test_sign_empty_message() {
        let identity = new_identity();
        let message: &[u8] = b"";

        let signature = identity.sign(message).unwrap();
        assert_eq!(signature.len(), ED25519_SIGNATURE_SIZE);
        assert!(identity.verify(message, &signature).unwrap());
    }

    #[test]
    fn test_encrypt_empty_plaintext() {
        let identity = new_identity();
        let plaintext: &[u8] = b"";

        let ciphertext = identity.encrypt(plaintext, &mut OsRng).unwrap();
        let decrypted = identity.decrypt(&ciphertext).unwrap();

        assert_eq!(decrypted.len(), 0);
    }

    #[test]
    fn test_sign_large_message() {
        let identity = new_identity();
        let message = [0xab; 100000]; // 100KB

        let signature = identity.sign(&message).unwrap();
        assert!(identity.verify(&message, &signature).unwrap());
    }

    #[test]
    fn test_encrypt_large_plaintext() {
        let identity = new_identity();
        let plaintext = [0xab; 100000]; // 100KB

        let ciphertext = identity.encrypt(&plaintext, &mut OsRng).unwrap();
        let decrypted = identity.decrypt(&ciphertext).unwrap();

        assert_eq!(decrypted.len(), plaintext.len());
        assert_eq!(&decrypted[..], &plaintext[..]);
    }

    #[test]
    fn test_invalid_public_key_length() {
        let short_bytes = [0u8; 32]; // Should be 64
        let result = Identity::from_public_key_bytes(&short_bytes);
        assert!(matches!(result, Err(IdentityError::InvalidKeyLength)));
    }

    #[test]
    fn test_invalid_private_key_length() {
        let short_bytes = [0u8; 32]; // Should be 64
        let result = Identity::from_private_key_bytes(&short_bytes);
        assert!(matches!(result, Err(IdentityError::InvalidKeyLength)));
    }

    #[test]
    fn test_invalid_signature_length() {
        let identity = new_identity();
        let short_sig = [0u8; 32]; // Should be 64

        // Short signature should return false, not error
        let result = identity.verify(b"message", &short_sig);
        assert_eq!(result, Ok(false));
    }

    #[test]
    fn test_corrupted_signature() {
        let identity = new_identity();
        let message = b"Test message";

        let mut signature = identity.sign(message).unwrap();
        // Corrupt the signature
        signature[0] ^= 0x01;

        assert!(!identity.verify(message, &signature).unwrap());
    }

    #[test]
    fn test_decrypt_corrupted_ephemeral_key() {
        let identity = new_identity();
        let plaintext = b"Secret message";

        let mut ciphertext = identity.encrypt(plaintext, &mut OsRng).unwrap();
        // Corrupt the ephemeral public key (first 32 bytes)
        ciphertext[0] ^= 0x01;

        // Should fail (HMAC will fail because derived key is different)
        let result = identity.decrypt(&ciphertext);
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_corrupted_token() {
        let identity = new_identity();
        let plaintext = b"Secret message";

        let mut ciphertext = identity.encrypt(plaintext, &mut OsRng).unwrap();
        // Corrupt the token part (after ephemeral key)
        ciphertext[40] ^= 0x01;

        let result = identity.decrypt(&ciphertext);
        assert_eq!(result, Err(IdentityError::DecryptionFailed));
    }

    #[test]
    fn test_decrypt_truncated_ciphertext() {
        let identity = new_identity();
        let plaintext = b"Secret message";

        let ciphertext = identity.encrypt(plaintext, &mut OsRng).unwrap();
        // Truncate to less than minimum size
        let truncated = &ciphertext[..32];

        let result = identity.decrypt(truncated);
        assert_eq!(result, Err(IdentityError::DecryptionFailed));
    }

    #[test]
    fn test_each_identity_unique_hash() {
        let id1 = new_identity();
        let id2 = new_identity();

        assert_ne!(id1.hash(), id2.hash());
    }

    #[test]
    fn test_identity_hash_deterministic() {
        let identity = new_identity();
        let prv_bytes = identity.private_key_bytes().unwrap();

        let restored = Identity::from_private_key_bytes(&prv_bytes).unwrap();

        assert_eq!(identity.hash(), restored.hash());
    }

    #[test]
    fn test_signature_deterministic() {
        // Ed25519 signatures are deterministic
        let identity = new_identity();
        let message = b"Test message";

        let sig1 = identity.sign(message).unwrap();
        let sig2 = identity.sign(message).unwrap();

        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_encryption_not_deterministic() {
        // Each encryption should produce different ciphertext (random ephemeral key + IV)
        let identity = new_identity();
        let plaintext = b"Same message";

        let ct1 = identity.encrypt(plaintext, &mut OsRng).unwrap();
        let ct2 = identity.encrypt(plaintext, &mut OsRng).unwrap();

        // Ciphertexts should be different
        assert_ne!(ct1, ct2);

        // But both should decrypt to the same plaintext
        assert_eq!(identity.decrypt(&ct1).unwrap(), plaintext);
        assert_eq!(identity.decrypt(&ct2).unwrap(), plaintext);
    }

    #[test]
    fn test_encrypt_for_public_only_identity() {
        // Should be able to encrypt for a public-only identity
        let identity = new_identity();
        let pub_bytes = identity.public_key_bytes();
        let pub_only = Identity::from_public_key_bytes(&pub_bytes).unwrap();

        let plaintext = b"Secret message";
        let ciphertext = pub_only.encrypt(plaintext, &mut OsRng).unwrap();

        // Original identity (with private key) should be able to decrypt
        let decrypted = identity.decrypt(&ciphertext).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_multiple_encrypt_decrypt_cycles() {
        let identity = new_identity();

        for i in 0..10 {
            let plaintext = alloc::format!("Message number {}", i);
            let ciphertext = identity.encrypt(plaintext.as_bytes(), &mut OsRng).unwrap();
            let decrypted = identity.decrypt(&ciphertext).unwrap();
            assert_eq!(&decrypted[..], plaintext.as_bytes());
        }
    }

    #[test]
    fn test_different_identities_different_public_keys() {
        let id1 = new_identity();
        let id2 = new_identity();

        assert_ne!(id1.public_key_bytes(), id2.public_key_bytes());
    }

    #[test]
    fn test_sign_then_verify_roundtrip() {
        let signer = new_identity();
        let pub_bytes = signer.public_key_bytes();
        let verifier = Identity::from_public_key_bytes(&pub_bytes).unwrap();

        let message = b"Important document";
        let signature = signer.sign(message).unwrap();

        // Verifier (public-only) should be able to verify
        assert!(verifier.verify(message, &signature).unwrap());
    }

    #[test]
    fn test_encrypt_single_byte() {
        let identity = new_identity();
        let plaintext = [0xaa];

        let ciphertext = identity.encrypt(&plaintext, &mut OsRng).unwrap();
        let decrypted = identity.decrypt(&ciphertext).unwrap();

        assert_eq!(decrypted.len(), 1);
        assert_eq!(decrypted[0], 0xaa);
    }

    #[test]
    fn test_verify_tampered_message() {
        let identity = new_identity();
        let message = b"Original message";
        let signature = identity.sign(message).unwrap();

        // Tampered message should not verify
        let tampered = b"Tampered message";
        assert!(!identity.verify(tampered, &signature).unwrap());
    }

    #[test]
    fn test_decrypt_minimum_ciphertext_size() {
        let identity = new_identity();
        // Minimum: ephemeral_pub (32) + IV (16) + one block (16) + HMAC (32) = 96
        let too_short = [0u8; 95];

        let result = identity.decrypt(&too_short);
        assert_eq!(result, Err(IdentityError::DecryptionFailed));
    }

    // Ratchet Encryption Tests
    #[test]
    fn test_encrypt_for_destination_without_ratchet() {
        // Without a ratchet, should work the same as regular encrypt
        let identity = new_identity();
        let plaintext = b"Hello without ratchet";

        let ciphertext = identity
            .encrypt_for_destination(plaintext, None, &mut OsRng)
            .unwrap();

        // Should be decryptable with normal decrypt
        let decrypted = identity.decrypt(&ciphertext).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_encrypt_for_destination_with_ratchet() {
        use crate::ratchet::Ratchet;

        const TEST_TIME_MS: u64 = 1704067200000;

        // Create identity and ratchet
        let identity = Identity::generate(&mut OsRng);
        let ratchet = Ratchet::generate(&mut OsRng, TEST_TIME_MS);

        let plaintext = b"Hello with ratchet!";
        let ratchet_pub = ratchet.public_key_bytes();

        // Encrypt with ratchet
        let ciphertext = identity
            .encrypt_for_destination(plaintext, Some(&ratchet_pub), &mut OsRng)
            .unwrap();

        // Should NOT be decryptable with normal decrypt (uses identity key)
        let result = identity.decrypt(&ciphertext);
        assert!(result.is_err());

        // Should be decryptable with decrypt_with_ratchets
        let ratchets = [ratchet];
        let (decrypted, ratchet_id) = identity
            .decrypt_with_ratchets(&ciphertext, &ratchets, false)
            .unwrap();

        assert_eq!(&decrypted[..], plaintext);
        assert!(ratchet_id.is_some());
        assert_eq!(ratchet_id.unwrap(), ratchets[0].id());
    }

    #[test]
    fn test_decrypt_with_multiple_ratchets() {
        use crate::ratchet::Ratchet;

        const TEST_TIME_MS: u64 = 1704067200000;

        let identity = Identity::generate(&mut OsRng);

        // Create multiple ratchets
        let ratchet1 = Ratchet::generate(&mut OsRng, TEST_TIME_MS);
        let ratchet2 = Ratchet::generate(&mut OsRng, TEST_TIME_MS);
        let ratchet3 = Ratchet::generate(&mut OsRng, TEST_TIME_MS);

        // Encrypt with ratchet2
        let plaintext = b"Encrypted with ratchet 2";
        let ciphertext = identity
            .encrypt_for_destination(plaintext, Some(&ratchet2.public_key_bytes()), &mut OsRng)
            .unwrap();

        // Try decrypting with all ratchets (ratchet2 should succeed)
        let ratchets = [ratchet1, ratchet2, ratchet3];
        let (decrypted, ratchet_id) = identity
            .decrypt_with_ratchets(&ciphertext, &ratchets, false)
            .unwrap();

        assert_eq!(&decrypted[..], plaintext);
        assert_eq!(ratchet_id.unwrap(), ratchets[1].id());
    }

    #[test]
    fn test_decrypt_with_identity_fallback() {
        use crate::ratchet::Ratchet;

        const TEST_TIME_MS: u64 = 1704067200000;

        let identity = Identity::generate(&mut OsRng);
        let ratchet = Ratchet::generate(&mut OsRng, TEST_TIME_MS);

        // Encrypt WITHOUT ratchet (uses identity key)
        let plaintext = b"Encrypted with identity key";
        let ciphertext = identity
            .encrypt_for_destination(plaintext, None, &mut OsRng)
            .unwrap();

        // Try decrypting with ratchet (will fail) then fallback to identity
        let ratchets = [ratchet];

        // Without fallback - should fail
        let result = identity.decrypt_with_ratchets(&ciphertext, &ratchets, false);
        assert!(result.is_err());

        // With fallback - should succeed using identity key
        let (decrypted, ratchet_id) = identity
            .decrypt_with_ratchets(&ciphertext, &ratchets, true)
            .unwrap();

        assert_eq!(&decrypted[..], plaintext);
        assert!(ratchet_id.is_none()); // Decrypted with identity, not ratchet
    }

    #[test]
    fn test_decrypt_with_empty_ratchets_and_fallback() {
        let identity = new_identity();
        let plaintext = b"Encrypted with identity";

        // Encrypt with identity
        let ciphertext = identity
            .encrypt_for_destination(plaintext, None, &mut OsRng)
            .unwrap();

        // Empty ratchets list with fallback
        let (decrypted, ratchet_id) = identity
            .decrypt_with_ratchets(&ciphertext, &[], true)
            .unwrap();

        assert_eq!(&decrypted[..], plaintext);
        assert!(ratchet_id.is_none());
    }

    #[test]
    fn test_decrypt_with_ratchets_wrong_identity() {
        use crate::ratchet::Ratchet;

        const TEST_TIME_MS: u64 = 1704067200000;

        let alice = Identity::generate(&mut OsRng);
        let bob = Identity::generate(&mut OsRng);
        let ratchet = Ratchet::generate(&mut OsRng, TEST_TIME_MS);

        // Encrypt for Alice's identity using ratchet
        let plaintext = b"Secret for Alice";
        let ciphertext = alice
            .encrypt_for_destination(plaintext, Some(&ratchet.public_key_bytes()), &mut OsRng)
            .unwrap();

        // Bob cannot decrypt even with the same ratchet (different identity hash used in HKDF)
        let ratchets = [ratchet];
        let result = bob.decrypt_with_ratchets(&ciphertext, &ratchets, true);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypt_for_destination_public_only_identity() {
        use crate::ratchet::Ratchet;

        const TEST_TIME_MS: u64 = 1704067200000;

        // Create identity with private keys
        let full_identity = Identity::generate(&mut OsRng);
        let ratchet = Ratchet::generate(&mut OsRng, TEST_TIME_MS);

        // Create public-only identity
        let pub_bytes = full_identity.public_key_bytes();
        let pub_only = Identity::from_public_key_bytes(&pub_bytes).unwrap();

        // Encrypt using public-only identity with ratchet
        let plaintext = b"Encrypted by sender using public identity";
        let ciphertext = pub_only
            .encrypt_for_destination(plaintext, Some(&ratchet.public_key_bytes()), &mut OsRng)
            .unwrap();

        // Full identity can decrypt with ratchet
        let ratchets = [ratchet];
        let (decrypted, _) = full_identity
            .decrypt_with_ratchets(&ciphertext, &ratchets, false)
            .unwrap();

        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_decrypt_with_ratchets_too_short() {
        let identity = new_identity();
        let too_short = [0u8; 50];

        let result = identity.decrypt_with_ratchets(&too_short, &[], true);
        assert_eq!(result, Err(IdentityError::DecryptionFailed));
    }

    // Proof Tests
    #[test]
    fn test_create_proof() {
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];

        let proof = identity.create_proof(&packet_hash).unwrap();

        // Proof should be 96 bytes (32 hash + 64 signature)
        assert_eq!(proof.len(), 96);

        // First 32 bytes should be the packet hash
        assert_eq!(&proof[..32], &packet_hash);

        // Signature should be valid
        assert!(identity.verify(&packet_hash, &proof[32..]).unwrap());
    }

    #[test]
    fn test_verify_proof() {
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];

        let proof = identity.create_proof(&packet_hash).unwrap();

        // Valid proof should verify
        assert!(identity.verify_proof(&proof, &packet_hash));
    }

    #[test]
    fn test_verify_proof_wrong_hash() {
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];
        let wrong_hash = [0x43u8; 32];

        let proof = identity.create_proof(&packet_hash).unwrap();

        // Proof should not verify against a different hash
        assert!(!identity.verify_proof(&proof, &wrong_hash));
    }

    #[test]
    fn test_verify_proof_wrong_identity() {
        let alice = new_identity();
        let bob = new_identity();
        let packet_hash = [0x42u8; 32];

        // Alice creates a proof
        let proof = alice.create_proof(&packet_hash).unwrap();

        // Bob cannot verify Alice's proof (different signing key)
        assert!(!bob.verify_proof(&proof, &packet_hash));
    }

    #[test]
    fn test_verify_proof_invalid_length() {
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];

        // Too short
        assert!(!identity.verify_proof(&[0u8; 50], &packet_hash));

        // Too long
        assert!(!identity.verify_proof(&[0u8; 100], &packet_hash));
    }

    #[test]
    fn test_create_proof_public_only_fails() {
        let identity = new_identity();
        let pub_bytes = identity.public_key_bytes();
        let pub_only = Identity::from_public_key_bytes(&pub_bytes).unwrap();

        let packet_hash = [0x42u8; 32];
        let result = pub_only.create_proof(&packet_hash);

        assert_eq!(result, Err(IdentityError::NoPrivateKey));
    }

    #[test]
    fn test_proof_deterministic() {
        // Ed25519 signatures are deterministic, so proofs should be too
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];

        let proof1 = identity.create_proof(&packet_hash).unwrap();
        let proof2 = identity.create_proof(&packet_hash).unwrap();

        assert_eq!(proof1, proof2);
    }

    #[test]
    fn test_proof_different_for_different_hashes() {
        let identity = new_identity();
        let hash1 = [0x42u8; 32];
        let hash2 = [0x43u8; 32];

        let proof1 = identity.create_proof(&hash1).unwrap();
        let proof2 = identity.create_proof(&hash2).unwrap();

        // Both hash and signature should differ
        assert_ne!(&proof1[..32], &proof2[..32]);
        assert_ne!(&proof1[32..], &proof2[32..]);
    }
}
