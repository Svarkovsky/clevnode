//! Interface Access Code (IFAC) implementation
//!
//! IFAC provides authenticated access control for Reticulum interfaces.
//! It uses Ed25519 signatures and XOR masking to authenticate packets
//! at the interface level, preventing unauthorized nodes from communicating
//! on protected network segments.
//!
//! # How IFAC Works
//!
//! 1. **Key Derivation**: A network name and/or passphrase are hashed
//!    and used to derive a 64-byte key via HKDF
//! 2. **Identity Creation**: An Ed25519 identity is created from the derived key
//! 3. **Transmission**: Packets are signed, the signature tail becomes the IFAC,
//!    and the packet is XOR-masked
//! 4. **Reception**: The mask is reversed, IFAC is verified against expected signature
//!
//! # Example
//!
//! ```
//! use leviculum_core::ifac::IfacConfig;
//!
//! // Create IFAC config with network name and passphrase
//! let config = IfacConfig::new(
//!     Some("mynetwork"),
//!     Some("secret123"),
//!     16, // IFAC size in bytes
//! ).unwrap();
//!
//! // Apply IFAC to outgoing packet
//! let raw_packet = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
//! let masked = config.apply_ifac(&raw_packet).unwrap();
//!
//! // Verify IFAC on incoming packet
//! let clean_packet = config.verify_ifac(&masked).unwrap();
//! assert_eq!(clean_packet, raw_packet);
//! ```

use alloc::vec;
use alloc::vec::Vec;

use crate::constants::{IDENTITY_KEY_SIZE, IFAC_MIN_SIZE, IFAC_SALT};
use crate::crypto::{derive_key, full_hash};
use crate::identity::Identity;

/// IFAC-related errors
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfacError {
    /// IFAC size is below minimum
    SizeTooSmall,
    /// Packet is too short to contain IFAC
    PacketTooShort,
    /// IFAC verification failed
    VerificationFailed,
    /// No IFAC flag set on packet that should have one
    MissingIfacFlag,
    /// Identity creation failed
    IdentityError,
}

impl core::fmt::Display for IfacError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IfacError::SizeTooSmall => write!(f, "IFAC size below minimum"),
            IfacError::PacketTooShort => write!(f, "Packet too short for IFAC"),
            IfacError::VerificationFailed => write!(f, "IFAC verification failed"),
            IfacError::MissingIfacFlag => write!(f, "Missing IFAC flag"),
            IfacError::IdentityError => write!(f, "Failed to create IFAC identity"),
        }
    }
}

/// IFAC flag bit mask (bit 7 of first header byte)
const IFAC_FLAG: u8 = 0x80;

/// Configuration for Interface Access Code authentication
#[derive(Clone)]
pub struct IfacConfig {
    /// Size of the IFAC in bytes (typically 8 or 16)
    ifac_size: usize,
    /// Derived 64-byte key for HKDF masking
    ifac_key: [u8; IDENTITY_KEY_SIZE],
    /// Ed25519 identity derived from the key
    ifac_identity: Identity,
}

impl core::fmt::Debug for IfacConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IfacConfig")
            .field("ifac_size", &self.ifac_size)
            .field("identity_hash", &self.ifac_identity.hash())
            .finish_non_exhaustive()
    }
}

impl IfacConfig {
    /// Create a new IFAC configuration
    ///
    /// # Arguments
    /// * `netname` - Optional network name
    /// * `netkey` - Optional network passphrase
    /// * `ifac_size` - Size of IFAC in bytes (minimum: IFAC_MIN_SIZE)
    ///
    /// # Returns
    /// * `Ok(IfacConfig)` if at least one of netname/netkey is provided
    /// * `Err(IfacError)` if configuration is invalid
    ///
    /// # Key Derivation Process
    /// 1. Hash netname (if provided) with SHA-256
    /// 2. Hash netkey (if provided) with SHA-256
    /// 3. Concatenate the hashes
    /// 4. Hash the concatenation
    /// 5. Use HKDF to derive 64-byte key
    /// 6. Create Ed25519 identity from the key
    pub fn new(
        netname: Option<&str>,
        netkey: Option<&str>,
        ifac_size: usize,
    ) -> Result<Self, IfacError> {
        if ifac_size < IFAC_MIN_SIZE {
            return Err(IfacError::SizeTooSmall);
        }

        // Build ifac_origin by concatenating hashes
        let mut ifac_origin = Vec::new();

        if let Some(name) = netname {
            let hash = full_hash(name.as_bytes());
            ifac_origin.extend_from_slice(&hash);
        }

        if let Some(key) = netkey {
            let hash = full_hash(key.as_bytes());
            ifac_origin.extend_from_slice(&hash);
        }

        // Hash the combined origin
        let ifac_origin_hash = full_hash(&ifac_origin);

        // Derive 64-byte key using HKDF
        let mut ifac_key = [0u8; IDENTITY_KEY_SIZE];
        derive_key(&ifac_origin_hash, Some(&IFAC_SALT), None, &mut ifac_key);

        // Create identity from the derived key
        let ifac_identity =
            Identity::from_private_key_bytes(&ifac_key).map_err(|_| IfacError::IdentityError)?;

        Ok(Self {
            ifac_size,
            ifac_key,
            ifac_identity,
        })
    }

