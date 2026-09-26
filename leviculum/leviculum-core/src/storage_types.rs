//! Pure data structs used by the Storage trait and Transport
//!
//! These are value types (DTOs) that cross the boundary between Transport
//! and Storage. They live at Layer 0 to avoid circular imports.

use alloc::vec::Vec;

use crate::constants::{PROOF_DATA_SIZE, RECEIPT_TIMEOUT_DEFAULT_MS, TRUNCATED_HASHBYTES};
use crate::destination::DestinationHash;
use crate::identity::Identity;

// Path Types
/// Path quality state (for path recovery)
///
/// Tracks whether a path is known to be working, unresponsive, or unknown.
/// Used to allow accepting same-emission worse-hop announces when the
/// current path has been marked unresponsive (Python Transport.py:1672-1681).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathState {
    /// Default state, no knowledge about path quality
    Unknown,
    /// Communication attempt failed (unvalidated link expired)
    Unresponsive,
    /// Communication succeeded (defined for API completeness; not used internally)
    Responsive,
}

/// Path table entry
#[derive(Debug, Clone)]
pub struct PathEntry {
    /// Number of hops to destination
    pub hops: u8,
    /// When this path expires (ms since clock epoch)
    pub expires_ms: u64,
    /// Interface index where we learned this path
    pub interface_index: usize,
    /// Random blobs seen for this destination (for replay detection)
    pub random_blobs: Vec<[u8; crate::constants::RANDOM_HASHBYTES]>,
    /// Identity hash of the next relay hop (from announce transport_id)
    pub next_hop: Option<[u8; TRUNCATED_HASHBYTES]>,
    /// The peer link the installing announce arrived through, as reported
    /// by a multi-peer interface (Codeberg #365) — the same 16 bytes the
    /// interface reports on peer-up/peer-lost. `None` when the carrier has
    /// no peer links (LoRa, serial) or the driver did not name one. Lets
    /// the peer-loss cull attribute an entry whose announce identity
    /// differs from the link identity (Columba presents different
    /// identities at the two layers).
    pub via_peer: Option<[u8; TRUNCATED_HASHBYTES]>,
}

impl PathEntry {
    /// Destination is directly connected (no relay needed).
    /// Hops are incremented on receipt: 1 = direct neighbor, 0 = local client.
    /// Matches Python semantics (hops == 1 after receipt increment).
    pub fn is_direct(&self) -> bool {
        self.hops == 1
    }

    /// Destination requires relay forwarding AND we know the next hop.
    pub fn needs_relay(&self) -> bool {
        self.hops > 1 && self.next_hop.is_some()
    }
}

// Link Types
/// Link table entry (for active links routed through this transport node)
#[derive(Debug, Clone)]
pub struct LinkEntry {
    /// When this link was created (ms)
    pub timestamp_ms: u64,
    /// Interface index toward the destination (outbound)
    pub next_hop_interface_index: usize,
    /// Remaining hops to destination
    pub remaining_hops: u8,
    /// Interface index where we received the link request (inbound, toward initiator)
    pub received_interface_index: usize,
    /// Total hops from initiator
    pub hops: u8,
    /// Whether the link has been validated by a proof
    pub validated: bool,
    /// Deadline for receiving a proof (ms), after which the entry is removed
    pub proof_timeout_ms: u64,
    /// Destination hash for path rediscovery on unvalidated link expiry
    pub destination_hash: [u8; TRUNCATED_HASHBYTES],
    /// Responder's Ed25519 signing key (from announce_cache at link creation).
    /// Used for LRPROOF signature validation. None if announce not cached.
    /// Removed when link entry is cleaned up (clean_link_table).
    pub peer_signing_key: Option<[u8; crate::constants::ED25519_KEY_SIZE]>,
}

// Reverse Types
/// Reverse table entry (for routing replies back)
#[derive(Debug, Clone, Copy)]
pub struct ReverseEntry {
    /// When this was learned (ms)
    pub timestamp_ms: u64,
    /// Interface index where the original packet was received
    pub receiving_interface_index: usize,
    /// Interface index where the packet was forwarded to
    pub outbound_interface_index: usize,
}

// Announce Types
/// Announce table entry (for rate limiting and rebroadcast tracking)
#[derive(Debug, Clone)]
pub struct AnnounceEntry {
    /// When we received this announce (ms)
    pub timestamp_ms: u64,
    /// Number of hops when received
    pub hops: u8,
    /// Number of retransmit attempts
    pub retries: u8,
    /// When to retransmit (ms, None = don't)
    pub retransmit_at_ms: Option<u64>,
    /// Raw packet bytes stored for rebroadcast
    pub raw_packet: Vec<u8>,
    /// Interface index this announce arrived on
    pub receiving_interface_index: usize,
    /// If set, send the deferred rebroadcast only to this specific interface
    /// instead of broadcasting to all. Used for path request responses, which
    /// should go only to the requesting interface (Python Transport.py:1037-1038).
    pub target_interface: Option<usize>,
    /// Number of times neighbors echoed this announce
    pub local_rebroadcasts: u8,
    /// If true, do not re-rebroadcast (PATH_RESPONSE context)
    pub block_rebroadcasts: bool,
}

