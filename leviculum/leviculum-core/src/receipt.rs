//! Packet receipt tracking for delivery confirmation
//!
//! A PacketReceipt tracks a sent packet awaiting proof of delivery.
//! When the receiver processes a packet, it can generate a cryptographic
//! proof (signature of the packet hash) and send it back.
//!
//! # Usage
//!
//! 1. After sending a packet, create a `PacketReceipt` for it
//! 2. Store receipts in a map keyed by truncated hash
//! 3. When a proof packet arrives, look up the receipt and validate
//! 4. Periodically check for expired receipts
//!
//! # Example
//!
//! ```ignore
//! use leviculum_core::receipt::{PacketReceipt, ReceiptStatus};
//! use leviculum_core::destination::DestinationHash;
//!
//! // After sending a packet
//! let packet_hash = [0x42u8; 32];
//! let dest_hash = DestinationHash::new([0x01u8; 16]);
//! let receipt = PacketReceipt::new(packet_hash, dest_hash, 1000);
//!
//! assert_eq!(receipt.status, ReceiptStatus::Sent);
//! assert!(!receipt.is_expired(1500)); // Not expired yet
//! assert!(receipt.is_expired(32000)); // Expired after timeout
//! ```

// PacketReceipt and ReceiptStatus live in crate::storage_types.

#[cfg(test)]
mod tests {
    use crate::constants::{RECEIPT_TIMEOUT_DEFAULT_MS, TRUNCATED_HASHBYTES};
    use crate::destination::DestinationHash;
    use crate::identity::Identity;
    use crate::storage_types::{PacketReceipt, ReceiptStatus};
    use rand_core::OsRng;

    fn new_identity() -> Identity {
        Identity::generate(&mut OsRng)
    }

    #[test]
    fn test_receipt_creation() {
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::new(packet_hash, dest_hash, 1000);

        assert_eq!(receipt.packet_hash, packet_hash);
        assert_eq!(receipt.destination_hash, dest_hash);
        assert_eq!(receipt.sent_at_ms, 1000);
        assert_eq!(receipt.status, ReceiptStatus::Sent);
        assert_eq!(receipt.timeout_ms, RECEIPT_TIMEOUT_DEFAULT_MS);

        // Truncated hash is the first 16 bytes of the full packet hash
        assert_eq!(receipt.truncated_hash, packet_hash[..TRUNCATED_HASHBYTES]);
    }

    #[test]
    fn test_receipt_with_custom_timeout() {
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::with_timeout(packet_hash, dest_hash, 1000, 5000);

        assert_eq!(receipt.timeout_ms, 5000);
    }

    #[test]
    fn test_receipt_expiry() {
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::with_timeout(packet_hash, dest_hash, 1000, 5000);

        // Not expired at sent time
        assert!(!receipt.is_expired(1000));

        // Not expired just before timeout
        assert!(!receipt.is_expired(5999));

        // Expired at timeout
        assert!(receipt.is_expired(6001));

        // Definitely expired way after
        assert!(receipt.is_expired(100000));
    }

    #[test]
    fn test_receipt_elapsed_remaining() {
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::with_timeout(packet_hash, dest_hash, 1000, 5000);

        // At start
        assert_eq!(receipt.elapsed_ms(1000), 0);
        assert_eq!(receipt.remaining_ms(1000), 5000);

        // Halfway
        assert_eq!(receipt.elapsed_ms(3500), 2500);
        assert_eq!(receipt.remaining_ms(3500), 2500);

        // At timeout
        assert_eq!(receipt.elapsed_ms(6000), 5000);
        assert_eq!(receipt.remaining_ms(6000), 0);

        // Past timeout
        assert_eq!(receipt.elapsed_ms(10000), 9000);
        assert_eq!(receipt.remaining_ms(10000), 0);
    }

    #[test]
    fn test_receipt_status_transitions() {
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let mut receipt = PacketReceipt::new(packet_hash, dest_hash, 1000);

        assert_eq!(receipt.status, ReceiptStatus::Sent);

        receipt.set_delivered();
        assert_eq!(receipt.status, ReceiptStatus::Delivered);

        // Can also transition to failed
        let mut receipt2 = PacketReceipt::new(packet_hash, dest_hash, 1000);
        receipt2.set_failed();
        assert_eq!(receipt2.status, ReceiptStatus::Failed);
    }

    #[test]
    fn test_validate_proof_valid() {
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::new(packet_hash, dest_hash, 1000);

        // Create a valid proof
        let proof = identity.create_proof(&packet_hash).unwrap();

        // Validation should succeed
        assert!(receipt.validate_proof(&proof, &identity));
    }

    #[test]
    fn test_validate_proof_wrong_hash() {
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];
        let wrong_hash = [0x43u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::new(packet_hash, dest_hash, 1000);

        // Create proof for wrong hash
        let proof = identity.create_proof(&wrong_hash).unwrap();

        // Validation should fail (hash mismatch)
        assert!(!receipt.validate_proof(&proof, &identity));
    }

    #[test]
    fn test_validate_proof_wrong_identity() {
        let alice = new_identity();
        let bob = new_identity();
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::new(packet_hash, dest_hash, 1000);

        // Alice creates the proof
        let proof = alice.create_proof(&packet_hash).unwrap();

        // Bob cannot validate Alice's proof
        assert!(!receipt.validate_proof(&proof, &bob));
    }

    #[test]
    fn test_validate_proof_invalid_length() {
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::new(packet_hash, dest_hash, 1000);

        // Too short
        assert!(!receipt.validate_proof(&[0u8; 50], &identity));

        // Too long
        assert!(!receipt.validate_proof(&[0u8; 100], &identity));
    }

    #[test]
    fn test_validate_proof_corrupted() {
        let identity = new_identity();
        let packet_hash = [0x42u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);
        let receipt = PacketReceipt::new(packet_hash, dest_hash, 1000);

        // Create valid proof, then corrupt it
        let mut proof = identity.create_proof(&packet_hash).unwrap();
        proof[50] ^= 0x01; // Flip a bit in the signature

        // Validation should fail
        assert!(!receipt.validate_proof(&proof, &identity));
    }

    #[test]
    fn test_truncated_hash_lookup() {
        // Test that receipts can be looked up by truncated hash
        let packet_hash1 = [0x42u8; 32];
        let packet_hash2 = [0x43u8; 32];
        let dest_hash = DestinationHash::new([0x01u8; 16]);

        let receipt1 = PacketReceipt::new(packet_hash1, dest_hash, 1000);
        let receipt2 = PacketReceipt::new(packet_hash2, dest_hash, 1000);

        // Different packet hashes should give different truncated hashes
        assert_ne!(receipt1.truncated_hash, receipt2.truncated_hash);

        // Truncated hash should be first 16 bytes of full hash
        assert_eq!(receipt1.truncated_hash, packet_hash1[..TRUNCATED_HASHBYTES]);
    }
}