    /// Get the IFAC size
    pub fn ifac_size(&self) -> usize {
        self.ifac_size
    }

    /// Get the IFAC identity
    pub fn identity(&self) -> &Identity {
        &self.ifac_identity
    }

    /// Apply IFAC to an outgoing packet
    ///
    /// # Process
    /// 1. Sign the raw packet with Ed25519
    /// 2. Take last `ifac_size` bytes of signature as IFAC
    /// 3. Generate mask stream using HKDF
    /// 4. Set IFAC flag in first byte
    /// 5. Insert IFAC after header (position 2)
    /// 6. Apply XOR mask (but not to IFAC bytes themselves)
    ///
    /// # Arguments
    /// * `raw` - The raw packet data (minimum 2 bytes for header)
    ///
    /// # Returns
    /// The masked packet with IFAC inserted
    pub fn apply_ifac(&self, raw: &[u8]) -> Result<Vec<u8>, IfacError> {
        if raw.len() < 2 {
            return Err(IfacError::PacketTooShort);
        }

        // Step 1: Sign the raw packet and extract IFAC
        let signature = self
            .ifac_identity
            .sign(raw)
            .map_err(|_| IfacError::IdentityError)?;
        let ifac = &signature[signature.len() - self.ifac_size..];

        // Step 2: Generate mask stream
        let mask_len = raw.len() + self.ifac_size;
        let mut mask = vec![0u8; mask_len];
        derive_key(ifac, Some(&self.ifac_key), None, &mut mask);

        // Step 3: Build new packet with IFAC inserted after header
        // Structure: [header(2)] [ifac(N)] [payload...]
        let mut new_raw = Vec::with_capacity(mask_len);
        // Set IFAC flag in first header byte
        new_raw.push(raw[0] | IFAC_FLAG);
        new_raw.push(raw[1]);
        new_raw.extend_from_slice(ifac);
        new_raw.extend_from_slice(&raw[2..]);

        // Step 4: Apply XOR mask
        let mut masked = Vec::with_capacity(mask_len);
        for (i, &byte) in new_raw.iter().enumerate() {
            if i == 0 {
                // Mask first byte but ensure IFAC flag remains set
                masked.push((byte ^ mask[i]) | IFAC_FLAG);
            } else if i == 1 || i > self.ifac_size + 1 {
                // Mask second header byte and payload (after IFAC)
                masked.push(byte ^ mask[i]);
            } else {
                // Don't mask IFAC bytes (indices 2 to ifac_size+1)
                masked.push(byte);
            }
        }

        Ok(masked)
    }