/// Per-destination announce rate tracking entry (Python: announce_rate_table)
///
/// Tracks violations when a destination announces too frequently and blocks
/// rebroadcast (but not path table updates) when violations exceed grace.
#[derive(Debug, Clone, Copy)]
pub struct AnnounceRateEntry {
    /// Timestamp of last accepted (non-violating) announce (ms)
    pub last_ms: u64,
    /// Number of rate violations (incremented on too-fast, decremented on good-rate)
    pub rate_violations: u8,
    /// Announces are blocked until this timestamp (ms)
    pub blocked_until_ms: u64,
}

// Receipt Types
/// Status of a packet receipt
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptStatus {
    /// Packet was sent, awaiting proof
    Sent,
    /// Proof received and validated - packet was delivered
    Delivered,
    /// Receipt timed out or proof validation failed
    Failed,
}

/// Tracks a sent packet awaiting proof of delivery
///
/// When a packet is sent that requests proof of delivery, a PacketReceipt
/// is created to track it. The receipt stores the packet hash and allows
/// validation of incoming proofs.
#[derive(Debug, Clone)]
pub struct PacketReceipt {
    /// Full SHA256 hash of the sent packet
    pub packet_hash: [u8; 32],
    /// Truncated hash (used as receipt ID for lookups)
    pub truncated_hash: [u8; TRUNCATED_HASHBYTES],
    /// Destination the packet was sent to
    pub destination_hash: DestinationHash,
    /// When the packet was sent (ms since epoch)
    pub sent_at_ms: u64,
    /// Current receipt status
    pub status: ReceiptStatus,
    /// Timeout duration in milliseconds
    pub timeout_ms: u64,
}

impl PacketReceipt {
    /// Create a new receipt for a sent packet
    pub fn new(packet_hash: [u8; 32], destination_hash: DestinationHash, sent_at_ms: u64) -> Self {
        // Take the first 16 bytes of the full hash directly, do NOT re-hash.
        // truncated_hash() would SHA256 the already-hashed input, producing
        // SHA256(SHA256(hashable))[0:16] instead of SHA256(hashable)[0:16].
        // Python uses packet.get_hash()[:16] which is a simple slice.
        let mut truncated = [0u8; TRUNCATED_HASHBYTES];
        truncated.copy_from_slice(&packet_hash[..TRUNCATED_HASHBYTES]);
        Self {
            packet_hash,
            truncated_hash: truncated,
            destination_hash,
            sent_at_ms,
            status: ReceiptStatus::Sent,
            timeout_ms: RECEIPT_TIMEOUT_DEFAULT_MS,
        }
    }

    /// Create a receipt with a custom timeout
    pub fn with_timeout(
        packet_hash: [u8; 32],
        destination_hash: DestinationHash,
        sent_at_ms: u64,
        timeout_ms: u64,
    ) -> Self {
        let mut truncated = [0u8; TRUNCATED_HASHBYTES];
        truncated.copy_from_slice(&packet_hash[..TRUNCATED_HASHBYTES]);
        Self {
            packet_hash,
            truncated_hash: truncated,
            destination_hash,
            sent_at_ms,
            status: ReceiptStatus::Sent,
            timeout_ms,
        }
    }

    /// Check if the receipt has timed out
    pub fn is_expired(&self, current_time_ms: u64) -> bool {
        current_time_ms.saturating_sub(self.sent_at_ms) > self.timeout_ms
    }

    /// Validate an incoming proof against this receipt
    pub fn validate_proof(&self, proof_data: &[u8], sender_identity: &Identity) -> bool {
        if proof_data.len() != PROOF_DATA_SIZE {
            return false;
        }

        sender_identity.verify_proof(proof_data, &self.packet_hash)
    }

    /// Mark this receipt as delivered
    pub fn set_delivered(&mut self) {
        self.status = ReceiptStatus::Delivered;
    }

    /// Mark this receipt as failed
    pub fn set_failed(&mut self) {
        self.status = ReceiptStatus::Failed;
    }

    /// Get the time elapsed since the packet was sent
    pub fn elapsed_ms(&self, current_time_ms: u64) -> u64 {
        current_time_ms.saturating_sub(self.sent_at_ms)
    }

    /// Get the remaining time before timeout
    pub fn remaining_ms(&self, current_time_ms: u64) -> u64 {
        let elapsed = self.elapsed_ms(current_time_ms);
        self.timeout_ms.saturating_sub(elapsed)
    }
}