    /// Verify and remove IFAC from an incoming packet
    ///
    /// # Process
    /// 1. Check IFAC flag is set
    /// 2. Extract IFAC from packet
    /// 3. Generate mask stream and unmask
    /// 4. Remove IFAC and clear flag
    /// 5. Verify by signing clean packet and comparing
    ///
    /// # Arguments
    /// * `raw` - The received packet with IFAC
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - The clean packet with IFAC removed
    /// * `Err(IfacError)` - If verification fails
    pub fn verify_ifac(&self, raw: &[u8]) -> Result<Vec<u8>, IfacError> {
        // Minimum: header(2) + ifac(N) + at least some payload
        let min_len = 2 + self.ifac_size;
        if raw.len() < min_len {
            return Err(IfacError::PacketTooShort);
        }

        // Step 1: Check IFAC flag
        if raw[0] & IFAC_FLAG == 0 {
            return Err(IfacError::MissingIfacFlag);
        }

        // Step 2: Extract IFAC (bytes 2 to 2+ifac_size)
        let ifac = &raw[2..2 + self.ifac_size];

        // Step 3: Generate mask stream
        let mut mask = vec![0u8; raw.len()];
        derive_key(ifac, Some(&self.ifac_key), None, &mut mask);

        // Step 4: Unmask the packet
        let mut unmasked = Vec::with_capacity(raw.len());
        for (i, &byte) in raw.iter().enumerate() {
            if i <= 1 || i > self.ifac_size + 1 {
                // Unmask header bytes and payload (after IFAC)
                unmasked.push(byte ^ mask[i]);
            } else {
                // Don't unmask IFAC bytes
                unmasked.push(byte);
            }
        }

        // Step 5: Clear IFAC flag and remove IFAC bytes
        let mut clean = Vec::with_capacity(raw.len() - self.ifac_size);
        clean.push(unmasked[0] & !IFAC_FLAG); // Clear IFAC flag
        clean.push(unmasked[1]);
        clean.extend_from_slice(&unmasked[2 + self.ifac_size..]);

        // Step 6: Verify by signing the clean packet
        let signature = self
            .ifac_identity
            .sign(&clean)
            .map_err(|_| IfacError::IdentityError)?;
        let expected_ifac = &signature[signature.len() - self.ifac_size..];

        // Step 7: Compare IFAC values
        if ifac != expected_ifac {
            return Err(IfacError::VerificationFailed);
        }

        Ok(clean)
    }

    /// Check if a packet has the IFAC flag set
    pub fn has_ifac_flag(raw: &[u8]) -> bool {
        !raw.is_empty() && (raw[0] & IFAC_FLAG != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ifac_config_creation() {
        let config = IfacConfig::new(Some("testnet"), Some("secret"), 16).unwrap();
        assert_eq!(config.ifac_size(), 16);
        assert!(config.identity().has_private_keys());
    }

    #[test]
    fn test_ifac_config_netname_only() {
        let config = IfacConfig::new(Some("testnet"), None, 16).unwrap();
        assert_eq!(config.ifac_size(), 16);
    }

    #[test]
    fn test_ifac_config_netkey_only() {
        let config = IfacConfig::new(None, Some("secret"), 16).unwrap();
        assert_eq!(config.ifac_size(), 16);
    }

    #[test]
    fn test_ifac_size_too_small() {
        let result = IfacConfig::new(Some("test"), None, 0);
        assert_eq!(result.unwrap_err(), IfacError::SizeTooSmall);
    }

    #[test]
    fn test_ifac_roundtrip() {
        let config = IfacConfig::new(Some("testnet"), Some("secret"), 16).unwrap();

        // Create a simple test packet
        let original = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];

        // Apply IFAC
        let masked = config.apply_ifac(&original).unwrap();

        // Verify packet is longer (IFAC added)
        assert_eq!(masked.len(), original.len() + config.ifac_size());

        // Verify IFAC flag is set
        assert!(IfacConfig::has_ifac_flag(&masked));

        // Verify and remove IFAC
        let clean = config.verify_ifac(&masked).unwrap();

        // Should match original
        assert_eq!(clean, original);
    }

    #[test]
    fn test_ifac_roundtrip_packet_larger_than_hkdf_rfc_limit() {
        // The mask spans the whole packet, so a packet above 8160 bytes needs
        // more key material than RFC 5869 allows. TCP negotiates an MTU of
        // 262144, so these packets do occur on IFAC-enabled interfaces.
        let config = IfacConfig::new(Some("testnet"), Some("secret"), 16).unwrap();

        let original: Vec<u8> = (0..16384u32).map(|i| i as u8).collect();

        let masked = config.apply_ifac(&original).unwrap();
        assert_eq!(masked.len(), original.len() + config.ifac_size());

        let clean = config.verify_ifac(&masked).unwrap();
        assert_eq!(clean, original);
    }

    #[test]
    fn test_ifac_flag_detection() {
        assert!(IfacConfig::has_ifac_flag(&[0x80]));
        assert!(IfacConfig::has_ifac_flag(&[0xFF]));
        assert!(!IfacConfig::has_ifac_flag(&[0x7F]));
        assert!(!IfacConfig::has_ifac_flag(&[0x00]));
        assert!(!IfacConfig::has_ifac_flag(&[]));
    }

    #[test]
    fn test_ifac_different_sizes() {
        for size in [1, 8, 16, 32] {
            let config = IfacConfig::new(Some("test"), None, size).unwrap();
            let original = vec![0x00, 0x01, 0x02, 0x03];

            let masked = config.apply_ifac(&original).unwrap();
            assert_eq!(masked.len(), original.len() + size);

            let clean = config.verify_ifac(&masked).unwrap();
            assert_eq!(clean, original);
        }
    }

    #[test]
    fn test_ifac_wrong_key_fails() {
        let config1 = IfacConfig::new(Some("net1"), Some("key1"), 16).unwrap();
        let config2 = IfacConfig::new(Some("net2"), Some("key2"), 16).unwrap();

        let original = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05];

        // Apply with config1
        let masked = config1.apply_ifac(&original).unwrap();

        // Try to verify with config2 - should fail
        let result = config2.verify_ifac(&masked);
        assert_eq!(result.unwrap_err(), IfacError::VerificationFailed);
    }

    #[test]
    fn test_ifac_tampered_packet_fails() {
        let config = IfacConfig::new(Some("test"), Some("secret"), 16).unwrap();

        let original = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let mut masked = config.apply_ifac(&original).unwrap();

        // Tamper with a payload byte
        let last_idx = masked.len() - 1;
        masked[last_idx] ^= 0x01;

        // Verification should fail
        let result = config.verify_ifac(&masked);
        assert_eq!(result.unwrap_err(), IfacError::VerificationFailed);
    }

    #[test]
    fn test_ifac_packet_too_short() {
        let config = IfacConfig::new(Some("test"), None, 16).unwrap();

        // Packet too short for apply
        let result = config.apply_ifac(&[0x00]);
        assert_eq!(result.unwrap_err(), IfacError::PacketTooShort);

        // Packet too short for verify (needs 2 + ifac_size)
        let short_packet = vec![0x80, 0x00, 0x01, 0x02]; // Only 4 bytes, need 18
        let result = config.verify_ifac(&short_packet);
        assert_eq!(result.unwrap_err(), IfacError::PacketTooShort);
    }

    #[test]
    fn test_ifac_missing_flag() {
        let config = IfacConfig::new(Some("test"), None, 8).unwrap();

        // Packet without IFAC flag
        let packet = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let result = config.verify_ifac(&packet);
        assert_eq!(result.unwrap_err(), IfacError::MissingIfacFlag);
    }

    #[test]
    fn test_ifac_deterministic_key_derivation() {
        // Same inputs should produce same identity
        let config1 = IfacConfig::new(Some("mynet"), Some("pass"), 16).unwrap();
        let config2 = IfacConfig::new(Some("mynet"), Some("pass"), 16).unwrap();

        assert_eq!(config1.ifac_key, config2.ifac_key);
        assert_eq!(config1.ifac_identity.hash(), config2.ifac_identity.hash());
    }

    #[test]
    fn test_ifac_different_inputs_different_keys() {
        let config1 = IfacConfig::new(Some("net1"), Some("key"), 16).unwrap();
        let config2 = IfacConfig::new(Some("net2"), Some("key"), 16).unwrap();
        let config3 = IfacConfig::new(Some("net1"), Some("other"), 16).unwrap();

        assert_ne!(config1.ifac_key, config2.ifac_key);
        assert_ne!(config1.ifac_key, config3.ifac_key);
        assert_ne!(config2.ifac_key, config3.ifac_key);
    }

    #[test]
    fn test_ifac_with_all_packet_types() {
        let config = IfacConfig::new(Some("test"), None, 8).unwrap();

        // Test with different first bytes (different packet types/flags)
        for first_byte in [0x00, 0x10, 0x20, 0x40, 0x7F] {
            let original = vec![first_byte, 0xAB, 0xCD, 0xEF];
            let masked = config.apply_ifac(&original).unwrap();
            let clean = config.verify_ifac(&masked).unwrap();
            assert_eq!(clean, original);
        }
    }

    #[test]
    fn test_ifac_empty_payload() {
        let config = IfacConfig::new(Some("test"), None, 8).unwrap();

        // Packet with just header (minimum valid)
        let original = vec![0x00, 0x01];
        let masked = config.apply_ifac(&original).unwrap();
        let clean = config.verify_ifac(&masked).unwrap();
        assert_eq!(clean, original);
    }

    #[test]
    fn test_ifac_large_packet() {
        let config = IfacConfig::new(Some("test"), None, 16).unwrap();

        // Create a large packet
        let mut original = vec![0x00, 0x00];
        original.extend_from_slice(&[0xAB; 400]);

        let masked = config.apply_ifac(&original).unwrap();
        assert_eq!(masked.len(), original.len() + 16);

        let clean = config.verify_ifac(&masked).unwrap();
        assert_eq!(clean, original);
    }
}
