//! Point-to-point verified connections
//!
//! Links provide:
//! - Verified, encrypted communication channels
//! - Perfect forward secrecy via ephemeral keys
//! - Bidirectional data transfer
//! - Keepalive and timeout management
//!
//! ## Link Establishment
//!
//! Links can be established in two directions:
//!
//! ### Initiator (client) side:
//! 1. Create link with `Link::new_outgoing()`
//! 2. Send LINK_REQUEST packet via `build_link_request_packet()`
//! 3. Receive PROOF from destination
//! 4. Verify proof with `process_proof()`
//! 5. Send RTT packet via `build_rtt_packet()` to finalize
//!
//! ### Responder (server) side:
//! 1. Receive LINK_REQUEST packet
//! 2. Create link with `Link::new_incoming()`
//! 3. Send PROOF via `build_proof_packet()`
//! 4. Receive RTT packet
//! 5. Finalize with `process_rtt()`
//!
//! ## Channel System
//!
//! For reliable message delivery, use the [`channel`] module which provides:
//! - Automatic retries with exponential backoff
//! - Message ordering via 16-bit sequence numbers
//! - Flow control with adaptive window sizing
//!
//! ## Integration
//!
//! Link management is handled directly by [`NodeCore`](crate::node::NodeCore),
//! which owns all link state and handles packet processing, timeouts,
//! keepalives, and channel retransmissions.

pub mod channel;

use alloc::vec::Vec;

use crate::constants::{
    ED25519_SIGNATURE_SIZE, ESTABLISHMENT_RESPONDER_BONUS_MS, ESTABLISHMENT_TIMEOUT_PER_HOP_MS,
    KEEPALIVE_INITIATOR_BYTE, KEEPALIVE_PAYLOAD_SIZE, KEEPALIVE_RESPONDER_BYTE,
    LINK_KEEPALIVE_MAX_RTT, LINK_KEEPALIVE_MIN_SECS, LINK_KEEPALIVE_SECS,
    LINK_KEEPALIVE_TIMEOUT_FACTOR, LINK_STALE_FACTOR, LINK_STALE_GRACE_SECS, MTU, PROOF_DATA_SIZE,
    RTT_RETRY_INTERVAL_MULTIPLIER, RTT_RETRY_MAX_ATTEMPTS, RTT_RETRY_MIN_INTERVAL_MS,
    SIGNALING_MODE_MASK, SIGNALING_MODE_SHIFT, SIGNALING_MTU_MASK, TRUNCATED_HASHBYTES, US_PER_MS,
    X25519_KEY_SIZE,
};
use crate::crypto::{derive_key, truncated_hash};
use crate::destination::{DestinationHash, ProofStrategy};
use crate::identity::Identity;
use crate::packet::PacketContext;
use channel::Channel;
use rand_core::CryptoRngCore;

// Link request/proof size constants
/// Size of link request payload without signaling (X25519 pub + Ed25519 pub)
pub(crate) const LINK_REQUEST_BASE_SIZE: usize = 64;
/// Size of link request payload with MTU signaling
const LINK_REQUEST_SIGNALING_SIZE: usize = 67;
/// Size of signed data in proof (link_id + X25519 pub + Ed25519 pub + signaling)
const PROOF_SIGNED_DATA_SIZE: usize = 83; // 16 + 32 + 32 + 3
/// Size of link establishment proof data (signature + X25519 pub + signaling)
const LINK_PROOF_SIZE: usize = 99; // 64 + 32 + 3
/// Size of signaling bytes (21-bit MTU + 3-bit mode)
pub(crate) const SIGNALING_SIZE: usize = 3;

/// Encode MTU and mode into 3-byte signaling format
///
/// Format: 21-bit MTU (bits 0-20) + 3-bit mode (bits 21-23)
/// Returns the lower 3 bytes of the big-endian representation.
pub(crate) fn encode_signaling_bytes(mtu: u32, mode: u8) -> [u8; SIGNALING_SIZE] {
    let signaling =
        (mtu & SIGNALING_MTU_MASK) | ((mode as u32 & SIGNALING_MODE_MASK) << SIGNALING_MODE_SHIFT);
    let bytes = signaling.to_be_bytes();
    [bytes[1], bytes[2], bytes[3]]
}

/// Decode MTU and mode from 3-byte signaling format
///
/// Inverse of `encode_signaling_bytes()`. Extracts the 21-bit MTU and
/// 3-bit mode from the packed big-endian representation.
pub(crate) fn decode_signaling_bytes(bytes: &[u8; SIGNALING_SIZE]) -> (u32, u8) {
    let raw = (bytes[0] as u32) << 16 | (bytes[1] as u32) << 8 | bytes[2] as u32;
    let mtu = raw & SIGNALING_MTU_MASK;
    let mode = ((raw >> SIGNALING_MODE_SHIFT) & SIGNALING_MODE_MASK) as u8;
    (mtu, mode)
}

/// Validate that a signaled mode is supported.
///
/// Only `MODE_AES256_CBC` (0x01) is enabled in the Reticulum protocol.
/// Python's `Link.ENABLED_MODES = [MODE_AES256_CBC]` rejects all others.
fn validate_mode(mode: u8) -> Result<(), LinkError> {
    if mode == crate::constants::MODE_AES256_CBC {
        Ok(())
    } else {
        Err(LinkError::UnsupportedMode)
    }
}

/// Compute encrypted link MDU from a given link MTU.
///
/// Accounts for minimum header, IFAC, encryption token overhead,
/// and AES block alignment. Matches Python `Link.update_mdu()`:
/// ```python
/// mdu = floor((mtu - IFAC_MIN_SIZE - HEADER_MINSIZE - TOKEN_OVERHEAD)
///             / AES128_BLOCKSIZE) * AES128_BLOCKSIZE - 1
/// ```
fn compute_link_mdu(mtu: u32) -> usize {
    use crate::constants::{AES_BLOCK_SIZE, HEADER_MINSIZE, IFAC_MIN_SIZE, TOKEN_OVERHEAD};
    let mtu = mtu as usize;
    let usable = mtu
        .saturating_sub(IFAC_MIN_SIZE)
        .saturating_sub(HEADER_MINSIZE)
        .saturating_sub(TOKEN_OVERHEAD);
    (usable / AES_BLOCK_SIZE) * AES_BLOCK_SIZE - 1
}

/// Build the signed data for proof verification/generation
///
/// Format: [link_id (16)] [x25519_pub (32)] [ed25519_pub (32)] [signaling (3)]
fn build_proof_signed_data(
    link_id: &LinkId,
    x25519_pub: &[u8; X25519_KEY_SIZE],
    ed25519_pub: &[u8; X25519_KEY_SIZE],
    signaling: &[u8; SIGNALING_SIZE],
) -> [u8; PROOF_SIGNED_DATA_SIZE] {
    let mut signed_data = [0u8; PROOF_SIGNED_DATA_SIZE];
    signed_data[..TRUNCATED_HASHBYTES].copy_from_slice(link_id.as_bytes());
    signed_data[TRUNCATED_HASHBYTES..TRUNCATED_HASHBYTES + X25519_KEY_SIZE]
        .copy_from_slice(x25519_pub);
    signed_data[TRUNCATED_HASHBYTES + X25519_KEY_SIZE..TRUNCATED_HASHBYTES + 2 * X25519_KEY_SIZE]
        .copy_from_slice(ed25519_pub);
    signed_data[TRUNCATED_HASHBYTES + 2 * X25519_KEY_SIZE..].copy_from_slice(signaling);
    signed_data
}

/// Handshake phase, tracks which side we're on and when we started waiting.
/// Consumed by `check_timeouts()` to detect stalled handshakes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkPhase {
    /// Initiator: LINK_REQUEST sent, waiting for PROOF
    PendingOutgoing { created_at_ms: u64 },
    /// Responder: PROOF sent, waiting for RTT packet
    PendingIncoming { proof_sent_at_ms: u64 },
    /// Handshake complete (Active, Stale, or Closed)
    Established,
}

/// Link state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Link request sent, waiting for proof
    Pending,
    /// Handshake in progress
    Handshake,
    /// Link established and active
    Active,
    /// Link inactive for too long
    Stale,
    /// Link closed
    Closed,
}

/// Link establishment error
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// Invalid state for operation
    InvalidState,
    /// Invalid proof received
    InvalidProof,
    /// Key exchange failed
    KeyExchangeFailed,
    /// No destination
    NoDestination,
    /// Invalid link request data
    InvalidRequest,
    /// No identity to sign with
    NoIdentity,
    /// Invalid RTT packet
    InvalidRtt,
    /// Link not found
    NotFound,
    /// Unsupported link encryption mode signaled by the peer
    UnsupportedMode,
    /// Send path is occupied, try later (mirrors [`crate::link::channel::ChannelError::Busy`])
    Busy,
    /// Channel is pacing sends, retry at the given time (mirrors [`crate::link::channel::ChannelError::PacingDelay`])
    PacingDelay { ready_at_ms: u64 },
    /// Destination not registered on this node
    DestinationNotRegistered,
}

impl core::fmt::Display for LinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LinkError::InvalidState => write!(f, "invalid state for operation"),
            LinkError::InvalidProof => write!(f, "invalid proof received"),
            LinkError::KeyExchangeFailed => write!(f, "key exchange failed"),
            LinkError::NoDestination => write!(f, "no destination"),
            LinkError::InvalidRequest => write!(f, "invalid link request data"),
            LinkError::NoIdentity => write!(f, "no identity to sign with"),
            LinkError::InvalidRtt => write!(f, "invalid RTT packet"),
            LinkError::NotFound => write!(f, "link not found"),
            LinkError::UnsupportedMode => write!(f, "unsupported link encryption mode"),
            LinkError::Busy => write!(f, "busy"),
            LinkError::PacingDelay { ready_at_ms } => {
                write!(f, "pacing delay until {}ms", ready_at_ms)
            }
            LinkError::DestinationNotRegistered => {
                write!(f, "destination not registered on this node")
            }
        }
    }
}

/// Reason why a link was closed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkCloseReason {
    /// Normal close requested
    Normal,
    /// Link handshake did not complete within the timeout period.
    /// The peer may be unreachable or the path may be invalid.
    Timeout,
    /// Invalid proof received
    InvalidProof,
    /// Peer closed the link
    PeerClosed,
    /// Link became stale (no activity)
    Stale,
    /// Channel message delivery failed after maximum retries.
    /// The link was established and operational, but a message could not
    /// be delivered despite repeated retransmission attempts.
    ChannelExhausted,
    /// The peer identified (LINKIDENTIFY) as a blackholed identity and the
    /// link was torn down (Codeberg #88; Python Link.py:1021-1023).
    Blackholed,
}

/// Peer's public keys from a link request
#[derive(Debug, Clone)]
pub struct PeerKeys {
    /// Peer's ephemeral X25519 public key (32 bytes)
    pub x25519_public: [u8; 32],
    /// Peer's ephemeral Ed25519 verifying key (32 bytes)
    pub ed25519_verifying: [u8; 32],
}

/// A 16-byte link identifier (truncated hash of link request)
///
/// In the Reticulum protocol, a link ID is derived from the SHA-256 hash of
/// the raw LINK_REQUEST packet, truncated to 16 bytes. Packets addressed to
/// a link use this value in the destination_hash field.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkId([u8; TRUNCATED_HASHBYTES]);

impl LinkId {
    /// Create a LinkId from raw bytes
    pub const fn new(bytes: [u8; TRUNCATED_HASHBYTES]) -> Self {
        Self(bytes)
    }

    /// Get the underlying bytes
    pub const fn as_bytes(&self) -> &[u8; TRUNCATED_HASHBYTES] {
        &self.0
    }

    /// Convert to the raw byte array
    pub const fn into_bytes(self) -> [u8; TRUNCATED_HASHBYTES] {
        self.0
    }
}

impl From<[u8; TRUNCATED_HASHBYTES]> for LinkId {
    fn from(bytes: [u8; TRUNCATED_HASHBYTES]) -> Self {
        Self(bytes)
    }
}

impl From<LinkId> for [u8; TRUNCATED_HASHBYTES] {
    fn from(id: LinkId) -> Self {
        id.0
    }
}

impl AsRef<[u8]> for LinkId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8; TRUNCATED_HASHBYTES]> for LinkId {
    fn as_ref(&self) -> &[u8; TRUNCATED_HASHBYTES] {
        &self.0
    }
}

impl core::borrow::Borrow<[u8; TRUNCATED_HASHBYTES]> for LinkId {
    fn borrow(&self) -> &[u8; TRUNCATED_HASHBYTES] {
        &self.0
    }
}

impl PartialEq<[u8; TRUNCATED_HASHBYTES]> for LinkId {
    fn eq(&self, other: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        self.0 == *other
    }
}

impl PartialEq<LinkId> for [u8; TRUNCATED_HASHBYTES] {
    fn eq(&self, other: &LinkId) -> bool {
        *self == other.0
    }
}

impl core::fmt::Debug for LinkId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "LinkId(")?;
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        write!(f, ")")
    }
}

impl core::fmt::Display for LinkId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

/// A verified point-to-point link
pub struct Link {
    /// Link identifier
    id: LinkId,
    /// Current state
    state: LinkState,
    /// Our ephemeral X25519 private key
    ephemeral_private: Option<x25519_dalek::StaticSecret>,
    /// Our ephemeral X25519 public key
    ephemeral_public: x25519_dalek::PublicKey,
    /// Our ephemeral Ed25519 signing key (retained for future link-level authentication)
    signing_key: Option<ed25519_dalek::SigningKey>,
    /// Our ephemeral Ed25519 verifying key
    verifying_key: ed25519_dalek::VerifyingKey,
    /// Peer's ephemeral public key (after handshake)
    peer_ephemeral_public: Option<x25519_dalek::PublicKey>,
    /// Peer's verifying key (after handshake)
    peer_verifying_key: Option<ed25519_dalek::VerifyingKey>,
    /// Derived link encryption key (64 bytes: 32 encrypt + 32 HMAC)
    link_key: Option<[u8; 64]>,
    /// Destination hash this link connects to
    destination_hash: DestinationHash,
    /// Whether we initiated this link
    initiator: bool,
    /// Hop count to destination
    hops: u8,
    /// Handshake round-trip time in microseconds.
    ///
    /// Measured once during link establishment (from the RTT packet exchange).
    /// Used for keepalive interval calculation and window tier promotion.
    /// For the smoothed RTT used in pacing, see `Channel::srtt_ms`.
    rtt_us: Option<u64>,
    /// Keepalive interval in seconds (calculated from RTT)
    keepalive_secs: u64,
    /// Optional keepalive override in seconds. When set, the link uses this
    /// interval instead of the RTT-derived value and it survives RTT
    /// recalculation. Threaded from `TransportConfig::link_keepalive_secs`;
    /// `None` reproduces the default RTT-driven behaviour.
    keepalive_override: Option<u64>,
    /// Stale time in seconds (keepalive_secs * STALE_FACTOR)
    stale_time_secs: u64,
    /// Time of last inbound packet (timestamp in seconds)
    last_inbound: u64,
    /// Whether any inbound packet has been recorded. Distinguishes "no inbound
    /// yet" from a genuine `last_inbound` of 0 (a link established in the node's
    /// first uptime second, where the monotonic clock still reads 0), so the
    /// latter is not wrongly treated as never-active by stale detection.
    had_inbound: bool,
    /// Time of last outbound packet (timestamp in seconds)
    last_outbound: u64,
    /// Last time we sent a keepalive (timestamp in seconds)
    last_keepalive: u64,
    /// Count of keepalives sent proactively (initiator), since establishment.
    /// Diagnostic counter only; not used for any timeout decision.
    keepalives_sent: u64,
    /// Count of keepalive responses received (initiator), since establishment.
    /// Diagnostic counter only; not used for any timeout decision.
    keepalives_acked: u64,
    /// When the link was established (timestamp in seconds)
    established_at: Option<u64>,
    /// Proof strategy for received data on this link (responder only)
    proof_strategy: ProofStrategy,
    /// Destination identity signing key for generating proofs (responder only)
    dest_signing_key: Option<ed25519_dalek::SigningKey>,
    /// Interface this link is attached to (for routing outbound link traffic).
    ///
    /// Set from the receiving interface of the link request (responder) or
    /// the proof (initiator). Mirrors Python's `Link.attached_interface`.
    attached_interface: Option<usize>,
    /// Cumulative sends rejected by the attached interface's airtime/next-slot
    /// gate before reaching the channel (leviculum#35 backpressure telemetry).
    iface_pacing_rejections: u64,
    /// Cumulative bytes confirmed delivered via completed outgoing resource
    /// transfers on this link (the completion proof covers the whole transfer)
    /// — leviculum#35 delivery telemetry, complementing the channel's
    /// per-envelope `delivered_bytes`.
    resource_bytes_delivered: u64,
    /// Whether compression is enabled for this link
    compression_enabled: bool,
    /// Negotiated link MTU from signaling bytes (default: base protocol MTU).
    /// Set during link establishment from the peer's signaling bytes.
    /// Removed when link is closed.
    negotiated_mtu: u32,
    /// Handshake phase (pending outgoing, pending incoming, or established)
    phase: LinkPhase,
    /// Channel for multiplexed message delivery (created lazily on first send/receive)
    channel: Option<Channel>,
    /// Timestamp (ms) of last RTT packet send (initiator only).
    /// Cleared when rtt_confirmed becomes true (no further retries needed).
    rtt_sent_at_ms: Option<u64>,
    /// Number of RTT packets sent (initial + retries, initiator only).
    rtt_send_count: u8,
    /// True once any inbound link packet confirms the responder is alive.
    /// Stops RTT retry on the initiator side.
    rtt_confirmed: bool,
    /// Extra establishment timeout from first-hop interface latency (ms).
    /// Matches Python's `get_first_hop_timeout()`: MTU × 8 × 1000 / bitrate.
    /// Set at link creation from the next-hop interface bitrate.
    /// Zero for fast interfaces (TCP, etc).
    first_hop_timeout_extra_ms: u64,
    /// Outgoing resource transfer (sender side).
    /// Removed when transfer completes or fails.
    outgoing_resource: Option<crate::resource::outgoing::OutgoingResource>,
    /// Plan for the remaining segments of a split outgoing transfer (files
    /// whose combined metadata+data exceeds RESOURCE_MAX_EFFICIENT_SIZE).
    /// `None` for single-segment transfers. Cleared with the outgoing resource.
    outgoing_segments: Option<crate::resource::outgoing::OutgoingSegmentPlan>,
    /// Incoming resource transfer (receiver side).
    /// Removed when transfer completes or fails.
    incoming_resource: Option<crate::resource::incoming::IncomingResource>,
    /// Pending advertisement awaiting application accept/reject (AcceptApp strategy).
    /// Removed when accept_resource() or reject_resource() is called.
    pending_resource_adv: Option<crate::resource::ResourceAdvertisement>,
    /// Resource acceptance strategy for this link.
    resource_strategy: crate::resource::ResourceStrategy,
    /// Remote identity, set when the link peer identifies via LINKIDENTIFY.
    /// Only populated on the responder side (non-initiator).
    /// Removal: cleared when the link is dropped (Link owns this data).
    remote_identity: Option<Identity>,
    /// Cached proof packet bytes for re-sending on duplicate link requests (responder only).
    ///
    /// Set in `send_establishment_proof()` after `build_proof_packet()` returns.
    /// Read in `handle_link_request()` when a duplicate request arrives for a
    /// link still in `PendingIncoming` phase (retry after a lost proof).
    /// Cleared when the link transitions to Active (in `process_rtt()`).
    /// ~118 bytes per pending responder link. Dropped when the link is removed.
    cached_proof: Option<Vec<u8>>,
    /// Cached resource proof packets for re-send on CacheRequest.
    /// Key: packet_hash of the proof packet. Value: raw proof wire bytes.
    /// Same lifecycle class as `cached_proof` (link-scoped, bounded: at most
    /// one active resource per link direction). Lives on Link directly rather
    /// than behind Storage because it is tiny, temporary, and dies with the link.
    /// Cleanup: entries are replaced on each new resource proof (insert overwrites).
    /// All entries are dropped when the link is closed or dropped.
    cached_resource_proofs: alloc::collections::BTreeMap<[u8; 32], Vec<u8>>,
    /// Per-link random establishment-timeout jitter, in permille of the base
    /// timeout (Codeberg #129). Drawn at construction and re-rolled on every
    /// establishment retransmit so concurrent initiators de-sync instead of
    /// retrying in lockstep. `0..=MAX_ESTABLISHMENT_JITTER_PERMILLE`; purely
    /// local timing, never on the wire.
    establishment_jitter_permille: u16,
}

/// Draw a fresh establishment-timeout jitter value (Codeberg #129):
/// uniform in `0..=MAX_ESTABLISHMENT_JITTER_PERMILLE`.
fn draw_establishment_jitter(rng: &mut impl CryptoRngCore) -> u16 {
    (rng.next_u32() % (crate::constants::MAX_ESTABLISHMENT_JITTER_PERMILLE as u32 + 1)) as u16
}

impl Link {
    /// Create a new outgoing link (initiator side)
    ///
    /// # Arguments
    /// * `destination_hash` - The destination to connect to
    /// * `rng` - Random number generator for key generation
    pub fn new_outgoing(destination_hash: DestinationHash, rng: &mut impl CryptoRngCore) -> Self {
        let ephemeral_private = x25519_dalek::StaticSecret::random_from_rng(&mut *rng);
        let ephemeral_public = x25519_dalek::PublicKey::from(&ephemeral_private);

        // Generate Ed25519 key from random bytes
        let mut ed25519_seed = [0u8; 32];
        rng.fill_bytes(&mut ed25519_seed);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed25519_seed);
        let verifying_key = signing_key.verifying_key();

        // Temporary link ID (will be set properly after receiving proof)
        let id = LinkId::new([0u8; TRUNCATED_HASHBYTES]);

        let establishment_jitter_permille = draw_establishment_jitter(rng);

        Self {
            id,
            state: LinkState::Pending,
            ephemeral_private: Some(ephemeral_private),
            ephemeral_public,
            signing_key: Some(signing_key),
            verifying_key,
            peer_ephemeral_public: None,
            peer_verifying_key: None,
            link_key: None,
            destination_hash,
            initiator: true,
            hops: 0,
            rtt_us: None,
            keepalive_secs: LINK_KEEPALIVE_SECS,
            keepalive_override: None,
            stale_time_secs: LINK_KEEPALIVE_SECS * LINK_STALE_FACTOR,
            last_inbound: 0,
            had_inbound: false,
            last_outbound: 0,
            last_keepalive: 0,
            keepalives_sent: 0,
            keepalives_acked: 0,
            established_at: None,
            proof_strategy: ProofStrategy::None,
            dest_signing_key: None,
            attached_interface: None,
            iface_pacing_rejections: 0,
            resource_bytes_delivered: 0,
            compression_enabled: false,
            negotiated_mtu: MTU as u32,
            phase: LinkPhase::Established,
            channel: None,
            rtt_sent_at_ms: None,
            rtt_send_count: 0,
            rtt_confirmed: false,
            first_hop_timeout_extra_ms: 0,
            outgoing_resource: None,
            outgoing_segments: None,
            incoming_resource: None,
            pending_resource_adv: None,
            resource_strategy: crate::resource::ResourceStrategy::AcceptNone,
            remote_identity: None,
            cached_proof: None,
            cached_resource_proofs: alloc::collections::BTreeMap::new(),
            establishment_jitter_permille,
        }
    }

    /// Create a new incoming link (responder/server side) from a LINK_REQUEST packet.
    ///
    /// This is called when a destination receives a LINK_REQUEST packet.
    /// The destination's identity is used for signing the proof.
    ///
    /// # Arguments
    /// * `request_data` - The 64-byte payload from the LINK_REQUEST packet
    ///   (32 bytes X25519 pub + 32 bytes Ed25519 pub)
    /// * `link_id` - The pre-computed link ID (from `calculate_link_id()`)
    /// * `destination_hash` - The hash of the destination receiving this request
    /// * `rng` - Random number generator for key generation
    ///
    /// # Returns
    /// A new Link in Pending state, ready for `build_proof_packet()` to be called.
    pub fn new_incoming(
        request_data: &[u8],
        link_id: LinkId,
        destination_hash: DestinationHash,
        rng: &mut impl CryptoRngCore,
        hw_mtu: Option<u32>,
    ) -> Result<Self, LinkError> {
        // LINK_REQUEST payload: [peer_x25519_pub (32)] [peer_ed25519_pub (32)] [signaling (0-3)]
        if request_data.len() < LINK_REQUEST_BASE_SIZE {
            return Err(LinkError::InvalidRequest);
        }

        // Extract peer's public keys
        let peer_x25519_bytes: [u8; 32] = request_data[..32]
            .try_into()
            .map_err(|_| LinkError::InvalidRequest)?;
        let peer_ed25519_bytes: [u8; 32] = request_data[32..64]
            .try_into()
            .map_err(|_| LinkError::InvalidRequest)?;

        // Extract negotiated MTU and validate mode from signaling bytes (if present)
        let negotiated_mtu = if request_data.len() >= LINK_REQUEST_SIGNALING_SIZE {
            let sig_bytes: [u8; SIGNALING_SIZE] = request_data
                [LINK_REQUEST_BASE_SIZE..LINK_REQUEST_SIGNALING_SIZE]
                .try_into()
                .map_err(|_| LinkError::InvalidRequest)?;
            let (mtu, mode) = decode_signaling_bytes(&sig_bytes);
            validate_mode(mode)?;
            let path_mtu = if mtu >= MTU as u32 { mtu } else { MTU as u32 };
            // Clamp to receiving interface's HW_MTU (matches Python Transport.inbound)
            match hw_mtu {
                Some(iface_mtu) => path_mtu.min(iface_mtu),
                None => path_mtu,
            }
        } else {
            MTU as u32
        };

        let peer_ephemeral_public = x25519_dalek::PublicKey::from(peer_x25519_bytes);
        let peer_verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&peer_ed25519_bytes)
            .map_err(|_| LinkError::InvalidRequest)?;

        // Generate our ephemeral X25519 key pair
        let ephemeral_private = x25519_dalek::StaticSecret::random_from_rng(&mut *rng);
        let ephemeral_public = x25519_dalek::PublicKey::from(&ephemeral_private);

        // Generate Ed25519 key for future link authentication (not used for proof signing)
        let mut ed25519_seed = [0u8; 32];
        rng.fill_bytes(&mut ed25519_seed);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed25519_seed);
        let verifying_key = signing_key.verifying_key();

        let establishment_jitter_permille = draw_establishment_jitter(rng);

        Ok(Self {
            id: link_id,
            state: LinkState::Pending,
            ephemeral_private: Some(ephemeral_private),
            ephemeral_public,
            signing_key: Some(signing_key),
            verifying_key,
            peer_ephemeral_public: Some(peer_ephemeral_public),
            peer_verifying_key: Some(peer_verifying_key),
            link_key: None,
            destination_hash,
            initiator: false, // We are the responder
            hops: 0,
            rtt_us: None,
            keepalive_secs: LINK_KEEPALIVE_SECS,
            keepalive_override: None,
            stale_time_secs: LINK_KEEPALIVE_SECS * LINK_STALE_FACTOR,
            last_inbound: 0,
            had_inbound: false,
            last_outbound: 0,
            last_keepalive: 0,
            keepalives_sent: 0,
            keepalives_acked: 0,
            established_at: None,
            proof_strategy: ProofStrategy::None,
            dest_signing_key: None,
            attached_interface: None,
            iface_pacing_rejections: 0,
            resource_bytes_delivered: 0,
            compression_enabled: false,
            negotiated_mtu,
            phase: LinkPhase::Established,
            channel: None,
            rtt_sent_at_ms: None,
            rtt_send_count: 0,
            rtt_confirmed: false,
            first_hop_timeout_extra_ms: 0,
            outgoing_resource: None,
            outgoing_segments: None,
            incoming_resource: None,
            pending_resource_adv: None,
            resource_strategy: crate::resource::ResourceStrategy::AcceptNone,
            remote_identity: None,
            cached_proof: None,
            cached_resource_proofs: alloc::collections::BTreeMap::new(),
            establishment_jitter_permille,
        })
    }

    /// Build the PROOF packet for responding to a LINK_REQUEST.
    ///
    /// This performs the DH key exchange and derives the link key.
    /// Must be called on an incoming link in Pending state.
    ///
    /// # Arguments
    /// * `identity` - The destination's identity (for signing the proof)
    /// * `mtu` - The MTU value for signaling (typically 500)
    /// * `mode` - The mode for signaling (typically 1 for AES-256-CBC)
    ///
    /// # Returns
    /// The raw PROOF packet bytes ready for framing/transmission.
    /// Link state transitions to Handshake.
    pub fn build_proof_packet(
        &mut self,
        identity: &Identity,
        mtu: u32,
        mode: u8,
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketContext, PacketFlags, PacketType, TransportType};

        self.require_state(LinkState::Pending)?;
        self.require_responder()?;

        // Get peer's public key (from the LINK_REQUEST)
        let peer_ephemeral_public = self
            .peer_ephemeral_public
            .ok_or(LinkError::KeyExchangeFailed)?;

        // Perform X25519 key exchange
        let ephemeral_private = self
            .ephemeral_private
            .as_ref()
            .ok_or(LinkError::KeyExchangeFailed)?;
        let shared_secret = ephemeral_private.diffie_hellman(&peer_ephemeral_public);

        // Derive link key using HKDF
        let link_key = Self::derive_link_key(shared_secret.as_bytes(), &self.id);
        self.link_key = Some(link_key);

        // Build signaling bytes: 21-bit MTU + 3-bit mode
        let signaling_bytes = encode_signaling_bytes(mtu, mode);

        // Build signed data: link_id (16) + our_x25519_pub (32) + dest_ed25519_pub (32) + signaling (3)
        // Python RNS uses the destination's Ed25519 signing key (not the link's ephemeral one)
        let signed_data = build_proof_signed_data(
            &self.id,
            self.ephemeral_public.as_bytes(),
            &identity.ed25519_verifying().to_bytes(),
            &signaling_bytes,
        );

        // Sign with destination's identity
        let signature = identity
            .sign(&signed_data)
            .map_err(|_| LinkError::NoIdentity)?;

        // Build proof data: [signature (64)] [our_x25519_pub (32)] [signaling (3)]
        let mut proof_data = alloc::vec![0u8; LINK_PROOF_SIZE];
        proof_data[..ED25519_SIGNATURE_SIZE].copy_from_slice(&signature);
        proof_data[ED25519_SIGNATURE_SIZE..ED25519_SIGNATURE_SIZE + X25519_KEY_SIZE]
            .copy_from_slice(self.ephemeral_public.as_bytes());
        proof_data[ED25519_SIGNATURE_SIZE + X25519_KEY_SIZE..].copy_from_slice(&signaling_bytes);

        // Build packet: PROOF packet addressed to the link_id
        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Link,
            packet_type: PacketType::Proof,
        };

        let mut packet =
            alloc::vec::Vec::with_capacity(1 + 1 + TRUNCATED_HASHBYTES + 1 + LINK_PROOF_SIZE);
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.id.as_bytes()); // destination = link_id
        packet.push(PacketContext::Lrproof.to_byte());
        packet.extend_from_slice(&proof_data);

        // Transition to Handshake state (waiting for RTT from initiator)
        self.state = LinkState::Handshake;

        Ok(packet)
    }

    /// Process the RTT packet from the initiator to finalize link establishment.
    ///
    /// This is called on the responder side after sending the PROOF.
    /// The RTT packet contains an encrypted msgpack float64 value.
    ///
    /// # Arguments
    /// * `encrypted_data` - The encrypted payload from the RTT packet
    ///
    /// # Returns
    /// Ok(rtt_seconds) if the link is now active, Err otherwise.
    pub fn process_rtt(&mut self, encrypted_data: &[u8]) -> Result<f64, LinkError> {
        self.require_state(LinkState::Handshake)?;
        self.require_responder()?;

        // Decrypt the RTT data
        let mut plaintext = alloc::vec![0u8; encrypted_data.len()];
        let plaintext_len = self.decrypt(encrypted_data, &mut plaintext)?;

        // RTT is msgpack-encoded float64: 0xCB + 8 bytes big-endian IEEE 754
        if plaintext_len < 9 || plaintext[0] != 0xCB {
            return Err(LinkError::InvalidRtt);
        }

        let rtt_bytes: [u8; 8] = plaintext[1..9]
            .try_into()
            .map_err(|_| LinkError::InvalidRtt)?;
        let rtt_seconds = f64::from_be_bytes(rtt_bytes);

        // The RTT float is HMAC-authenticated but still peer-supplied. Reject a
        // non-finite or non-positive value and clamp an implausibly large one:
        // NaN collapses the RTT-derived keepalive/stale timers to 0 (the link is
        // then marked Stale and closed within seconds of establishment), and
        // +inf pushes the stale-close timeout to effectively never (the Stale
        // link entry is pinned in the table forever). A real peer sends a small
        // positive measurement.
        if !rtt_seconds.is_finite() || rtt_seconds <= 0.0 {
            return Err(LinkError::InvalidRtt);
        }
        const MAX_PLAUSIBLE_RTT_SECS: f64 = 3600.0;
        let rtt_seconds = rtt_seconds.min(MAX_PLAUSIBLE_RTT_SECS);

        // Store RTT (convert to microseconds for internal storage)
        self.rtt_us = Some((rtt_seconds * 1_000_000.0) as u64);
        crate::tracing::debug!(
            rtt_ms = (rtt_seconds * 1000.0) as u64,
            "link: handshake RTT measured"
        );

        // Link is now active, clear cached proof (no longer needed for re-send)
        self.state = LinkState::Active;
        self.cached_proof = None;

        Ok(rtt_seconds)
    }

    /// Get the link ID
    pub fn id(&self) -> &LinkId {
        &self.id
    }

    /// Get the current state
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// Get the destination hash
    pub fn destination_hash(&self) -> &DestinationHash {
        &self.destination_hash
    }

    /// Check if we initiated this link
    pub fn is_initiator(&self) -> bool {
        self.initiator
    }

    /// Get the proof strategy for received data on this link
    pub fn proof_strategy(&self) -> ProofStrategy {
        self.proof_strategy
    }

    /// Set the proof strategy for received data on this link
    pub fn set_proof_strategy(&mut self, strategy: ProofStrategy) {
        self.proof_strategy = strategy;
    }

    /// Get the destination identity signing key (for proof generation)
    pub fn dest_signing_key(&self) -> Option<&ed25519_dalek::SigningKey> {
        self.dest_signing_key.as_ref()
    }

    /// Set the destination identity signing key (for proof generation)
    pub fn set_dest_signing_key(&mut self, key: ed25519_dalek::SigningKey) {
        self.dest_signing_key = Some(key);
    }

    /// Get the signing key for data/channel proof generation.
    ///
    /// Returns `dest_signing_key` (the destination identity key) for the
    /// responder, or the link's own ephemeral `signing_key` for the initiator.
    /// Matches Python's `Link.sig_prv` which is identity-based for responders
    /// and ephemeral for initiators.
    pub fn proof_signing_key(&self) -> Option<&ed25519_dalek::SigningKey> {
        self.dest_signing_key.as_ref().or(self.signing_key.as_ref())
    }

    /// Get the interface this link is attached to
    ///
    /// An interface, not a peer: every packet an established link sends
    /// (`NodeCore::route_link_packet`, `send_establishment_proof`,
    /// channel data) goes out with no #376 delivery hint, so on a BLE
    /// carrier it reaches every live link, not just the neighbour the
    /// link is with. The peer IS knowable — the LinkRequest arrives
    /// through `Transport::process_incoming_from_peer` — but it would
    /// have to travel on the link-establishment transport events to
    /// reach here, next to the `interface_index` these two setters take.
    /// That plumbing is the next #376 step, not part of the deferred
    /// proof this one carried.
    pub fn attached_interface(&self) -> Option<usize> {
        self.attached_interface
    }

    /// Set the interface this link is attached to
    pub fn set_attached_interface(&mut self, iface: usize) {
        self.attached_interface = Some(iface);
    }

    /// Record a send rejected by the attached interface's airtime/next-slot
    /// gate (leviculum#35 backpressure telemetry).
    pub(crate) fn note_iface_pacing_rejection(&mut self) {
        self.iface_pacing_rejections = self.iface_pacing_rejections.saturating_add(1);
    }

    /// Cumulative interface-gate send rejections (leviculum#35).
    pub fn iface_pacing_rejections(&self) -> u64 {
        self.iface_pacing_rejections
    }

    /// Credit a completed outgoing resource transfer's bytes as delivered —
    /// the completion proof confirms the peer reassembled the whole transfer
    /// (leviculum#35 delivery telemetry).
    pub(crate) fn add_resource_bytes_delivered(&mut self, bytes: u64) {
        self.resource_bytes_delivered = self.resource_bytes_delivered.saturating_add(bytes);
    }

    /// Cumulative bytes delivered via completed outgoing resources (leviculum#35).
    pub fn resource_bytes_delivered(&self) -> u64 {
        self.resource_bytes_delivered
    }

    /// Get the cached proof bytes (for re-sending on duplicate link requests)
    pub fn cached_proof(&self) -> Option<&[u8]> {
        self.cached_proof.as_deref()
    }

    /// Store proof bytes for potential re-send on duplicate link requests.
    ///
    /// Called in `send_establishment_proof()` after `build_proof_packet()`. Cleared
    /// when the link transitions to Active in `process_rtt()`.
    pub fn set_cached_proof(&mut self, proof: Vec<u8>) {
        self.cached_proof = Some(proof);
    }

    /// Cache a resource proof packet for re-send on CacheRequest.
    pub(crate) fn cache_resource_proof(&mut self, packet_hash: [u8; 32], raw: Vec<u8>) {
        self.cached_resource_proofs.insert(packet_hash, raw);
    }

    /// Look up a cached resource proof by packet_hash.
    pub(crate) fn get_cached_resource_proof(&self, packet_hash: &[u8; 32]) -> Option<&Vec<u8>> {
        self.cached_resource_proofs.get(packet_hash)
    }

    /// Check if compression is enabled for this link
    pub fn compression_enabled(&self) -> bool {
        self.compression_enabled
    }

    /// Enable or disable compression for this link
    pub fn set_compression(&mut self, enabled: bool) {
        self.compression_enabled = enabled;
    }

    /// Get the remote identity if the peer has identified on this link.
    ///
    /// `pub` because `NodeCore::get_remote_identity()` delegates here, and
    /// `leviculum-std` driver exposes it to async callers.
    pub fn remote_identity(&self) -> Option<&Identity> {
        self.remote_identity.as_ref()
    }

    /// Store the validated remote identity (called by identify handler).
    pub(crate) fn set_remote_identity(&mut self, identity: Identity) {
        self.remote_identity = Some(identity);
    }

    /// Get the hop count
    pub fn hops(&self) -> u8 {
        self.hops
    }

    /// Set the hop count (from path table or incoming packet)
    pub fn set_hops(&mut self, hops: u8) {
        self.hops = hops;
    }

    /// Compute the establishment timeout for this link, matching Python's formula.
    ///
    /// Initiator (PendingOutgoing):
    ///   `first_hop_timeout + ESTABLISHMENT_TIMEOUT_PER_HOP × max(1, hops)`
    ///   where first_hop_timeout = MTU × 8 × 1000 / bitrate + per_hop_timeout
    ///   (falls back to per_hop_timeout when bitrate is unknown).
    ///
    /// Responder (PendingIncoming):
    ///   `ESTABLISHMENT_TIMEOUT_PER_HOP × max(1, hops) + KEEPALIVE`
    ///   The KEEPALIVE term gives the RTT packet time to travel back.
    ///
    /// Both branches then add the per-link random jitter (Codeberg #129):
    /// `base + base × jitter_permille / 1000`, additive only, so concurrent
    /// initiators de-sync instead of retransmitting in lockstep. The jitter
    /// field is set once (re-rolled only on retransmit), so this stays a
    /// pure getter returning a stable value between rolls.
    pub fn establishment_timeout_ms(&self) -> u64 {
        let hops = core::cmp::max(1, self.hops as u64);
        let base = if self.initiator {
            // Python: first_hop_timeout(dest) + ESTABLISHMENT_TIMEOUT_PER_HOP * max(1, hops)
            // first_hop_timeout = MTU * per_byte_latency + DEFAULT_PER_HOP_TIMEOUT
            // We split it: first_hop_timeout_extra_ms carries the MTU*latency part,
            // and the base per_hop_timeout is folded into the hops term.
            self.first_hop_timeout_extra_ms + ESTABLISHMENT_TIMEOUT_PER_HOP_MS * (hops + 1)
        } else {
            ESTABLISHMENT_TIMEOUT_PER_HOP_MS * hops + ESTABLISHMENT_RESPONDER_BONUS_MS
        };
        base + base * self.establishment_jitter_permille as u64 / 1000
    }

    /// Re-draw the establishment jitter (Codeberg #129). Called on every
    /// establishment retransmit so each retry attempt de-syncs afresh —
    /// with a single per-link roll, two links whose draws happened to land
    /// close together would stay close on every retry.
    pub(crate) fn reroll_establishment_jitter(&mut self, rng: &mut impl CryptoRngCore) {
        self.establishment_jitter_permille = draw_establishment_jitter(rng);
    }

    /// Force a specific jitter permille. Test hook: exact-value timeout
    /// tests pin it to 0 to assert the Python-parity base formula.
    #[cfg(test)]
    pub(crate) fn set_establishment_jitter_permille(&mut self, permille: u16) {
        self.establishment_jitter_permille = permille;
    }

    /// Set the first-hop timeout extra from interface bitrate.
    /// Computes MTU × 8 × 1000 / bitrate_bps (milliseconds).
    pub(crate) fn set_first_hop_timeout_from_bitrate(&mut self, bitrate_bps: u32) {
        if bitrate_bps > 0 {
            self.first_hop_timeout_extra_ms = (MTU as u64) * 8 * 1000 / (bitrate_bps as u64);
        }
    }

    /// Store a handshake RTT measurement (milliseconds → microseconds).
    ///
    /// Called on the **initiator** side after computing RTT in
    /// `handle_link_proof()`, so that `rtt_secs()` returns a meaningful
    /// value for RTT-retry packets instead of `None`.
    pub(crate) fn set_rtt_ms(&mut self, rtt_ms: u64) {
        self.rtt_us = Some(rtt_ms.saturating_mul(US_PER_MS));
    }

    /// Get the RTT estimate in microseconds
    pub fn rtt_us(&self) -> Option<u64> {
        self.rtt_us
    }

    /// Get the RTT estimate in seconds
    pub fn rtt_secs(&self) -> Option<f64> {
        self.rtt_us.map(|us| us as f64 / 1_000_000.0)
    }

    /// Get the handshake RTT in milliseconds with default fallback
    ///
    /// Returns the one-time handshake RTT measurement, or the default channel
    /// RTT if no measurement is available yet. This is a conservative estimate
    /// used for keepalive timing and window tier decisions.
    ///
    /// For the continuously-updated smoothed RTT used in pacing, see
    /// `Channel::srtt_ms`.
    pub fn rtt_ms(&self) -> u64 {
        use crate::constants::{CHANNEL_DEFAULT_RTT_MS, US_PER_MS};
        self.rtt_us
            .map(|us| (us / US_PER_MS).max(1))
            .unwrap_or(CHANNEL_DEFAULT_RTT_MS)
    }

    /// Get the maximum data unit for this link
    ///
    /// Returns the encrypted link MDU calculated from the negotiated MTU.
    /// Accounts for minimum header, IFAC, encryption token overhead,
    /// and AES block alignment. Matches Python `Link.update_mdu()`.
    ///
    /// With the default MTU (500), this returns 431. For larger negotiated
    /// MTUs (e.g., TCP with HW_MTU=262144), returns proportionally larger
    /// values. Note: `constants::MDU` (464) is the unencrypted packet MDU;
    /// this method returns the encrypted link MDU which is always smaller.
    pub fn mdu(&self) -> usize {
        compute_link_mdu(self.negotiated_mtu)
    }

    /// Get the negotiated link MTU
    ///
    /// Returns the MTU established during link handshake via signaling bytes.
    /// Defaults to `MTU` (500) if no larger MTU was negotiated.
    pub fn negotiated_mtu(&self) -> u32 {
        self.negotiated_mtu
    }

    /// Get our ephemeral public key bytes for the link request
    pub fn ephemeral_public_bytes(&self) -> [u8; X25519_KEY_SIZE] {
        *self.ephemeral_public.as_bytes()
    }

    /// Get our signing verifying key bytes for the link request
    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.verifying_key.to_bytes()
    }

    /// Create the link request data without MTU signaling
    /// Format: [ephemeral_pub (32)] [sig_pub (32)]
    pub fn create_link_request(&self) -> [u8; LINK_REQUEST_BASE_SIZE] {
        let mut data = [0u8; LINK_REQUEST_BASE_SIZE];
        data[..X25519_KEY_SIZE].copy_from_slice(self.ephemeral_public.as_bytes());
        data[X25519_KEY_SIZE..].copy_from_slice(&self.verifying_key.to_bytes());
        data
    }

    /// Create the link request data with MTU signaling
    /// Format: [ephemeral_pub (32)] [sig_pub (32)] [mtu_signaling (3)]
    pub fn create_link_request_with_mtu(
        &self,
        mtu: u32,
        mode: u8,
    ) -> [u8; LINK_REQUEST_SIGNALING_SIZE] {
        let mut data = [0u8; LINK_REQUEST_SIGNALING_SIZE];
        data[..X25519_KEY_SIZE].copy_from_slice(self.ephemeral_public.as_bytes());
        data[X25519_KEY_SIZE..LINK_REQUEST_BASE_SIZE]
            .copy_from_slice(&self.verifying_key.to_bytes());
        // MTU signaling: 21-bit MTU + 3-bit mode
        let signaling_bytes = encode_signaling_bytes(mtu, mode);
        data[LINK_REQUEST_BASE_SIZE..].copy_from_slice(&signaling_bytes);
        data
    }

    /// Check if the link is active
    pub fn is_active(&self) -> bool {
        self.state == LinkState::Active
    }

    /// Verify a signature from the peer using their Ed25519 verifying key
    ///
    /// This is used to verify data packet proofs sent by the peer.
    /// Returns false if no peer verifying key is available (link not established)
    /// or if the signature is invalid.
    ///
    /// # Arguments
    /// * `message` - The message that was signed (e.g., packet hash)
    /// * `signature` - The Ed25519 signature (64 bytes)
    pub fn verify_peer_signature(&self, message: &[u8], signature: &[u8]) -> bool {
        use ed25519_dalek::Verifier;

        let Some(peer_key) = &self.peer_verifying_key else {
            return false;
        };

        if signature.len() != ED25519_SIGNATURE_SIZE {
            return false;
        }

        let Ok(sig_bytes): Result<[u8; ED25519_SIGNATURE_SIZE], _> = signature.try_into() else {
            return false;
        };

        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        peer_key.verify(message, &sig).is_ok()
    }

    /// Validate a link proof (PROVE_ALL format)
    ///
    /// This matches Python Reticulum's `validate_data_proof` behavior:
    /// 1. Check proof length (PROOF_DATA_SIZE bytes: hash + signature)
    /// 2. Verify hash in proof matches expected packet hash
    /// 3. Verify signature over the hash
    ///
    /// **Note:** Proof validation is directional. The initiator can validate proofs
    /// from the responder (destination owner) because `peer_verifying_key` is set to
    /// the destination's identity key. The reverse direction is not supported, the
    /// responder's `peer_verifying_key` is the initiator's ephemeral key, not their
    /// identity key.
    ///
    /// # Arguments
    /// * `proof_data` - The proof data (PROOF_DATA_SIZE bytes: packet_hash + signature)
    /// * `expected_hash` - The expected packet hash (32 bytes)
    ///
    /// # Returns
    /// `true` if hash matches and signature is valid
    pub fn validate_data_proof(&self, proof_data: &[u8], expected_hash: &[u8; 32]) -> bool {
        if proof_data.len() != PROOF_DATA_SIZE {
            return false;
        }

        // Extract hash from proof
        let proof_hash = &proof_data[..32];

        // Compare hash to expected
        if proof_hash != expected_hash {
            return false;
        }

        // Verify signature over the hash
        let signature = &proof_data[32..];
        self.verify_peer_signature(proof_hash, signature)
    }

    /// Create a proof for a received data packet using an Ed25519 signing key
    ///
    /// This variant takes a raw signing key instead of a full Identity,
    /// which is useful when the signing key has been extracted and stored
    /// separately (e.g., in NodeCore's destination registry).
    ///
    /// # Arguments
    /// * `packet_hash` - The hash of the received packet
    /// * `signing_key` - The Ed25519 signing key to sign with
    ///
    /// # Returns
    /// The proof data (PROOF_DATA_SIZE bytes)
    pub fn create_data_proof_with_signing_key(
        &self,
        packet_hash: &[u8; 32],
        signing_key: &ed25519_dalek::SigningKey,
    ) -> [u8; PROOF_DATA_SIZE] {
        use ed25519_dalek::Signer;

        let signature = signing_key.sign(packet_hash);

        let mut proof = [0u8; PROOF_DATA_SIZE];
        proof[..32].copy_from_slice(packet_hash);
        proof[32..].copy_from_slice(&signature.to_bytes());

        proof
    }

    /// Check if the link should be considered stale (no inbound for stale_time)
    ///
    /// A link becomes stale when no packets have been received for stale_time_secs,
    /// which is calculated as keepalive_secs * LINK_STALE_FACTOR.
    pub fn is_stale(&self, current_time_secs: u64) -> bool {
        if self.state != LinkState::Active {
            return false;
        }
        if !self.had_inbound {
            return false; // No inbound recorded yet
        }
        let elapsed = current_time_secs.saturating_sub(self.last_inbound);
        elapsed > self.stale_time_secs
    }

    /// Check if a stale link should be closed (timeout expired)
    ///
    /// After a link becomes stale, it should be closed after:
    /// RTT * LINK_KEEPALIVE_TIMEOUT_FACTOR + LINK_STALE_GRACE_SECS
    pub fn should_close(&self, current_time_secs: u64) -> bool {
        if self.state != LinkState::Stale {
            return false;
        }
        if !self.had_inbound {
            return false;
        }
        let elapsed = current_time_secs.saturating_sub(self.last_inbound);
        elapsed > self.stale_close_timeout_secs()
    }

    /// Total inactivity (since last inbound) after which a stale link is closed.
    ///
    /// `stale_time + RTT * LINK_KEEPALIVE_TIMEOUT_FACTOR + LINK_STALE_GRACE_SECS`,
    /// using this link's measured RTT. Pure; shared by `should_close()` and the
    /// `LINK_DIED`/`LINK_ESTAB` diagnostics.
    pub fn stale_close_timeout_secs(&self) -> u64 {
        let rtt_secs = self.rtt_secs().unwrap_or(0.0);
        self.stale_time_secs
            + (rtt_secs * LINK_KEEPALIVE_TIMEOUT_FACTOR as f64) as u64
            + LINK_STALE_GRACE_SECS
    }

    /// Calculate keepalive interval from RTT (matching Python formula)
    ///
    /// Python formula: max(min(rtt * (KEEPALIVE_MAX / KEEPALIVE_MAX_RTT), KEEPALIVE_MAX), KEEPALIVE_MIN)
    /// Where KEEPALIVE_MAX=360, KEEPALIVE_MAX_RTT=1.75, KEEPALIVE_MIN=5
    pub fn calculate_keepalive_from_rtt(rtt_secs: f64) -> u64 {
        let interval = rtt_secs * (LINK_KEEPALIVE_SECS as f64 / LINK_KEEPALIVE_MAX_RTT);
        let clamped = interval.clamp(LINK_KEEPALIVE_MIN_SECS as f64, LINK_KEEPALIVE_SECS as f64);
        clamped as u64
    }

    /// Update keepalive timing after RTT is known.
    ///
    /// A keepalive override (if set) wins over the RTT-derived value, so a
    /// configured short keepalive survives link establishment.
    pub fn update_keepalive_from_rtt(&mut self, rtt_secs: f64) {
        if self.keepalive_override.is_some() {
            self.apply_keepalive_override();
            return;
        }
        self.keepalive_secs = Self::calculate_keepalive_from_rtt(rtt_secs);
        self.stale_time_secs = self.keepalive_secs * LINK_STALE_FACTOR;
    }

    /// Set or clear the keepalive override (seconds).
    ///
    /// When set, the link uses this interval (clamped to
    /// `LINK_KEEPALIVE_MIN_SECS`) instead of the RTT-derived value, and the
    /// override survives RTT recalculation. `None` restores RTT-driven timing.
    pub fn set_keepalive_override(&mut self, secs: Option<u64>) {
        self.keepalive_override = secs;
        if secs.is_some() {
            self.apply_keepalive_override();
        }
    }

    /// Apply the current keepalive override to the live timing fields.
    fn apply_keepalive_override(&mut self) {
        if let Some(secs) = self.keepalive_override {
            let clamped = secs.max(LINK_KEEPALIVE_MIN_SECS);
            self.keepalive_secs = clamped;
            self.stale_time_secs = clamped * LINK_STALE_FACTOR;
        }
    }

    // RTT retry state (initiator only)
    /// Timestamp of the last RTT packet send (milliseconds)
    pub(crate) fn rtt_sent_at_ms(&self) -> Option<u64> {
        self.rtt_sent_at_ms
    }

    /// Number of RTT packets sent (initial + retries)
    pub(crate) fn rtt_send_count(&self) -> u8 {
        self.rtt_send_count
    }

    /// Whether an inbound packet has confirmed RTT delivery
    pub(crate) fn rtt_confirmed(&self) -> bool {
        self.rtt_confirmed
    }

    /// Retry interval for RTT packets: `max(rtt_ms * 3, 10s)`
    pub(crate) fn rtt_retry_interval_ms(&self) -> u64 {
        core::cmp::max(
            self.rtt_ms() * RTT_RETRY_INTERVAL_MULTIPLIER,
            RTT_RETRY_MIN_INTERVAL_MS,
        )
    }

    /// Whether this link is eligible for an RTT retry (initiator, active,
    /// unconfirmed, has sent at least once, hasn't exhausted retries).
    pub(crate) fn needs_rtt_retry(&self) -> bool {
        self.initiator
            && self.state() == LinkState::Active
            && !self.rtt_confirmed
            && self.rtt_send_count > 0
            && self.rtt_send_count <= RTT_RETRY_MAX_ATTEMPTS
    }

    /// Record that an RTT packet was sent (initial or retry)
    pub(crate) fn record_rtt_sent(&mut self, now_ms: u64) {
        self.rtt_sent_at_ms = Some(now_ms);
        self.rtt_send_count += 1;
    }

    /// Mark RTT delivery as confirmed (any inbound link packet or proof).
    /// Clears `rtt_sent_at_ms` since no further retries are needed.
    pub(crate) fn confirm_rtt(&mut self) {
        self.rtt_confirmed = true;
        self.rtt_sent_at_ms = None;
    }

    /// Check if we should send a keepalive now (initiator only)
    ///
    /// Returns true if enough time has passed since our last keepalive
    /// and we are the initiator (only initiators send keepalives proactively).
    pub fn should_send_keepalive(&self, now_secs: u64) -> bool {
        if !self.initiator || self.state != LinkState::Active {
            return false;
        }
        if self.last_keepalive == 0 {
            // First keepalive after establishment
            if let Some(established) = self.established_at {
                return now_secs.saturating_sub(established) >= self.keepalive_secs;
            }
            return false;
        }
        now_secs.saturating_sub(self.last_keepalive) >= self.keepalive_secs
    }

    /// Record an inbound packet timestamp
    pub fn record_inbound(&mut self, now_secs: u64) {
        self.last_inbound = now_secs;
        self.had_inbound = true;
    }

    /// Record an outbound packet timestamp
    pub fn record_outbound(&mut self, now_secs: u64) {
        self.last_outbound = now_secs;
    }

    /// Record that we sent a keepalive
    pub fn record_keepalive_sent(&mut self, now_secs: u64) {
        self.last_keepalive = now_secs;
        self.keepalives_sent = self.keepalives_sent.saturating_add(1);
    }

    /// Number of proactive keepalives sent since establishment (diagnostic).
    pub fn keepalives_sent(&self) -> u64 {
        self.keepalives_sent
    }

    /// Number of keepalive responses received since establishment (diagnostic).
    pub fn keepalives_acked(&self) -> u64 {
        self.keepalives_acked
    }

    /// Mark the link as established
    pub fn mark_established(&mut self, now_secs: u64) {
        self.established_at = Some(now_secs);
        self.last_inbound = now_secs;
        self.had_inbound = true;
    }

    /// Get the keepalive interval in seconds
    pub fn keepalive_secs(&self) -> u64 {
        self.keepalive_secs
    }

    /// Get the stale time threshold in seconds
    pub fn stale_time_secs(&self) -> u64 {
        self.stale_time_secs
    }

    /// Get the last keepalive sent timestamp in seconds
    pub fn last_keepalive_secs(&self) -> u64 {
        self.last_keepalive
    }

    /// Get the established-at timestamp in seconds (if established)
    pub fn established_at_secs(&self) -> Option<u64> {
        self.established_at
    }

    /// Get the last inbound packet timestamp in seconds
    pub fn last_inbound_secs(&self) -> u64 {
        self.last_inbound
    }

    /// Set the link state
    pub fn set_state(&mut self, state: LinkState) {
        self.state = state;
    }

    /// Get the current handshake phase
    pub(crate) fn phase(&self) -> LinkPhase {
        self.phase
    }

    /// Set the handshake phase
    pub(crate) fn set_phase(&mut self, phase: LinkPhase) {
        self.phase = phase;
    }

    /// Get an immutable reference to the channel (if created)
    pub(crate) fn channel(&self) -> Option<&Channel> {
        self.channel.as_ref()
    }

    /// Get a mutable reference to the channel (if created)
    pub(crate) fn channel_mut(&mut self) -> Option<&mut Channel> {
        self.channel.as_mut()
    }

    /// Check if a channel has been created for this link
    pub(crate) fn has_channel(&self) -> bool {
        self.channel.is_some()
    }

    /// Get or create the channel, initializing window from the given RTT.
    ///
    /// The caller must extract `rtt_ms` from `self.rtt_ms()` BEFORE calling
    /// this, to avoid a `&self` borrow conflicting with the `&mut self.channel` write.
    pub(crate) fn ensure_channel(&mut self, rtt_ms: u64) -> &mut Channel {
        if self.channel.is_none() {
            let mut ch = Channel::new();
            ch.update_window_for_rtt(rtt_ms);
            ch.seed_srtt(rtt_ms);
            self.channel = Some(ch);
        }
        // Safe: we just ensured Some above
        self.channel.as_mut().unwrap()
    }

    // Resource accessors
    /// Set the resource acceptance strategy.
    pub fn set_resource_strategy(&mut self, strategy: crate::resource::ResourceStrategy) {
        self.resource_strategy = strategy;
    }

    /// Get the resource acceptance strategy.
    pub fn resource_strategy(&self) -> crate::resource::ResourceStrategy {
        self.resource_strategy
    }

    /// Whether there is a pending resource advertisement awaiting accept/reject.
    pub fn has_pending_resource(&self) -> bool {
        self.pending_resource_adv.is_some()
    }

    /// Get the pending resource advertisement.
    pub fn pending_resource_adv(&self) -> Option<&crate::resource::ResourceAdvertisement> {
        self.pending_resource_adv.as_ref()
    }

    /// Set a pending resource advertisement.
    pub(crate) fn set_pending_resource_adv(&mut self, adv: crate::resource::ResourceAdvertisement) {
        self.pending_resource_adv = Some(adv);
    }

    /// Take the pending resource advertisement.
    pub(crate) fn take_pending_resource_adv(
        &mut self,
    ) -> Option<crate::resource::ResourceAdvertisement> {
        self.pending_resource_adv.take()
    }

    /// Whether an outgoing resource transfer is in progress.
    pub fn has_outgoing_resource(&self) -> bool {
        self.outgoing_resource.is_some()
    }

    /// Whether an incoming resource transfer is in progress.
    pub fn has_incoming_resource(&self) -> bool {
        self.incoming_resource.is_some()
    }

    /// Get outgoing resource status.
    pub fn outgoing_resource_status(&self) -> Option<crate::resource::ResourceStatus> {
        self.outgoing_resource.as_ref().map(|r| r.status())
    }

    /// Get incoming resource status.
    pub fn incoming_resource_status(&self) -> Option<crate::resource::ResourceStatus> {
        self.incoming_resource.as_ref().map(|r| r.status())
    }

    /// Get outgoing resource progress (0.0 to 1.0).
    pub fn outgoing_resource_progress(&self) -> Option<f32> {
        self.outgoing_resource.as_ref().map(|r| r.progress())
    }

    /// Get incoming resource progress (0.0 to 1.0).
    pub fn incoming_resource_progress(&self) -> Option<f32> {
        self.incoming_resource.as_ref().map(|r| r.progress())
    }

    /// Get a reference to the outgoing resource.
    pub(crate) fn outgoing_resource(&self) -> Option<&crate::resource::outgoing::OutgoingResource> {
        self.outgoing_resource.as_ref()
    }

    /// Get a mutable reference to the outgoing resource.
    pub(crate) fn outgoing_resource_mut(
        &mut self,
    ) -> Option<&mut crate::resource::outgoing::OutgoingResource> {
        self.outgoing_resource.as_mut()
    }

    /// Set the outgoing resource.
    pub(crate) fn set_outgoing_resource(
        &mut self,
        res: crate::resource::outgoing::OutgoingResource,
    ) {
        self.outgoing_resource = Some(res);
    }

    /// Take the outgoing resource out of the link (leaving None).
    pub(crate) fn take_outgoing_resource(
        &mut self,
    ) -> Option<crate::resource::outgoing::OutgoingResource> {
        self.outgoing_resource.take()
    }

    /// Clear the outgoing resource (and any remaining segment plan).
    ///
    /// Called on transfer failure/cancellation, so the segment plan of a split
    /// transfer is dropped with it. Between-segment transitions use
    /// take/set_outgoing_resource instead, which leave the plan intact.
    pub(crate) fn clear_outgoing_resource(&mut self) {
        self.outgoing_resource = None;
        self.outgoing_segments = None;
    }

    /// Set the outgoing segment plan for a split transfer.
    pub(crate) fn set_outgoing_segments(
        &mut self,
        plan: crate::resource::outgoing::OutgoingSegmentPlan,
    ) {
        self.outgoing_segments = Some(plan);
    }

    /// Take the outgoing segment plan out of the link (leaving None).
    pub(crate) fn take_outgoing_segments(
        &mut self,
    ) -> Option<crate::resource::outgoing::OutgoingSegmentPlan> {
        self.outgoing_segments.take()
    }

    /// Get a reference to the incoming resource.
    pub(crate) fn incoming_resource(&self) -> Option<&crate::resource::incoming::IncomingResource> {
        self.incoming_resource.as_ref()
    }

    /// Get a mutable reference to the incoming resource.
    pub(crate) fn incoming_resource_mut(
        &mut self,
    ) -> Option<&mut crate::resource::incoming::IncomingResource> {
        self.incoming_resource.as_mut()
    }

    /// Set the incoming resource.
    pub(crate) fn set_incoming_resource(
        &mut self,
        res: crate::resource::incoming::IncomingResource,
    ) {
        self.incoming_resource = Some(res);
    }

    /// Take the incoming resource out of the link (leaving None).
    pub(crate) fn take_incoming_resource(
        &mut self,
    ) -> Option<crate::resource::incoming::IncomingResource> {
        self.incoming_resource.take()
    }

    /// Clear the incoming resource.
    pub(crate) fn clear_incoming_resource(&mut self) {
        self.incoming_resource = None;
    }

    #[cfg(test)]
    pub(crate) fn set_timing_for_test(
        &mut self,
        keepalive_secs: u64,
        stale_time_secs: u64,
        last_inbound: u64,
    ) {
        self.keepalive_secs = keepalive_secs;
        self.stale_time_secs = stale_time_secs;
        self.last_inbound = last_inbound;
        self.had_inbound = true;
    }

    #[cfg(test)]
    pub(crate) fn set_link_key_for_test(&mut self, key: [u8; 64]) {
        self.link_key = Some(key);
    }

    /// Close the link (transition to Closed state)
    pub fn close(&mut self) {
        self.state = LinkState::Closed;
    }

    /// Require link to be in a specific state
    fn require_state(&self, expected: LinkState) -> Result<(), LinkError> {
        if self.state == expected {
            Ok(())
        } else {
            Err(LinkError::InvalidState)
        }
    }

    /// Require that we are the responder (server side)
    fn require_responder(&self) -> Result<(), LinkError> {
        if !self.initiator {
            Ok(())
        } else {
            Err(LinkError::InvalidState)
        }
    }

    /// Calculate link ID from the raw packet bytes
    ///
    /// Python RNS calculates link_id as:
    /// ```python
    /// hashable_part = bytes([raw[0] & 0x0F])
    /// if header_type == HEADER_2:
    ///     hashable_part += raw[(TRUNCATED_HASHBYTES//8)+2:]  # Skip transport_id
    /// else:
    ///     hashable_part += raw[2:]
    /// # Remove signalling bytes if present (payload > 64 bytes)
    /// link_id = truncated_hash(hashable_part)
    /// ```
    ///
    /// For HEADER_1: [flags (1)] [hops (1)] [dest_hash (16)] [context (1)] [payload (64-67)]
    /// For HEADER_2: [flags (1)] [hops (1)] [transport_id (16)] [dest_hash (16)] [context (1)] [payload (64-67)]
    ///
    /// The hashable part always includes: (flags & 0x0F) + dest_hash + context + payload
    /// This ensures the link_id is the same regardless of whether transport headers are present.
    pub fn calculate_link_id(raw_packet: &[u8]) -> LinkId {
        use crate::packet::HeaderType;

        let flags = raw_packet[0];
        let header_type = if flags & 0x40 != 0 {
            HeaderType::Type2
        } else {
            HeaderType::Type1
        };

        // Build hashable part: (flags & 0x0F) + [dest_hash + context + payload]
        // For HEADER_2, skip the transport_id (16 bytes after flags+hops)
        let mut hashable = alloc::vec::Vec::with_capacity(raw_packet.len() - 1);
        hashable.push(flags & 0x0F);

        let data_start = match header_type {
            HeaderType::Type2 => 2 + TRUNCATED_HASHBYTES, // Skip flags, hops, transport_id
            HeaderType::Type1 => 2,                       // Skip flags, hops only
        };
        hashable.extend_from_slice(&raw_packet[data_start..]);

        // If payload has signalling bytes (payload > LINK_REQUEST_BASE_SIZE), remove them
        // After stripping header: dest_hash(16) + context(1) + payload(64+) = 81+ bytes
        // Minimum hashable with 64-byte payload = 1 + 16 + 1 + 64 = 82 bytes
        let payload_offset = 1 + TRUNCATED_HASHBYTES + 1; // (flags&0x0F) + dest_hash + context = 18 bytes
        if hashable.len() > payload_offset + LINK_REQUEST_BASE_SIZE {
            let diff = hashable.len() - (payload_offset + LINK_REQUEST_BASE_SIZE);
            hashable.truncate(hashable.len() - diff);
        }

        LinkId::new(truncated_hash(&hashable))
    }

    /// Set the link ID (called after calculating it from the sent packet)
    pub fn set_link_id(&mut self, link_id: LinkId) {
        self.id = link_id;
    }

    /// Process a link proof from the destination
    ///
    /// PROOF format: [signature (64)] [peer_ephemeral_pub (32)] [signaling (0-3)]
    ///
    /// Returns Ok(()) if proof is valid and keys are derived
    pub fn process_proof(&mut self, proof_data: &[u8]) -> Result<(), LinkError> {
        use ed25519_dalek::Verifier;

        self.require_state(LinkState::Pending)?;

        // Proof format: signature (64) + X25519_pub (32) + signalling (3) = LINK_PROOF_SIZE bytes
        if proof_data.len() < LINK_PROOF_SIZE {
            return Err(LinkError::InvalidProof);
        }

        // Extract signature (first ED25519_SIGNATURE_SIZE bytes)
        let signature_bytes: [u8; ED25519_SIGNATURE_SIZE] = proof_data[..ED25519_SIGNATURE_SIZE]
            .try_into()
            .map_err(|_| LinkError::InvalidProof)?;
        let signature = ed25519_dalek::Signature::from_bytes(&signature_bytes);

        // Extract peer's ephemeral X25519 public key (next X25519_KEY_SIZE bytes)
        let peer_pub_bytes: [u8; X25519_KEY_SIZE] = proof_data
            [ED25519_SIGNATURE_SIZE..ED25519_SIGNATURE_SIZE + X25519_KEY_SIZE]
            .try_into()
            .map_err(|_| LinkError::InvalidProof)?;
        let peer_ephemeral_public = x25519_dalek::PublicKey::from(peer_pub_bytes);

        // Extract signalling bytes (last SIGNALING_SIZE bytes)
        let signalling_bytes: [u8; SIGNALING_SIZE] = proof_data
            [ED25519_SIGNATURE_SIZE + X25519_KEY_SIZE..LINK_PROOF_SIZE]
            .try_into()
            .map_err(|_| LinkError::InvalidProof)?;

        // Python RNS signs: link_id (16) + pub_bytes (32) + sig_pub_bytes (32) + signalling (3)
        // Where (from responder's perspective in prove()):
        //   pub_bytes = responder's X25519 ephemeral public key (in proof bytes 64-96)
        //   sig_pub_bytes = responder's Ed25519 signing public key (destination's verifying key)
        //   signalling = MTU and mode bytes from the proof
        let peer_verifying_key = self
            .peer_verifying_key
            .as_ref()
            .ok_or(LinkError::NoDestination)?;

        let signed_data = build_proof_signed_data(
            &self.id,
            &peer_pub_bytes,
            &peer_verifying_key.to_bytes(),
            &signalling_bytes,
        );

        // Verify the signature
        peer_verifying_key
            .verify(&signed_data, &signature)
            .map_err(|_| LinkError::InvalidProof)?;

        // Perform X25519 key exchange
        let ephemeral_private = self
            .ephemeral_private
            .take()
            .ok_or(LinkError::KeyExchangeFailed)?;
        let shared_secret = ephemeral_private.diffie_hellman(&peer_ephemeral_public);

        // Derive link key using HKDF
        let link_key = Self::derive_link_key(shared_secret.as_bytes(), &self.id);

        // Validate mode and store confirmed MTU from responder's signaling bytes
        let (confirmed_mtu, mode) = decode_signaling_bytes(&signalling_bytes);
        validate_mode(mode).map_err(|_| LinkError::InvalidProof)?;
        self.negotiated_mtu = if confirmed_mtu >= MTU as u32 {
            confirmed_mtu
        } else {
            MTU as u32
        };

        // Update state
        self.peer_ephemeral_public = Some(peer_ephemeral_public);
        self.link_key = Some(link_key);
        self.state = LinkState::Active;

        Ok(())
    }

    /// Set the destination's signing key (from announce)
    ///
    /// This must be called before process_proof() so we can verify the signature
    pub fn set_destination_keys(
        &mut self,
        verifying_key_bytes: &[u8; 32],
    ) -> Result<(), LinkError> {
        let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(verifying_key_bytes)
            .map_err(|_| LinkError::KeyExchangeFailed)?;
        self.peer_verifying_key = Some(verifying_key);
        Ok(())
    }

    /// Derive the 64-byte link key from shared secret and link ID
    ///
    /// Format: [signing/hmac_key (32)] [encryption_key (32)]
    /// This matches Python Reticulum's Token class which expects:
    /// - key[:32] = signing_key (for HMAC)
    /// - key[32:] = encryption_key (for AES)
    fn derive_link_key(shared_secret: &[u8; 32], link_id: &LinkId) -> [u8; 64] {
        let mut key = [0u8; 64];
        derive_key(shared_secret, Some(link_id.as_bytes()), None, &mut key);
        key
    }

    /// Get the derived HMAC/signing key (first 32 bytes of link_key)
    pub fn hmac_key(&self) -> Option<&[u8]> {
        self.link_key.as_ref().map(|k| &k[..32])
    }

    /// Get the derived encryption key (last 32 bytes of link_key)
    pub fn encryption_key(&self) -> Option<&[u8]> {
        self.link_key.as_ref().map(|k| &k[32..])
    }

    /// Get the full link key (for token encryption)
    pub fn link_key(&self) -> Option<&[u8; 64]> {
        self.link_key.as_ref()
    }

    /// Snapshot the link-derived inputs a resource build needs, so the
    /// CPU-heavy build can run without holding a borrow of this link (or the
    /// node lock above it) — leviculum#29.
    pub(crate) fn resource_crypt_params(&self) -> crate::resource::ResourceCryptParams {
        crate::resource::ResourceCryptParams {
            active: self.is_active(),
            negotiated_mtu: self.negotiated_mtu(),
            mdu: self.mdu(),
            token_key: self.link_key,
        }
    }

    /// Build the complete link request packet data with MTU signaling.
    ///
    /// Always includes 3-byte signaling bytes (67-byte payload) for interop
    /// with Python Reticulum, which accepts both 64-byte and 67-byte requests.
    ///
    /// # Arguments
    /// * `hw_mtu` - Hardware MTU of the outbound interface, or `None` to use base MTU (500)
    ///
    /// # Returns
    /// The raw packet bytes ready for framing/transmission.
    /// Also sets the link_id on this Link.
    pub fn build_link_request_packet(&mut self, hw_mtu: Option<u32>) -> alloc::vec::Vec<u8> {
        use crate::constants::MODE_AES256_CBC;
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketContext, PacketFlags, PacketType, TransportType};

        let mtu = hw_mtu.unwrap_or(MTU as u32);
        let request_data = self.create_link_request_with_mtu(mtu, MODE_AES256_CBC);

        // Build flags
        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Single,
            packet_type: PacketType::LinkRequest,
        };

        // Packet format: [flags (1)] [hops (1)] [dest_hash (16)] [context (1)] [data (67)]
        let mut packet = alloc::vec::Vec::with_capacity(
            1 + 1 + TRUNCATED_HASHBYTES + 1 + LINK_REQUEST_SIGNALING_SIZE,
        );
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.destination_hash.as_bytes());
        packet.push(PacketContext::None.to_byte());
        packet.extend_from_slice(&request_data);

        // Calculate and set link ID from the complete packet
        let link_id = Self::calculate_link_id(&packet);
        self.set_link_id(link_id);

        packet
    }

    /// Build link request packet with transport headers for multi-hop routing.
    ///
    /// When the packet needs to be routed through a transport node (indicated by
    /// having a transport_id from an announce), we use HEADER_2 format with the
    /// transport_id so intermediate nodes can route the packet.
    ///
    /// # Arguments
    /// * `next_hop` - The transport_id (identity hash of the next hop node that will forward this packet)
    /// * `hops_to_dest` - Number of hops to the destination (from path info, stored for reference)
    /// * `hw_mtu` - Hardware MTU of the outbound interface, or `None` to use base MTU (500)
    ///
    /// # Returns
    /// The raw packet bytes ready for framing/transmission.
    /// Also sets the link_id on this Link.
    ///
    /// # Note
    /// If `next_hop` is None, falls back to HEADER_1 (direct/broadcast).
    /// If `next_hop` is Some, uses HEADER_2 with transport routing regardless of hop count,
    /// because the presence of transport_id in an announce indicates the packet must be
    /// routed through that transport node.
    /// Regenerate the initiator's ephemeral X25519/Ed25519 keys.
    ///
    /// Codeberg #66: a link-establishment retry must carry fresh keys so
    /// its bytes (and packet hash) differ from the lost first request —
    /// identical re-sends die in every transport node's duplicate-hash
    /// dedup (ours and Python's alike). The caller must rebuild the link
    /// request afterwards; `build_link_request_packet*` then derives and
    /// sets the new link id. Only meaningful on a pending initiator link.
    pub fn regenerate_ephemeral_keys(&mut self, rng: &mut impl CryptoRngCore) {
        debug_assert!(self.initiator, "key regeneration is initiator-side only");
        let ephemeral_private = x25519_dalek::StaticSecret::random_from_rng(&mut *rng);
        self.ephemeral_public = x25519_dalek::PublicKey::from(&ephemeral_private);
        self.ephemeral_private = Some(ephemeral_private);

        let mut ed25519_seed = [0u8; 32];
        rng.fill_bytes(&mut ed25519_seed);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed25519_seed);
        self.verifying_key = signing_key.verifying_key();
        self.signing_key = Some(signing_key);
    }

    pub fn build_link_request_packet_with_transport(
        &mut self,
        next_hop: Option<[u8; TRUNCATED_HASHBYTES]>,
        hops_to_dest: u8,
        hw_mtu: Option<u32>,
    ) -> alloc::vec::Vec<u8> {
        use crate::constants::MODE_AES256_CBC;
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketContext, PacketFlags, PacketType, TransportType};

        // If no transport_id provided, use HEADER_1 (direct/broadcast)
        let Some(transport_id) = next_hop else {
            return self.build_link_request_packet(hw_mtu);
        };

        let mtu = hw_mtu.unwrap_or(MTU as u32);
        let request_data = self.create_link_request_with_mtu(mtu, MODE_AES256_CBC);

        // Build flags for HEADER_2 with transport routing
        // Type2=1 (bit 6), Transport=1 (bit 4), Single=1 (bits 3-2), LinkReq=2 (bits 1-0)
        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type2,
            context_flag: false,
            transport_type: TransportType::Transport,
            dest_type: DestinationType::Single,
            packet_type: PacketType::LinkRequest,
        };

        // Packet format: [flags (1)] [hops (1)] [transport_id (16)] [dest_hash (16)] [context (1)] [data (67)]
        let mut packet = alloc::vec::Vec::with_capacity(
            1 + 1 + TRUNCATED_HASHBYTES + TRUNCATED_HASHBYTES + 1 + LINK_REQUEST_SIGNALING_SIZE,
        );
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0 (we're originating)
        packet.extend_from_slice(&transport_id);
        packet.extend_from_slice(self.destination_hash.as_bytes());
        packet.push(PacketContext::None.to_byte());
        packet.extend_from_slice(&request_data);

        // Calculate and set link ID from the complete packet
        // Link ID calculation uses the same algorithm regardless of header type
        let link_id = Self::calculate_link_id(&packet);
        self.set_link_id(link_id);

        // Store hop count
        self.hops = hops_to_dest;

        packet
    }

    /// Get the token key (same as link key, no conversion needed)
    ///
    /// Link key format matches Token format: [hmac/signing (32)][encryption (32)]
    /// This matches Python Reticulum's Token class expectations.
    fn token_key(&self) -> Option<[u8; 64]> {
        self.link_key
    }

    /// Encrypt data for transmission over this link
    ///
    /// Returns the encrypted token: \[IV (16)\]\[ciphertext\]\[HMAC (32)\]
    pub fn encrypt(
        &self,
        plaintext: &[u8],
        output: &mut [u8],
        rng: &mut impl CryptoRngCore,
    ) -> Result<usize, LinkError> {
        use crate::crypto::encrypt_token;

        let token_key = self.token_key().ok_or(LinkError::InvalidState)?;
        let mut iv = [0u8; 16];
        rng.fill_bytes(&mut iv);

        encrypt_token(&token_key, &iv, plaintext, output).map_err(|_| LinkError::KeyExchangeFailed)
    }

    /// Decrypt data received over this link
    ///
    /// The token format is: \[IV (16)\]\[ciphertext\]\[HMAC (32)\]
    ///
    /// Returns the plaintext length written to output.
    pub fn decrypt(&self, token: &[u8], output: &mut [u8]) -> Result<usize, LinkError> {
        use crate::crypto::decrypt_token;

        let token_key = self.token_key().ok_or(LinkError::InvalidState)?;

        decrypt_token(&token_key, token, output).map_err(|e| {
            use crate::crypto::TokenError;
            match e {
                TokenError::HmacVerificationFailed => LinkError::InvalidProof,
                _ => LinkError::KeyExchangeFailed,
            }
        })
    }

    /// Calculate the required output buffer size for encrypting data
    ///
    /// Returns the size needed for: IV (16) + padded ciphertext + HMAC (32)
    pub fn encrypted_size(plaintext_len: usize) -> usize {
        let padded = ((plaintext_len / 16) + 1) * 16;
        16 + padded + 32
    }

    /// Build a data packet for transmission over this link
    ///
    /// The plaintext is encrypted and wrapped in a proper packet format.
    /// Returns the raw packet bytes ready for framing/transmission.
    pub fn build_data_packet(
        &self,
        plaintext: &[u8],
        rng: &mut impl CryptoRngCore,
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        self.build_data_packet_with_context(plaintext, PacketContext::None, rng)
    }

    /// Build a data packet with a specific context for transmission over this link
    ///
    /// The plaintext is encrypted and wrapped in a proper packet format.
    /// Returns the raw packet bytes ready for framing/transmission.
    ///
    /// # Arguments
    /// * `plaintext` - The data to encrypt and send
    /// * `packet_context` - The packet context (e.g., Channel, Resource)
    /// * `rng` - Random number generator for encryption IV
    pub fn build_data_packet_with_context(
        &self,
        plaintext: &[u8],
        packet_context: PacketContext,
        rng: &mut impl CryptoRngCore,
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketFlags, PacketType, TransportType};

        self.require_state(LinkState::Active)?;

        // Encrypt the data
        let encrypted_len = Self::encrypted_size(plaintext.len());
        let mut encrypted = alloc::vec![0u8; encrypted_len];
        let enc_len = self.encrypt(plaintext, &mut encrypted, rng)?;

        // Build flags for link data packet
        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: packet_context != PacketContext::None,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Link,
            packet_type: PacketType::Data,
        };

        // Packet format: [flags (1)] [hops (1)] [link_id (16)] [context (1)] [encrypted_data]
        let mut packet = alloc::vec::Vec::with_capacity(1 + 1 + TRUNCATED_HASHBYTES + 1 + enc_len);
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.id.as_bytes());
        packet.push(packet_context.to_byte());
        packet.extend_from_slice(&encrypted[..enc_len]);

        Ok(packet)
    }

    /// Build a data packet with pre-encrypted (raw) payload.
    ///
    /// Unlike `build_data_packet_with_context()`, this does NOT encrypt the data.
    /// Used for Resource data parts (context=RESOURCE) where the entire data blob
    /// is encrypted in bulk before segmentation.
    ///
    /// Python equivalent: `Packet(link, data, context=Packet.RESOURCE)` where
    /// `Packet.pack()` skips encryption for `context == RESOURCE`.
    pub fn build_raw_data_packet(
        &self,
        data: &[u8],
        packet_context: PacketContext,
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketFlags, PacketType, TransportType};

        self.require_state(LinkState::Active)?;

        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: packet_context != PacketContext::None,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Link,
            packet_type: PacketType::Data,
        };

        let mut packet =
            alloc::vec::Vec::with_capacity(1 + 1 + TRUNCATED_HASHBYTES + 1 + data.len());
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.id.as_bytes());
        packet.push(packet_context.to_byte());
        packet.extend_from_slice(data);

        Ok(packet)
    }

    /// Build a PROOF packet with a specific context and raw payload.
    ///
    /// Used for Resource completion proofs (context=RESOURCE_PRF).
    /// Unlike data proofs, resource proofs are NOT encrypted and carry
    /// `resource_hash + proof_hash` (not an Ed25519 signature).
    pub fn build_proof_packet_with_context(
        &self,
        data: &[u8],
        packet_context: PacketContext,
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketFlags, PacketType, TransportType};

        self.require_state(LinkState::Active)?;

        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: packet_context != PacketContext::None,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Link,
            packet_type: PacketType::Proof,
        };

        let mut packet =
            alloc::vec::Vec::with_capacity(1 + 1 + TRUNCATED_HASHBYTES + 1 + data.len());
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.id.as_bytes());
        packet.push(packet_context.to_byte());
        packet.extend_from_slice(data);

        Ok(packet)
    }

    /// Build a data proof packet using an Ed25519 signing key
    ///
    /// Creates a PROOF packet that confirms receipt of a data packet.
    /// The proof is signed with the provided signing key, which should be
    /// the destination's identity signing key (the sender can verify it
    /// using the verifying key from the announce).
    ///
    /// # Arguments
    /// * `packet_hash` - The hash of the received packet
    /// * `signing_key` - The Ed25519 signing key to sign with
    ///
    /// # Returns
    /// The raw proof packet bytes ready for transmission.
    pub fn build_data_proof_packet_with_signing_key(
        &self,
        packet_hash: &[u8; 32],
        signing_key: &ed25519_dalek::SigningKey,
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        let proof_data = self.create_data_proof_with_signing_key(packet_hash, signing_key);
        self.assemble_data_proof_packet(&proof_data)
    }

    /// Assemble a data proof into a complete PROOF packet
    fn assemble_data_proof_packet(
        &self,
        proof_data: &[u8; PROOF_DATA_SIZE],
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketContext, PacketFlags, PacketType, TransportType};

        self.require_state(LinkState::Active)?;

        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Link,
            packet_type: PacketType::Proof,
        };

        // Packet format: [flags (1)] [hops (1)] [link_id (16)] [context (1)] [proof_data]
        let mut packet =
            alloc::vec::Vec::with_capacity(1 + 1 + TRUNCATED_HASHBYTES + 1 + PROOF_DATA_SIZE);
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.id.as_bytes());
        packet.push(PacketContext::None.to_byte());
        packet.extend_from_slice(proof_data);

        Ok(packet)
    }

    /// Build the RTT (Round-Trip Time) packet that finalizes link establishment
    ///
    /// This packet must be sent by the initiator after processing the proof.
    /// The destination only considers the link "established" after receiving this.
    ///
    /// The RTT value is msgpack-encoded (float64 format).
    pub fn build_rtt_packet(
        &self,
        rtt_seconds: f64,
        rng: &mut impl CryptoRngCore,
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketContext, PacketFlags, PacketType, TransportType};

        self.require_state(LinkState::Active)?;

        // Encode RTT as msgpack float64: 0xCB + 8 bytes big-endian IEEE 754
        let mut rtt_data = [0u8; 9];
        rtt_data[0] = 0xCB; // msgpack float64 marker
        rtt_data[1..9].copy_from_slice(&rtt_seconds.to_be_bytes());

        // Encrypt the RTT data
        let encrypted_len = Self::encrypted_size(rtt_data.len());
        let mut encrypted = alloc::vec![0u8; encrypted_len];
        let enc_len = self.encrypt(&rtt_data, &mut encrypted, rng)?;

        // Build flags for link RTT packet
        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Link,
            packet_type: PacketType::Data,
        };

        // Packet format: [flags (1)] [hops (1)] [link_id (16)] [context (1)] [encrypted_data]
        let mut packet = alloc::vec::Vec::with_capacity(1 + 1 + TRUNCATED_HASHBYTES + 1 + enc_len);
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.id.as_bytes());
        packet.push(PacketContext::Lrrtt.to_byte());
        packet.extend_from_slice(&encrypted[..enc_len]);

        Ok(packet)
    }

    /// Build a keepalive packet
    ///
    /// Keepalive format: Single byte payload in KEEPALIVE context
    /// - Initiator sends 0xFF
    /// - Responder echoes 0xFE
    ///
    /// Keepalive packets are NOT encrypted (matching Python Reticulum behavior).
    pub fn build_keepalive_packet(&self) -> Result<alloc::vec::Vec<u8>, LinkError> {
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketContext, PacketFlags, PacketType, TransportType};

        // Allow keepalive echo on both Active and Stale links (recovery path)
        if self.state != LinkState::Active && self.state != LinkState::Stale {
            return Err(LinkError::InvalidState);
        }

        // Keepalive payload: KEEPALIVE_INITIATOR_BYTE for initiator, KEEPALIVE_RESPONDER_BYTE for responder
        let payload = if self.initiator {
            KEEPALIVE_INITIATOR_BYTE
        } else {
            KEEPALIVE_RESPONDER_BYTE
        };

        // Build flags for keepalive packet
        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Link,
            packet_type: PacketType::Data,
        };

        // Packet format: [flags (1)] [hops (1)] [link_id (16)] [context (1)] [payload (1)]
        let mut packet = alloc::vec::Vec::with_capacity(
            1 + 1 + TRUNCATED_HASHBYTES + 1 + KEEPALIVE_PAYLOAD_SIZE,
        );
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.id.as_bytes());
        packet.push(PacketContext::Keepalive.to_byte());
        packet.push(payload);

        Ok(packet)
    }

    /// Build a graceful close packet
    ///
    /// Close packet format: Encrypted link_id as payload in LINKCLOSE context
    /// Both sides can initiate close. Receiving a valid close packet transitions
    /// the link to CLOSED state.
    pub fn build_close_packet(
        &self,
        rng: &mut impl CryptoRngCore,
    ) -> Result<alloc::vec::Vec<u8>, LinkError> {
        use crate::destination::DestinationType;
        use crate::packet::{HeaderType, PacketContext, PacketFlags, PacketType, TransportType};

        // Can close from Active or Stale state
        if self.state != LinkState::Active && self.state != LinkState::Stale {
            return Err(LinkError::InvalidState);
        }

        // Close payload is the encrypted link_id
        let encrypted_len = Self::encrypted_size(TRUNCATED_HASHBYTES);
        let mut encrypted = alloc::vec![0u8; encrypted_len];
        let enc_len = self.encrypt(self.id.as_bytes(), &mut encrypted, rng)?;

        // Build flags for close packet
        let flags = PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Link,
            packet_type: PacketType::Data,
        };

        // Packet format: [flags (1)] [hops (1)] [link_id (16)] [context (1)] [encrypted_data]
        let mut packet = alloc::vec::Vec::with_capacity(1 + 1 + TRUNCATED_HASHBYTES + 1 + enc_len);
        packet.push(flags.to_byte());
        packet.push(0); // hops = 0
        packet.extend_from_slice(self.id.as_bytes());
        packet.push(PacketContext::LinkClose.to_byte());
        packet.extend_from_slice(&encrypted[..enc_len]);

        Ok(packet)
    }

    /// Process an incoming close packet
    ///
    /// Verifies that the decrypted payload matches our link_id.
    /// If valid, transitions the link to CLOSED state.
    ///
    /// # Arguments
    /// * `encrypted_data` - The encrypted payload from the LINKCLOSE packet
    ///
    /// # Returns
    /// Ok(()) if the close was valid and link is now closed, Err otherwise.
    pub fn process_close(&mut self, encrypted_data: &[u8]) -> Result<(), LinkError> {
        // Can receive close from Active or Stale state
        if self.state != LinkState::Active && self.state != LinkState::Stale {
            return Err(LinkError::InvalidState);
        }

        // Decrypt the close data
        let mut plaintext = alloc::vec![0u8; encrypted_data.len()];
        let plaintext_len = self.decrypt(encrypted_data, &mut plaintext)?;

        // Close payload should be the link_id (16 bytes)
        if plaintext_len != TRUNCATED_HASHBYTES {
            return Err(LinkError::InvalidProof);
        }

        // Verify the decrypted payload matches our link_id
        if plaintext[..plaintext_len] != *self.id.as_bytes() {
            return Err(LinkError::InvalidProof);
        }

        // Valid close - transition to CLOSED
        self.state = LinkState::Closed;

        Ok(())
    }

    /// Process an incoming keepalive packet
    ///
    /// Validates the keepalive byte (keepalive packets are NOT encrypted).
    /// - Initiator expects 0xFE from responder
    /// - Responder expects 0xFF from initiator
    ///
    /// # Returns
    /// Ok(true) if responder should echo back a keepalive, Ok(false) otherwise.
    pub fn process_keepalive(&mut self, data: &[u8]) -> Result<bool, LinkError> {
        // Allow processing keepalives on both Active and Stale links (recovery path)
        if self.state != LinkState::Active && self.state != LinkState::Stale {
            return Err(LinkError::InvalidState);
        }

        // Keepalive payload should be exactly KEEPALIVE_PAYLOAD_SIZE byte(s)
        if data.len() != KEEPALIVE_PAYLOAD_SIZE {
            return Err(LinkError::InvalidRtt);
        }

        let keepalive_byte = data[0];

        // Validate the keepalive byte
        if self.initiator {
            // We're initiator, expect KEEPALIVE_RESPONDER_BYTE from responder
            if keepalive_byte != KEEPALIVE_RESPONDER_BYTE {
                return Err(LinkError::InvalidRtt);
            }
            // A valid responder echo proves our keepalive round-tripped.
            self.keepalives_acked = self.keepalives_acked.saturating_add(1);
            // Initiator doesn't echo back
            Ok(false)
        } else {
            // We're responder, expect KEEPALIVE_INITIATOR_BYTE from initiator
            if keepalive_byte != KEEPALIVE_INITIATOR_BYTE {
                return Err(LinkError::InvalidRtt);
            }
            // Responder should echo back
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::MTU;
    use crate::destination::DestinationHash;
    use alloc::vec;
    use alloc::vec::Vec;
    use rand_core::OsRng;

    #[test]
    fn test_link_creation() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);

        assert_eq!(link.state(), LinkState::Pending);
        assert!(link.is_initiator());
        assert_eq!(link.destination_hash(), &dest_hash);
        // ID starts as zeros before handshake completes
        assert_eq!(link.id(), &[0u8; TRUNCATED_HASHBYTES]);
    }

    #[test]
    fn test_link_request_data() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);

        let request = link.create_link_request();
        assert_eq!(request.len(), 64);

        // Check that keys are embedded
        assert_eq!(&request[..32], &link.ephemeral_public_bytes());
        assert_eq!(&request[32..64], &link.verifying_key_bytes());
    }

    #[test]
    fn test_link_request_with_mtu() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);

        let request = link.create_link_request_with_mtu(MTU as u32, 1);
        assert_eq!(request.len(), 67);

        // Check that keys are embedded
        assert_eq!(&request[..32], &link.ephemeral_public_bytes());
        assert_eq!(&request[32..64], &link.verifying_key_bytes());

        // Check MTU signaling bytes encode MTU and mode
        // MTU 500 = 0x1F4, mode 1 -> signaling = 0x1F4 | (1 << 21) = 0x2001F4
        // Big-endian bytes [1:] = [0x20, 0x01, 0xF4]
        assert_eq!(request[64], 0x20);
        assert_eq!(request[65], 0x01);
        assert_eq!(request[66], 0xF4);
    }

    #[test]
    fn test_link_id_calculation() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);

        // Construct raw packet: [flags][hops][dest_hash][context][request_data]
        let request = link.create_link_request();
        let mut raw_packet = Vec::new();
        raw_packet.push(0x02); // flags
        raw_packet.push(0x00); // hops
        raw_packet.extend_from_slice(dest_hash.as_bytes());
        raw_packet.push(0x00); // context
        raw_packet.extend_from_slice(&request);

        let link_id = Link::calculate_link_id(&raw_packet);

        // Link ID should be 16 bytes
        assert_eq!(link_id.as_bytes().len(), TRUNCATED_HASHBYTES);
        // Should be deterministic
        assert_eq!(link_id, Link::calculate_link_id(&raw_packet));
    }

    #[test]
    fn test_set_link_id() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        let new_id = LinkId::new([0xAB; TRUNCATED_HASHBYTES]);
        link.set_link_id(new_id);

        assert_eq!(*link.id(), new_id);
    }

    #[test]
    fn test_process_proof_invalid_state() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        link.set_state(LinkState::Active);

        let fake_proof = [0u8; PROOF_DATA_SIZE];
        let result = link.process_proof(&fake_proof);
        assert!(matches!(result, Err(LinkError::InvalidState)));
    }

    #[test]
    fn test_process_proof_too_short() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        let short_proof = [0u8; 98]; // One byte short of required 99
        let result = link.process_proof(&short_proof);
        assert!(matches!(result, Err(LinkError::InvalidProof)));
    }

    #[test]
    fn test_process_proof_no_destination_key() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        let fake_proof = [0u8; 99]; // 64 sig + 32 X25519 + 3 signalling
        let result = link.process_proof(&fake_proof);
        // Should fail because we haven't set the destination's verifying key
        assert!(matches!(result, Err(LinkError::NoDestination)));
    }

    #[test]
    fn test_set_destination_keys() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        // Create a valid Ed25519 verifying key
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
        let verifying_key = signing_key.verifying_key();

        let result = link.set_destination_keys(&verifying_key.to_bytes());
        assert!(result.is_ok());
    }

    #[test]
    fn test_key_derivation() {
        // Test that key derivation produces consistent results
        let shared_secret = [0x42; 32];
        let link_id = LinkId::new([0x13; TRUNCATED_HASHBYTES]);

        let key1 = Link::derive_link_key(&shared_secret, &link_id);
        let key2 = Link::derive_link_key(&shared_secret, &link_id);

        assert_eq!(key1, key2);
        assert_eq!(key1.len(), 64);

        // Different link_id should produce different key
        let link_id2 = LinkId::new([0x14; TRUNCATED_HASHBYTES]);
        let key3 = Link::derive_link_key(&shared_secret, &link_id2);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_full_handshake_simulation() {
        use ed25519_dalek::Signer;

        // Simulate a full handshake between initiator and destination
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);

        // --- Initiator side ---
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        let request = link.create_link_request();

        // Build raw packet: [flags][hops][dest_hash][context][request]
        let mut raw_packet = Vec::new();
        raw_packet.push(0x02); // flags
        raw_packet.push(0x00); // hops
        raw_packet.extend_from_slice(dest_hash.as_bytes());
        raw_packet.push(0x00); // context
        raw_packet.extend_from_slice(&request);

        // Calculate and set link ID from raw packet
        let link_id = Link::calculate_link_id(&raw_packet);
        link.set_link_id(link_id);

        // --- Destination side (simulated) ---
        // Destination has its own identity with signing key
        let dest_signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]);
        let dest_verifying_key = dest_signing_key.verifying_key();

        // Destination generates ephemeral X25519 key
        let dest_ephemeral_private = x25519_dalek::StaticSecret::random_from_rng(rand_core::OsRng);
        let dest_ephemeral_public = x25519_dalek::PublicKey::from(&dest_ephemeral_private);

        // Python RNS signs: link_id (16) + pub_bytes (32) + sig_pub_bytes (32) + signalling (3)
        // Where (from responder's perspective):
        //   pub_bytes = responder's X25519 ephemeral public key
        //   sig_pub_bytes = responder's Ed25519 signing public key
        //   signalling = MTU and mode bytes
        let signalling_bytes = encode_signaling_bytes(500, crate::constants::MODE_AES256_CBC);
        let mut signed_data = [0u8; 83];
        signed_data[..TRUNCATED_HASHBYTES].copy_from_slice(link_id.as_bytes());
        signed_data[TRUNCATED_HASHBYTES..TRUNCATED_HASHBYTES + 32]
            .copy_from_slice(dest_ephemeral_public.as_bytes()); // Responder's X25519 pub
        signed_data[TRUNCATED_HASHBYTES + 32..TRUNCATED_HASHBYTES + 64]
            .copy_from_slice(&dest_verifying_key.to_bytes()); // Responder's Ed25519 pub
        signed_data[TRUNCATED_HASHBYTES + 64..].copy_from_slice(&signalling_bytes);
        let signature = dest_signing_key.sign(&signed_data);

        // Create proof: [signature (64)] [ephemeral_pub (32)] [signalling (3)]
        let mut proof = [0u8; 99];
        proof[..64].copy_from_slice(&signature.to_bytes());
        proof[64..96].copy_from_slice(dest_ephemeral_public.as_bytes());
        proof[96..99].copy_from_slice(&signalling_bytes);

        // --- Back to initiator ---
        // Set destination's verifying key (would come from announce)
        link.set_destination_keys(&dest_verifying_key.to_bytes())
            .unwrap();

        // Process the proof
        let result = link.process_proof(&proof);
        assert!(result.is_ok());

        // Link should now be active
        assert_eq!(link.state(), LinkState::Active);
        assert!(link.link_key().is_some());
        assert!(link.encryption_key().is_some());
        assert!(link.hmac_key().is_some());
    }

    #[test]
    fn test_link_encrypt_decrypt() {
        let (initiator, _responder) = setup_active_link_pair();

        let plaintext = b"Hello, encrypted link!";
        let encrypted_len = Link::encrypted_size(plaintext.len());
        let mut encrypted = vec![0u8; encrypted_len];

        let enc_len = initiator
            .encrypt(plaintext, &mut encrypted, &mut OsRng)
            .unwrap();
        assert!(enc_len > plaintext.len());

        let mut decrypted = vec![0u8; plaintext.len() + 16];
        let dec_len = initiator
            .decrypt(&encrypted[..enc_len], &mut decrypted)
            .unwrap();

        assert_eq!(dec_len, plaintext.len());
        assert_eq!(&decrypted[..dec_len], plaintext);
    }

    #[test]
    fn test_link_decrypt_tampered() {
        let (initiator, _responder) = setup_active_link_pair();

        let plaintext = b"Secret message";
        let mut encrypted = vec![0u8; Link::encrypted_size(plaintext.len())];
        let enc_len = initiator
            .encrypt(plaintext, &mut encrypted, &mut OsRng)
            .unwrap();

        // Tamper with the ciphertext
        encrypted[20] ^= 0xFF;

        // Decrypt should fail due to HMAC verification
        let mut decrypted = vec![0u8; 64];
        let result = initiator.decrypt(&encrypted[..enc_len], &mut decrypted);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypted_size() {
        // 0 bytes -> 16 bytes padded -> 16 + 16 IV + 32 HMAC = 64
        assert_eq!(Link::encrypted_size(0), 64);

        // 1 byte -> 16 bytes padded -> 64
        assert_eq!(Link::encrypted_size(1), 64);

        // 15 bytes -> 16 bytes padded -> 64
        assert_eq!(Link::encrypted_size(15), 64);

        // 16 bytes -> 32 bytes padded -> 16 + 32 + 32 = 80
        assert_eq!(Link::encrypted_size(16), 80);

        // 100 bytes -> 112 bytes padded -> 16 + 112 + 32 = 160
        assert_eq!(Link::encrypted_size(100), 160);
    }

    #[test]
    fn test_build_data_packet() {
        let (initiator, _responder) = setup_active_link_pair();

        let message = b"Hello, link!";
        let packet = initiator.build_data_packet(message, &mut OsRng).unwrap();

        // Verify packet structure:
        // [flags (1)] [hops (1)] [link_id (16)] [context (1)] [encrypted_data]
        assert!(packet.len() >= 19 + 48); // header + min encrypted size

        // dest_type=Link=0b11, packet_type=Data=0b00 -> bits 3-0 = 0b1100 = 0x0C
        assert_eq!(packet[0] & 0x0F, 0x0C);

        // Hops should be 0
        assert_eq!(packet[1], 0x00);

        // Link ID should be in bytes 2-17
        assert_eq!(&packet[2..18], initiator.id().as_bytes());

        // Context should be None (0x00)
        assert_eq!(packet[18], 0x00);

        // Decrypt payload to verify round-trip
        let encrypted_data = &packet[19..];
        let mut decrypted = vec![0u8; message.len() + 16];
        let dec_len = initiator.decrypt(encrypted_data, &mut decrypted).unwrap();
        assert_eq!(dec_len, message.len());
        assert_eq!(&decrypted[..dec_len], message);
    }

    #[test]
    fn test_build_data_packet_not_active() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);

        // Link is in Pending state, not Active
        let result = link.build_data_packet(b"Hello", &mut OsRng);
        assert!(matches!(result, Err(LinkError::InvalidState)));
    }

    // ==================== RESPONDER-SIDE TESTS ====================

    #[test]
    fn test_new_incoming_link() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);

        // Create initiator's link request
        let initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();

        // Calculate link ID (as if from raw packet)
        let mut raw_packet = Vec::new();
        raw_packet.push(0x02); // flags
        raw_packet.push(0x00); // hops
        raw_packet.extend_from_slice(dest_hash.as_bytes());
        raw_packet.push(0x00); // context
        raw_packet.extend_from_slice(&request_data);
        let link_id = Link::calculate_link_id(&raw_packet);

        // Create responder's link
        let responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();

        assert_eq!(responder.state(), LinkState::Pending);
        assert!(!responder.is_initiator());
        assert_eq!(responder.id(), &link_id);
        assert_eq!(responder.destination_hash(), &dest_hash);
        // Responder should have peer's public keys set
        assert!(responder.peer_ephemeral_public.is_some());
        assert!(responder.peer_verifying_key.is_some());
    }

    #[test]
    fn test_new_incoming_link_invalid_request() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0xAB; TRUNCATED_HASHBYTES]);

        // Too short request data
        let short_data = [0u8; 63];
        let result = Link::new_incoming(&short_data, link_id, dest_hash, &mut OsRng, None);
        assert!(matches!(result, Err(LinkError::InvalidRequest)));
    }

    #[test]
    fn test_build_proof_packet() {
        use crate::identity::Identity;

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);

        // Create destination identity
        let identity = Identity::generate(&mut OsRng);

        // Create initiator's link request
        let initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();

        // Calculate link ID
        let mut raw_packet = Vec::new();
        raw_packet.push(0x02);
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(dest_hash.as_bytes());
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(&request_data);
        let link_id = Link::calculate_link_id(&raw_packet);

        // Create responder's link
        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();

        // Build proof packet
        let proof_packet = responder
            .build_proof_packet(&identity, MTU as u32, 1)
            .unwrap();

        // Verify responder state
        assert_eq!(responder.state(), LinkState::Handshake);
        assert!(responder.link_key().is_some());

        // Verify packet structure
        // [flags (1)] [hops (1)] [link_id (16)] [context (1)] [proof_data (99)]
        assert_eq!(proof_packet.len(), 1 + 1 + TRUNCATED_HASHBYTES + 1 + 99);

        // Context should be LRPROOF
        use crate::packet::PacketContext;
        assert_eq!(proof_packet[18], PacketContext::Lrproof.to_byte());

        // Link ID in packet should match
        assert_eq!(&proof_packet[2..18], link_id.as_bytes());
    }

    #[test]
    fn test_build_proof_packet_wrong_state() {
        use crate::identity::Identity;

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let identity = Identity::generate(&mut OsRng);

        let initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();
        let link_id = LinkId::new([0xAB; TRUNCATED_HASHBYTES]);

        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();

        // Build proof once (transitions to Handshake)
        responder
            .build_proof_packet(&identity, MTU as u32, 1)
            .unwrap();

        // Second call should fail (wrong state)
        let result = responder.build_proof_packet(&identity, MTU as u32, 1);
        assert!(matches!(result, Err(LinkError::InvalidState)));
    }

    #[test]
    fn test_build_proof_packet_from_initiator() {
        use crate::identity::Identity;

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let identity = Identity::generate(&mut OsRng);

        // Initiator should not be able to build proof
        let mut initiator = Link::new_outgoing(dest_hash, &mut OsRng);

        let result = initiator.build_proof_packet(&identity, MTU as u32, 1);
        assert!(matches!(result, Err(LinkError::InvalidState)));
    }

    #[test]
    fn test_full_handshake_responder_side() {
        use crate::identity::Identity;

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);

        // --- Destination setup ---
        let dest_identity = Identity::generate(&mut OsRng);

        // --- Initiator side ---
        let mut initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();

        // Calculate link ID from raw packet
        let mut raw_packet = Vec::new();
        raw_packet.push(0x02);
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(dest_hash.as_bytes());
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(&request_data);
        let link_id = Link::calculate_link_id(&raw_packet);
        initiator.set_link_id(link_id);

        // --- Responder side ---
        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();

        // Build proof packet
        let proof_packet = responder
            .build_proof_packet(&dest_identity, MTU as u32, 1)
            .unwrap();

        // --- Back to initiator ---
        // Set destination's verifying key
        initiator
            .set_destination_keys(dest_identity.ed25519_verifying().as_bytes())
            .unwrap();

        // Extract proof data from packet (skip header)
        let proof_data = &proof_packet[19..]; // Skip flags(1) + hops(1) + link_id(16) + context(1)

        // Process proof
        initiator.process_proof(proof_data).unwrap();
        assert_eq!(initiator.state(), LinkState::Active);

        // Both links should have the same derived key
        assert_eq!(initiator.link_key().unwrap(), responder.link_key().unwrap());
    }

    #[test]
    fn test_process_rtt() {
        use crate::identity::Identity;

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let dest_identity = Identity::generate(&mut OsRng);

        // Set up links
        let mut initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();

        let mut raw_packet = Vec::new();
        raw_packet.push(0x02);
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(dest_hash.as_bytes());
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(&request_data);
        let link_id = Link::calculate_link_id(&raw_packet);
        initiator.set_link_id(link_id);

        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();

        // Build and process proof
        let proof_packet = responder
            .build_proof_packet(&dest_identity, MTU as u32, 1)
            .unwrap();
        initiator
            .set_destination_keys(dest_identity.ed25519_verifying().as_bytes())
            .unwrap();
        initiator.process_proof(&proof_packet[19..]).unwrap();

        // Initiator builds RTT packet
        let rtt_seconds = 0.05; // 50ms
        let rtt_packet = initiator.build_rtt_packet(rtt_seconds, &mut OsRng).unwrap();

        // Extract encrypted data from RTT packet
        let encrypted_data = &rtt_packet[19..];

        // Responder processes RTT
        let received_rtt = responder.process_rtt(encrypted_data).unwrap();

        // Responder should now be active
        assert_eq!(responder.state(), LinkState::Active);
        assert!((received_rtt - rtt_seconds).abs() < 0.001);
        assert!(responder.rtt_us().is_some());
    }

    #[test]
    fn test_process_rtt_rejects_non_finite() {
        use crate::identity::Identity;

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let dest_identity = Identity::generate(&mut OsRng);
        let mut initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();
        let mut raw_packet = Vec::new();
        raw_packet.push(0x02);
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(dest_hash.as_bytes());
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(&request_data);
        let link_id = Link::calculate_link_id(&raw_packet);
        initiator.set_link_id(link_id);
        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        let proof_packet = responder
            .build_proof_packet(&dest_identity, MTU as u32, 1)
            .unwrap();
        initiator
            .set_destination_keys(dest_identity.ed25519_verifying().as_bytes())
            .unwrap();
        initiator.process_proof(&proof_packet[19..]).unwrap();

        // A non-finite or non-positive RTT must be rejected, leaving the
        // responder in Handshake (not Active) so the poisoned keepalive/stale
        // timers never take effect. build_rtt_packet is &self, so reuse it.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.0] {
            let rtt_packet = initiator.build_rtt_packet(bad, &mut OsRng).unwrap();
            let result = responder.process_rtt(&rtt_packet[19..]);
            assert!(
                matches!(result, Err(LinkError::InvalidRtt)),
                "RTT {bad} must be rejected, got {result:?}"
            );
        }
        assert_eq!(
            responder.state(),
            LinkState::Handshake,
            "a rejected RTT must not activate the link"
        );

        // A plausible finite RTT still activates the link (control).
        let good = initiator.build_rtt_packet(0.05, &mut OsRng).unwrap();
        responder.process_rtt(&good[19..]).unwrap();
        assert_eq!(responder.state(), LinkState::Active);
    }

    #[test]
    fn test_process_rtt_wrong_state() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0xAB; TRUNCATED_HASHBYTES]);

        let initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();

        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();

        // Responder is still in Pending state (no proof built yet)
        let fake_data = [0u8; 64];
        let result = responder.process_rtt(&fake_data);
        assert!(matches!(result, Err(LinkError::InvalidState)));
    }

    #[test]
    fn test_bidirectional_data_after_handshake() {
        let (initiator, responder) = setup_active_link_pair();

        assert_eq!(initiator.state(), LinkState::Active);
        assert_eq!(responder.state(), LinkState::Active);

        // Initiator -> responder
        let message1 = b"Hello from initiator!";
        let mut encrypted1 = vec![0u8; Link::encrypted_size(message1.len())];
        let enc_len1 = initiator
            .encrypt(message1, &mut encrypted1, &mut OsRng)
            .unwrap();

        let mut decrypted1 = vec![0u8; message1.len() + 16];
        let dec_len1 = responder
            .decrypt(&encrypted1[..enc_len1], &mut decrypted1)
            .unwrap();
        assert_eq!(&decrypted1[..dec_len1], message1);

        // Responder -> initiator
        let message2 = b"Hello from responder!";
        let mut encrypted2 = vec![0u8; Link::encrypted_size(message2.len())];
        let enc_len2 = responder
            .encrypt(message2, &mut encrypted2, &mut OsRng)
            .unwrap();

        let mut decrypted2 = vec![0u8; message2.len() + 16];
        let dec_len2 = initiator
            .decrypt(&encrypted2[..enc_len2], &mut decrypted2)
            .unwrap();
        assert_eq!(&decrypted2[..dec_len2], message2);
    }

    #[test]
    fn test_build_link_request_with_transport() {
        use crate::packet::{HeaderType, Packet, TransportType};

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let transport_id = [0xAB; TRUNCATED_HASHBYTES];

        // Test with transport_id and hops=1 (should still use HEADER_2 because transport_id is set)
        let mut link1 = Link::new_outgoing(dest_hash, &mut OsRng);
        let packet1 = link1.build_link_request_packet_with_transport(Some(transport_id), 1, None);

        // Should be HEADER_2 (transport_id is set, even with hops=1)
        let parsed1 = Packet::unpack(&packet1).unwrap();
        assert_eq!(parsed1.flags.header_type, HeaderType::Type2);
        assert_eq!(parsed1.flags.transport_type, TransportType::Transport);
        assert_eq!(parsed1.transport_id, Some(transport_id));
        assert_eq!(parsed1.destination_hash, dest_hash);
        assert_eq!(link1.hops(), 1);

        // Test with hops > 1 and transport_id (should use HEADER_2)
        let mut link2 = Link::new_outgoing(dest_hash, &mut OsRng);
        let packet2 = link2.build_link_request_packet_with_transport(Some(transport_id), 2, None);

        // Should be HEADER_2 with transport_id
        let parsed2 = Packet::unpack(&packet2).unwrap();
        assert_eq!(parsed2.flags.header_type, HeaderType::Type2);
        assert_eq!(parsed2.flags.transport_type, TransportType::Transport);
        assert_eq!(parsed2.transport_id, Some(transport_id));
        assert_eq!(parsed2.destination_hash, dest_hash);
        assert_eq!(link2.hops(), 2);

        // Test with no transport_id (should use HEADER_1)
        let mut link3 = Link::new_outgoing(dest_hash, &mut OsRng);
        let packet3 = link3.build_link_request_packet_with_transport(None, 5, None);

        let parsed3 = Packet::unpack(&packet3).unwrap();
        assert_eq!(parsed3.flags.header_type, HeaderType::Type1);
        assert!(parsed3.transport_id.is_none());
    }

    #[test]
    fn test_build_link_request_with_transport_flags() {
        use crate::packet::Packet;

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let transport_id = [0xCD; TRUNCATED_HASHBYTES];

        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        let packet = link.build_link_request_packet_with_transport(Some(transport_id), 3, None);

        // Parse and verify flags byte
        let parsed = Packet::unpack(&packet).unwrap();

        // Expected flags for HEADER_2 link request:
        // Bit 6: Header Type = 1 (Type2)
        // Bit 5: Context Flag = 0
        // Bit 4: Transport Type = 1 (Transport)
        // Bits 3-2: Dest Type = 00 (Single = 0x00)
        // Bits 1-0: Packet Type = 10 (LinkRequest)
        // = 0b01010010 = 0x52
        let expected_flags = 0x52;
        assert_eq!(
            packet[0], expected_flags,
            "Flags should be 0x52 for HEADER_2 transport link request"
        );

        // Hops should be 0 (we're originating)
        assert_eq!(packet[1], 0x00);

        // Transport ID should be at bytes 2-17
        assert_eq!(&packet[2..18], &transport_id);

        // Destination hash should be at bytes 18-33
        assert_eq!(&packet[18..34], dest_hash.as_bytes().as_slice());

        // Verify parsed packet matches
        assert_eq!(parsed.hops, 0);
        assert_eq!(parsed.transport_id, Some(transport_id));
        assert_eq!(parsed.destination_hash, dest_hash);
    }

    #[test]
    fn test_link_id_same_for_header1_and_header2() {
        // This test verifies that the link_id calculation is the same
        // regardless of whether HEADER_1 or HEADER_2 is used.
        // This is critical for transport routing to work correctly.

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let transport_id = [0xAB; TRUNCATED_HASHBYTES];

        // Create two links with the same ephemeral keys by using the same seed
        // We need to ensure the request data is identical for fair comparison
        let seed = [0x55; 32];
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();

        let x25519_secret = x25519_dalek::StaticSecret::from([0x66; 32]);
        let x25519_public = x25519_dalek::PublicKey::from(&x25519_secret);

        // Manually create the link request data (same for both)
        let mut request_data = [0u8; 64];
        request_data[..32].copy_from_slice(x25519_public.as_bytes());
        request_data[32..64].copy_from_slice(&verifying_key.to_bytes());

        // Build HEADER_1 packet manually
        let mut header1_packet = Vec::new();
        header1_packet.push(0x02); // flags: Type1, Broadcast, Single, LinkRequest
        header1_packet.push(0x00); // hops
        header1_packet.extend_from_slice(dest_hash.as_bytes());
        header1_packet.push(0x00); // context
        header1_packet.extend_from_slice(&request_data);

        // Build HEADER_2 packet manually
        let mut header2_packet = Vec::new();
        header2_packet.push(0x52); // flags: Type2, Transport, Single, LinkRequest
        header2_packet.push(0x00); // hops
        header2_packet.extend_from_slice(&transport_id);
        header2_packet.extend_from_slice(dest_hash.as_bytes());
        header2_packet.push(0x00); // context
        header2_packet.extend_from_slice(&request_data);

        // Calculate link_id for both
        let link_id_h1 = Link::calculate_link_id(&header1_packet);
        let link_id_h2 = Link::calculate_link_id(&header2_packet);

        // They should be identical
        assert_eq!(
            link_id_h1, link_id_h2,
            "Link ID should be the same for HEADER_1 and HEADER_2 packets with same content"
        );
    }

    // ==================== KEEPALIVE TESTS ====================

    #[test]
    fn test_keepalive_calculation_from_rtt() {
        // Test minimum clamping
        assert_eq!(Link::calculate_keepalive_from_rtt(0.0), 5);
        assert_eq!(Link::calculate_keepalive_from_rtt(0.01), 5);

        // Test calculation at midpoint (RTT = 1.75 -> keepalive = 360)
        // Formula: rtt * (360 / 1.75) = rtt * 205.71
        // At RTT = 1.75: 1.75 * 205.71 = 360
        assert_eq!(Link::calculate_keepalive_from_rtt(1.75), 360);

        // Test maximum clamping
        assert_eq!(Link::calculate_keepalive_from_rtt(2.0), 360);
        assert_eq!(Link::calculate_keepalive_from_rtt(10.0), 360);

        // Test intermediate values
        // RTT = 0.1: 0.1 * (360/1.75) = 20.57 -> 20
        let keepalive_01 = Link::calculate_keepalive_from_rtt(0.1);
        assert!((20..=21).contains(&keepalive_01));

        // RTT = 0.5: 0.5 * (360/1.75) = 102.86 -> 102
        let keepalive_05 = Link::calculate_keepalive_from_rtt(0.5);
        assert!((102..=103).contains(&keepalive_05));
    }

    #[test]
    fn test_update_keepalive_from_rtt() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        // Default values
        assert_eq!(link.keepalive_secs(), 360);
        assert_eq!(link.stale_time_secs(), 720); // 360 * 2

        // Update with low RTT
        link.update_keepalive_from_rtt(0.05);
        assert_eq!(link.keepalive_secs(), 10); // ~10.28, clamped
        assert_eq!(link.stale_time_secs(), 20); // 10 * 2

        // Update with high RTT
        link.update_keepalive_from_rtt(2.0);
        assert_eq!(link.keepalive_secs(), 360);
        assert_eq!(link.stale_time_secs(), 720);
    }

    #[test]
    fn test_keepalive_override_survives_rtt() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        // Override takes effect immediately.
        link.set_keepalive_override(Some(6));
        assert_eq!(link.keepalive_secs(), 6);
        assert_eq!(link.stale_time_secs(), 12); // 6 * 2

        // RTT recalculation must NOT replace the override.
        link.update_keepalive_from_rtt(2.0);
        assert_eq!(link.keepalive_secs(), 6);
        assert_eq!(link.stale_time_secs(), 12);

        // Below-minimum overrides clamp to LINK_KEEPALIVE_MIN_SECS.
        link.set_keepalive_override(Some(1));
        assert_eq!(link.keepalive_secs(), LINK_KEEPALIVE_MIN_SECS);

        // Clearing the override restores RTT-driven behaviour.
        link.set_keepalive_override(None);
        link.update_keepalive_from_rtt(2.0);
        assert_eq!(link.keepalive_secs(), 360);
        assert_eq!(link.stale_time_secs(), 720);
    }

    #[test]
    fn test_link_established_at_clock_zero_can_go_stale() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        link.set_state(LinkState::Active);
        // Establish at clock second 0 (the node's first uptime second): the
        // monotonic clock starts at 0, so last_inbound is stamped 0.
        link.mark_established(0);

        // After the stale window of silence the link must be detectable as
        // stale; a last_inbound of 0 must mean "established at t=0", not
        // "never had inbound".
        let now = link.stale_time_secs() + 1;
        assert!(
            link.is_stale(now),
            "a link established at clock second 0 must still go stale"
        );

        // And from Stale it must still be closable, not wedged open forever.
        link.set_state(LinkState::Stale);
        let later = link.stale_time_secs() * 4 + 100;
        assert!(
            link.should_close(later),
            "a stale link established at clock second 0 must still close"
        );
    }

    #[test]
    fn test_should_send_keepalive() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);

        // Create initiator and responder
        let mut initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();
        let link_id = LinkId::new([0xAB; TRUNCATED_HASHBYTES]);

        // Not active yet - should not send
        assert!(!initiator.should_send_keepalive(1000));

        initiator.set_state(LinkState::Active);
        initiator.mark_established(1000);
        initiator.set_timing_for_test(10, initiator.stale_time_secs(), 1000);

        // Just established - should not send yet
        assert!(!initiator.should_send_keepalive(1005));

        // After keepalive interval - should send
        assert!(initiator.should_send_keepalive(1011));

        // Responder should never proactively send keepalives
        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        responder.set_state(LinkState::Active);
        responder.mark_established(1000);
        responder.set_timing_for_test(10, responder.stale_time_secs(), 1000);

        // Even after interval, responder should not send
        assert!(!responder.should_send_keepalive(2000));
    }

    #[test]
    fn test_is_stale_and_should_close() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        link.set_state(LinkState::Active);
        link.set_timing_for_test(10, 20, 1000); // stale after 20s of no inbound

        // Not stale yet
        assert!(!link.is_stale(1015));

        // Becomes stale after stale_time_secs
        assert!(link.is_stale(1021));

        // is_stale only works for Active state
        link.set_state(LinkState::Stale);
        assert!(!link.is_stale(1021));

        // should_close only works for Stale state
        // With RTT of 0, timeout = stale_time (20) + RTT*4 (0) + grace (5) = 25
        assert!(!link.should_close(1020)); // Not past stale_time yet from last_inbound
                                           // Total elapsed needs to be > stale_time + RTT*4 + grace = 20 + 0 + 5 = 25
        assert!(link.should_close(1026));
    }

    #[test]
    fn test_stale_close_timeout_secs() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        // stale_time=20s, RTT=0 -> 20 + 0*4 + grace(5) = 25
        link.set_timing_for_test(10, 20, 1000);
        assert_eq!(link.stale_close_timeout_secs(), 20 + LINK_STALE_GRACE_SECS);

        // RTT=3s -> 20 + 3*KEEPALIVE_TIMEOUT_FACTOR + grace
        link.set_rtt_ms(3000);
        assert_eq!(
            link.stale_close_timeout_secs(),
            20 + 3 * LINK_KEEPALIVE_TIMEOUT_FACTOR + LINK_STALE_GRACE_SECS
        );

        // The threshold must equal the boundary should_close() trips at.
        link.set_state(LinkState::Stale);
        let timeout = link.stale_close_timeout_secs();
        assert!(!link.should_close(1000 + timeout));
        assert!(link.should_close(1000 + timeout + 1));
    }

    #[test]
    fn test_keepalive_counters() {
        let (mut initiator, mut responder) = setup_active_link_pair();

        // Initiator sends keepalives; responder echoes.
        initiator.record_keepalive_sent(100);
        initiator.record_keepalive_sent(200);
        assert_eq!(initiator.keepalives_sent(), 2);
        assert_eq!(initiator.keepalives_acked(), 0);

        // Responder receives initiator keepalive, echoes back the responder byte.
        let init_ka = initiator.build_keepalive_packet().unwrap();
        let echo = init_ka.last().copied().unwrap();
        assert!(responder.process_keepalive(&[echo]).unwrap()); // responder echoes

        // Initiator processes the responder echo -> ack counter increments.
        let resp_ka = responder.build_keepalive_packet().unwrap();
        let resp_byte = resp_ka.last().copied().unwrap();
        assert!(!initiator.process_keepalive(&[resp_byte]).unwrap()); // initiator does not echo
        assert_eq!(initiator.keepalives_acked(), 1);
    }

    fn setup_active_link_pair() -> (Link, Link) {
        use crate::identity::Identity;

        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let dest_identity = Identity::generate(&mut OsRng);

        let mut initiator = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = initiator.create_link_request();

        let mut raw_packet = Vec::new();
        raw_packet.push(0x02);
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(dest_hash.as_bytes());
        raw_packet.push(0x00);
        raw_packet.extend_from_slice(&request_data);
        let link_id = Link::calculate_link_id(&raw_packet);
        initiator.set_link_id(link_id);

        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();

        let proof_packet = responder
            .build_proof_packet(&dest_identity, MTU as u32, 1)
            .unwrap();
        initiator
            .set_destination_keys(dest_identity.ed25519_verifying().as_bytes())
            .unwrap();
        initiator.process_proof(&proof_packet[19..]).unwrap();

        let rtt_packet = initiator.build_rtt_packet(0.05, &mut OsRng).unwrap();
        responder.process_rtt(&rtt_packet[19..]).unwrap();

        (initiator, responder)
    }

    #[test]
    fn test_build_keepalive_packet() {
        let (initiator, responder) = setup_active_link_pair();

        // Initiator builds keepalive with 0xFF
        let initiator_ka = initiator.build_keepalive_packet().unwrap();
        assert!(!initiator_ka.is_empty());
        // Packet: [flags(1)][hops(1)][link_id(16)][context(1)][payload(1)] = 20 bytes
        assert_eq!(initiator_ka.len(), 20);

        // Responder builds keepalive with 0xFE
        let responder_ka = responder.build_keepalive_packet().unwrap();
        assert!(!responder_ka.is_empty());
        assert_eq!(responder_ka.len(), 20);

        // Both should have Keepalive context
        use crate::packet::PacketContext;
        assert_eq!(initiator_ka[18], PacketContext::Keepalive.to_byte());
        assert_eq!(responder_ka[18], PacketContext::Keepalive.to_byte());

        // Verify raw payload bytes (not encrypted)
        assert_eq!(initiator_ka[19], KEEPALIVE_INITIATOR_BYTE);
        assert_eq!(responder_ka[19], KEEPALIVE_RESPONDER_BYTE);
    }

    #[test]
    fn test_process_keepalive() {
        let (mut initiator, mut responder) = setup_active_link_pair();

        // Initiator sends keepalive (0xFF)
        let initiator_ka = initiator.build_keepalive_packet().unwrap();
        let data = &initiator_ka[19..]; // Skip header

        // Responder processes it - should indicate to echo back
        let should_echo = responder.process_keepalive(data).unwrap();
        assert!(should_echo, "Responder should echo back keepalive");

        // Responder sends echo (0xFE)
        let responder_echo = responder.build_keepalive_packet().unwrap();
        let echo_data = &responder_echo[19..];

        // Initiator processes echo - should NOT indicate to echo back
        let should_echo = initiator.process_keepalive(echo_data).unwrap();
        assert!(!should_echo, "Initiator should not echo back");
    }

    #[test]
    fn test_process_keepalive_wrong_byte() {
        let (mut initiator, mut responder) = setup_active_link_pair();

        // Responder builds a keepalive (0xFE)
        let responder_ka = responder.build_keepalive_packet().unwrap();
        let data = &responder_ka[19..];

        // Responder tries to process its own type of keepalive (expects 0xFF, gets 0xFE)
        let result = responder.process_keepalive(data);
        assert!(result.is_err());

        // Similarly, initiator sends 0xFF
        let initiator_ka = initiator.build_keepalive_packet().unwrap();
        let data = &initiator_ka[19..];

        // Initiator tries to process its own type (expects 0xFE, gets 0xFF)
        let result = initiator.process_keepalive(data);
        assert!(result.is_err());
    }

    // ==================== CLOSE PACKET TESTS ====================

    #[test]
    fn test_build_close_packet() {
        let (initiator, _responder) = setup_active_link_pair();

        let close_packet = initiator.build_close_packet(&mut OsRng).unwrap();
        assert!(!close_packet.is_empty());

        // Should have LinkClose context
        use crate::packet::PacketContext;
        assert_eq!(close_packet[18], PacketContext::LinkClose.to_byte());
    }

    #[test]
    fn test_process_close_valid() {
        let (initiator, mut responder) = setup_active_link_pair();

        // Initiator builds close packet
        let close_packet = initiator.build_close_packet(&mut OsRng).unwrap();
        let encrypted_data = &close_packet[19..]; // Skip header

        // Responder processes it
        let result = responder.process_close(encrypted_data);
        assert!(result.is_ok());
        assert_eq!(responder.state(), LinkState::Closed);
    }

    #[test]
    fn test_process_close_invalid_link_id() {
        let (mut initiator, mut responder) = setup_active_link_pair();

        // Manually change initiator's link_id to produce invalid close packet
        let original_id = *initiator.id();
        initiator.id = LinkId::new([0xFF; TRUNCATED_HASHBYTES]);

        // Build close packet with wrong link_id
        let close_packet = initiator.build_close_packet(&mut OsRng).unwrap();
        let encrypted_data = &close_packet[19..];

        // Restore for proper decryption (we want to test link_id mismatch)
        initiator.id = original_id;

        // Responder processes it - should fail due to link_id mismatch
        let result = responder.process_close(encrypted_data);
        assert!(result.is_err());
        assert_eq!(responder.state(), LinkState::Active); // State unchanged
    }

    #[test]
    fn test_close_from_stale_state() {
        let (mut initiator, _responder) = setup_active_link_pair();

        // Transition to Stale
        initiator.set_state(LinkState::Stale);

        // Should still be able to build close packet from Stale state
        let result = initiator.build_close_packet(&mut OsRng);
        assert!(result.is_ok());
    }

    #[test]
    fn test_close_from_wrong_state() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);

        // Link is in Pending state
        let result = link.build_close_packet(&mut OsRng);
        assert!(matches!(result, Err(LinkError::InvalidState)));
    }

    #[test]
    fn test_link_state_transitions() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        assert_eq!(link.state(), LinkState::Pending);

        link.set_state(LinkState::Active);
        assert_eq!(link.state(), LinkState::Active);
        assert!(link.is_active());

        link.set_state(LinkState::Stale);
        assert_eq!(link.state(), LinkState::Stale);
        assert!(!link.is_active());

        link.close();
        assert_eq!(link.state(), LinkState::Closed);
    }

    #[test]
    fn test_record_timestamps() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);

        link.record_inbound(12345);
        link.record_outbound(12346);
        link.record_keepalive_sent(12347);
        link.mark_established(12340);

        // Just verify no panic - internal fields not exposed
        assert!(link.established_at.is_some());
    }

    // MTU Negotiation Tests
    #[test]
    fn test_decode_signaling_bytes_roundtrip() {
        // Round-trip: encode then decode must produce the same values
        let test_cases: &[(u32, u8)] = &[
            (500, 1),
            (262_144, 1),
            (1064, 1),
            (0, 0),
            (SIGNALING_MTU_MASK, 7), // max values
        ];
        for &(mtu, mode) in test_cases {
            let encoded = encode_signaling_bytes(mtu, mode);
            let (decoded_mtu, decoded_mode) = decode_signaling_bytes(&encoded);
            assert_eq!(decoded_mtu, mtu, "MTU roundtrip failed for {}", mtu);
            assert_eq!(decoded_mode, mode, "Mode roundtrip failed for {}", mode);
        }
    }

    #[test]
    fn test_decode_signaling_bytes_known_values() {
        // Zero signaling bytes → MTU=0, mode=0
        let (mtu, mode) = decode_signaling_bytes(&[0, 0, 0]);
        assert_eq!(mtu, 0);
        assert_eq!(mode, 0);

        // MTU=500 (0x1F4), mode=1 → signaling = (1 << 21) | 500 = 0x2001F4
        // Big-endian bytes: [0x20, 0x01, 0xF4]
        let encoded = encode_signaling_bytes(500, 1);
        assert_eq!(encoded, [0x20, 0x01, 0xF4]);
        let (mtu, mode) = decode_signaling_bytes(&encoded);
        assert_eq!(mtu, 500);
        assert_eq!(mode, 1);
    }

    #[test]
    fn test_compute_link_mdu_default() {
        // With default MTU=500, Python's formula gives 431
        // floor((500 - 1 - 19 - 48) / 16) * 16 - 1 = 27*16 - 1 = 431
        assert_eq!(compute_link_mdu(500), 431);
    }

    #[test]
    fn test_compute_link_mdu_large() {
        // With TCP HW_MTU=262144
        // floor((262144 - 1 - 19 - 48) / 16) * 16 - 1
        // = floor(262076 / 16) * 16 - 1
        // = 16379 * 16 - 1
        // = 262064 - 1 = 262063
        assert_eq!(compute_link_mdu(262_144), 262_063);
    }

    #[test]
    fn test_compute_link_mdu_udp() {
        // With UDP HW_MTU=1064
        // floor((1064 - 1 - 19 - 48) / 16) * 16 - 1
        // = floor(996 / 16) * 16 - 1
        // = 62 * 16 - 1
        // = 992 - 1 = 991
        assert_eq!(compute_link_mdu(1064), 991);
    }

    #[test]
    fn test_link_mdu_default() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);
        // Default negotiated_mtu is 500 → mdu() should return 431
        assert_eq!(link.mdu(), 431);
        assert_eq!(link.negotiated_mtu(), MTU as u32);
    }

    #[test]
    fn test_link_mdu_after_negotiation() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        // Simulate MTU negotiation
        link.negotiated_mtu = 262_144;
        assert_eq!(link.mdu(), 262_063);
    }

    #[test]
    fn test_new_incoming_with_signaling_bytes() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);

        // Build a 67-byte request with MTU=262144
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request_with_mtu(262_144, 1);
        assert_eq!(request_data.len(), LINK_REQUEST_SIGNALING_SIZE);

        let incoming =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        assert_eq!(incoming.negotiated_mtu(), 262_144);
        assert_eq!(incoming.mdu(), 262_063);
    }

    #[test]
    fn test_new_incoming_without_signaling_bytes() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);

        // Build a 64-byte request without signaling
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request();
        assert_eq!(request_data.len(), LINK_REQUEST_BASE_SIZE);

        let incoming =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        // No signaling → defaults to base MTU
        assert_eq!(incoming.negotiated_mtu(), MTU as u32);
        assert_eq!(incoming.mdu(), 431);
    }

    #[test]
    fn test_new_incoming_signaling_below_base_mtu() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);

        // Signaling with MTU=100 (below base MTU) → should clamp to 500
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request_with_mtu(100, 1);

        let incoming =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        assert_eq!(incoming.negotiated_mtu(), MTU as u32);
    }

    #[test]
    fn test_new_incoming_clamp_to_hw_mtu() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);

        // Signaled MTU=262144, but interface HW_MTU=1064 → clamp to 1064
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request_with_mtu(262_144, 1);

        let incoming =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, Some(1064)).unwrap();
        assert_eq!(incoming.negotiated_mtu(), 1064);
    }

    #[test]
    fn test_new_incoming_no_clamp_without_hw_mtu() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);

        // Signaled MTU=262144, no interface HW_MTU → keep 262144
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request_with_mtu(262_144, 1);

        let incoming =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        assert_eq!(incoming.negotiated_mtu(), 262_144);
    }

    #[test]
    fn test_new_incoming_no_upscale_from_hw_mtu() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);

        // Signaled MTU=500, interface HW_MTU=8192 → keep 500 (no upscale)
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request_with_mtu(500, 1);

        let incoming =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, Some(8192)).unwrap();
        assert_eq!(incoming.negotiated_mtu(), 500);
    }

    #[test]
    fn test_build_link_request_always_includes_signaling() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        let packet = link.build_link_request_packet(None);

        // Packet: flags(1) + hops(1) + dest_hash(16) + context(1) + data(67) = 86
        assert_eq!(
            packet.len(),
            1 + 1 + TRUNCATED_HASHBYTES + 1 + LINK_REQUEST_SIGNALING_SIZE
        );
    }

    #[test]
    fn test_build_link_request_with_hw_mtu() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        let packet = link.build_link_request_packet(Some(262_144));

        // Should still be 86 bytes (signaling always present)
        assert_eq!(
            packet.len(),
            1 + 1 + TRUNCATED_HASHBYTES + 1 + LINK_REQUEST_SIGNALING_SIZE
        );

        // Verify signaling bytes encode the HW_MTU
        let data_start = 1 + 1 + TRUNCATED_HASHBYTES + 1; // flags+hops+dest+context
        let sig_bytes: [u8; SIGNALING_SIZE] = packet
            [data_start + LINK_REQUEST_BASE_SIZE..data_start + LINK_REQUEST_SIGNALING_SIZE]
            .try_into()
            .unwrap();
        let (mtu, mode) = decode_signaling_bytes(&sig_bytes);
        assert_eq!(mtu, 262_144);
        assert_eq!(mode, crate::constants::MODE_AES256_CBC);
    }

    #[test]
    fn test_validate_mode_accepts_aes256_cbc() {
        assert!(validate_mode(crate::constants::MODE_AES256_CBC).is_ok());
    }

    #[test]
    fn test_validate_mode_rejects_unknown() {
        // MODE_AES128_CBC (0x00) is defined in Python but NOT enabled
        assert_eq!(validate_mode(0x00), Err(LinkError::UnsupportedMode));
        // MODE_AES256_GCM (0x02) is reserved
        assert_eq!(validate_mode(0x02), Err(LinkError::UnsupportedMode));
        // All other values
        for mode in [0x03, 0x04, 0x05, 0x06, 0x07] {
            assert_eq!(validate_mode(mode), Err(LinkError::UnsupportedMode));
        }
    }

    #[test]
    fn test_new_incoming_rejects_unsupported_mode() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);

        // Build request with unsupported mode (AES-128-CBC = 0x00)
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request_with_mtu(500, 0x00);

        let result = Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None);
        assert!(
            matches!(result, Err(LinkError::UnsupportedMode)),
            "expected UnsupportedMode, got {:?}",
            result.err()
        );
    }

    // ============ ESTABLISHMENT JITTER (Codeberg #129) ============

    /// Deterministic CryptoRngCore for jitter tests: a stepping counter, so
    /// consecutive draws differ predictably and the tests can never flake.
    struct SeqRng(u32);
    impl rand_core::RngCore for SeqRng {
        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_add(0x9E37_79B9);
            self.0
        }
        fn next_u64(&mut self) -> u64 {
            ((self.next_u32() as u64) << 32) | self.next_u32() as u64
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for b in dest.iter_mut() {
                *b = self.next_u32() as u8;
            }
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }
    impl rand_core::CryptoRng for SeqRng {}

    /// The jitter is ADDITIVE and bounded: the timeout never drops below the
    /// base formula and never exceeds base + MAX permille.
    #[test]
    fn establishment_jitter_is_bounded() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let base = 12_000u64; // 0 hops, no bitrate extra
        let max = base + base * crate::constants::MAX_ESTABLISHMENT_JITTER_PERMILLE as u64 / 1000;
        for _ in 0..200 {
            let link = Link::new_outgoing(dest_hash, &mut OsRng);
            let t = link.establishment_timeout_ms();
            assert!(t >= base, "jitter must never fire early: {t} < {base}");
            assert!(t <= max, "jitter above the +25% cap: {t} > {max}");
        }
    }

    /// The de-sync property (#129): two links drawn from one RNG get
    /// different jittered timeouts, so concurrent initiators drift apart.
    #[test]
    fn establishment_jitter_varies_between_links() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut rng = SeqRng(0);
        let a = Link::new_outgoing(dest_hash, &mut rng);
        let b = Link::new_outgoing(dest_hash, &mut rng);
        assert_ne!(
            a.establishment_timeout_ms(),
            b.establishment_timeout_ms(),
            "two concurrent initiators must not share a timeout"
        );
    }

    /// Each retry attempt de-syncs afresh: rerolling the jitter changes the
    /// timeout (within a bounded number of draws — a single draw may repeat).
    #[test]
    fn establishment_jitter_reroll_changes_timeout() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut rng = SeqRng(7);
        let mut link = Link::new_outgoing(dest_hash, &mut rng);
        let before = link.establishment_timeout_ms();
        let changed = (0..16).any(|_| {
            link.reroll_establishment_jitter(&mut rng);
            link.establishment_timeout_ms() != before
        });
        assert!(changed, "16 rerolls never changed the jittered timeout");
    }

    /// Python-parity base preserved: with the jitter forced to 0 the timeout
    /// is byte-identical to the pre-#129 formula, both branches.
    #[test]
    fn establishment_timeout_base_formula_with_zero_jitter() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        link.set_establishment_jitter_permille(0);
        assert_eq!(link.establishment_timeout_ms(), 12_000);
        link.set_hops(3);
        assert_eq!(link.establishment_timeout_ms(), 24_000);

        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request();
        let mut responder =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        responder.set_establishment_jitter_permille(0);
        assert_eq!(responder.establishment_timeout_ms(), 60_000);
    }

    #[test]
    fn establishment_timeout_initiator_0_hops() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        link.set_establishment_jitter_permille(0);
        assert_eq!(link.hops(), 0);
        // max(1,0)=1, (1+1)*6000 = 12000
        assert_eq!(link.establishment_timeout_ms(), 12_000);
    }

    #[test]
    fn establishment_timeout_initiator_3_hops() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        link.set_establishment_jitter_permille(0);
        link.set_hops(3);
        // (3+1)*6000 = 24000
        assert_eq!(link.establishment_timeout_ms(), 24_000);
    }

    #[test]
    fn establishment_timeout_responder_0_hops() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request();
        let mut link =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        link.set_establishment_jitter_permille(0);
        assert!(!link.is_initiator());
        assert_eq!(link.hops(), 0);
        // max(1,0)*6000 + 54000 = 60000
        assert_eq!(link.establishment_timeout_ms(), 60_000);
    }

    #[test]
    fn establishment_timeout_responder_3_hops() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link_id = LinkId::new([0x01; TRUNCATED_HASHBYTES]);
        let outgoing = Link::new_outgoing(dest_hash, &mut OsRng);
        let request_data = outgoing.create_link_request();
        let mut link =
            Link::new_incoming(&request_data, link_id, dest_hash, &mut OsRng, None).unwrap();
        link.set_establishment_jitter_permille(0);
        link.set_hops(3);
        // 3*6000 + 54000 = 72000
        assert_eq!(link.establishment_timeout_ms(), 72_000);
    }

    #[test]
    fn establishment_timeout_initiator_with_lora_bitrate() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        link.set_establishment_jitter_permille(0);
        // SF10/BW125k ≈ 976 bps
        link.set_first_hop_timeout_from_bitrate(976);
        // first_hop_extra = 500 * 8 * 1000 / 976 = 4098 ms
        // total = 4098 + 6000 * (max(1,0) + 1) = 4098 + 12000 = 16098
        assert_eq!(link.establishment_timeout_ms(), 16_098);
    }

    #[test]
    fn establishment_timeout_initiator_lora_3_hops() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        link.set_establishment_jitter_permille(0);
        link.set_hops(3);
        link.set_first_hop_timeout_from_bitrate(976);
        // 4098 + 6000 * (3+1) = 4098 + 24000 = 28098
        assert_eq!(link.establishment_timeout_ms(), 28_098);
    }

    #[test]
    fn first_hop_timeout_zero_bitrate_is_noop() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        link.set_establishment_jitter_permille(0);
        link.set_first_hop_timeout_from_bitrate(0);
        // No extra timeout added
        assert_eq!(link.establishment_timeout_ms(), 12_000);
    }

    #[test]
    fn establishment_timeout_unknown_bitrate_2_hops() {
        // Simulates a client connecting via TCP to a transport daemon that
        // routes through LoRa. The client doesn't know the next-hop bitrate,
        // so connect() uses UNKNOWN_BITRATE_ASSUMPTION_BPS (300 bps).
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
        link.set_establishment_jitter_permille(0);
        link.set_hops(2);
        link.set_first_hop_timeout_from_bitrate(crate::constants::UNKNOWN_BITRATE_ASSUMPTION_BPS);
        // first_hop_extra = 500 * 8 * 1000 / 300 = 13333 ms
        // total = 13333 + 6000 * (2+1) = 13333 + 18000 = 31333
        assert_eq!(link.establishment_timeout_ms(), 31_333);
    }

    // ==================== RAW DATA PACKET TESTS ====================

    #[test]
    fn test_build_raw_data_packet() {
        let (initiator, _responder) = setup_active_link_pair();

        let payload = b"raw resource data";
        let packet = initiator
            .build_raw_data_packet(payload, PacketContext::Resource)
            .unwrap();

        // [flags (1)] [hops (1)] [link_id (16)] [context (1)] [raw_data]
        assert_eq!(
            packet.len(),
            1 + 1 + TRUNCATED_HASHBYTES + 1 + payload.len()
        );

        // dest_type=Link=0b11, packet_type=Data=0b00 -> bits 3-0 = 0x0C
        // context_flag=true -> bit 5 = 0x20
        assert_eq!(packet[0] & 0x0F, 0x0C);
        assert_ne!(packet[0] & 0x20, 0); // context flag set

        // Hops = 0
        assert_eq!(packet[1], 0x00);

        // Link ID
        assert_eq!(&packet[2..18], initiator.id().as_bytes());

        // Context byte = RESOURCE = 0x01
        assert_eq!(packet[18], PacketContext::Resource.to_byte());

        // Payload is raw (NOT encrypted)
        assert_eq!(&packet[19..], payload);
    }

    #[test]
    fn test_build_raw_data_packet_not_active() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);

        let result = link.build_raw_data_packet(b"data", PacketContext::Resource);
        assert!(matches!(result, Err(LinkError::InvalidState)));
    }

    #[test]
    fn test_build_raw_data_packet_no_context() {
        let (initiator, _responder) = setup_active_link_pair();

        let packet = initiator
            .build_raw_data_packet(b"data", PacketContext::None)
            .unwrap();

        // context_flag should be false when context is None
        assert_eq!(packet[0] & 0x20, 0);
        assert_eq!(packet[18], PacketContext::None.to_byte());
    }

    // ==================== PROOF PACKET WITH CONTEXT TESTS ====================

    #[test]
    fn test_build_proof_packet_with_context() {
        let (initiator, _responder) = setup_active_link_pair();

        let proof_data = b"resource_hash_plus_proof_hash____"; // 32 bytes
        let packet = initiator
            .build_proof_packet_with_context(proof_data, PacketContext::ResourcePrf)
            .unwrap();

        // [flags (1)] [hops (1)] [link_id (16)] [context (1)] [proof_data]
        assert_eq!(
            packet.len(),
            1 + 1 + TRUNCATED_HASHBYTES + 1 + proof_data.len()
        );

        // dest_type=Link=0b11, packet_type=Proof=0b11 -> bits 3-0 = 0x0F
        assert_eq!(packet[0] & 0x0F, 0x0F);

        // context_flag should be set
        assert_ne!(packet[0] & 0x20, 0);

        // Link ID
        assert_eq!(&packet[2..18], initiator.id().as_bytes());

        // Context = ResourcePrf = 0x05
        assert_eq!(packet[18], PacketContext::ResourcePrf.to_byte());

        // Payload is raw
        assert_eq!(&packet[19..], proof_data);
    }

    #[test]
    fn test_build_proof_packet_with_context_not_active() {
        let dest_hash = DestinationHash::new([0x42; TRUNCATED_HASHBYTES]);
        let link = Link::new_outgoing(dest_hash, &mut OsRng);

        let result = link.build_proof_packet_with_context(b"data", PacketContext::ResourcePrf);
        assert!(matches!(result, Err(LinkError::InvalidState)));
    }

    // LinkIdentify tests
    #[test]
    fn test_link_identify_roundtrip() {
        use crate::identity::Identity;

        let (initiator, responder) = setup_active_link_pair();
        let link_id = *initiator.id();

        // Create an identity to identify with
        let identity = Identity::generate(&mut OsRng);
        let public_key = identity.public_key_bytes();

        // Build signed_data and signature
        let mut signed_data = [0u8; 80];
        signed_data[..16].copy_from_slice(link_id.as_bytes());
        signed_data[16..80].copy_from_slice(&public_key);
        let signature = identity.sign(&signed_data).unwrap();

        // Build proof payload
        let mut proof = [0u8; 128];
        proof[..64].copy_from_slice(&public_key);
        proof[64..].copy_from_slice(&signature);

        // Encrypt with initiator's link key
        let packet = initiator
            .build_data_packet_with_context(&proof, PacketContext::LinkIdentify, &mut OsRng)
            .unwrap();

        // Packet format: [flags(1)][hops(1)][link_id(16)][context(1)][encrypted]
        let encrypted_data = &packet[19..];

        // Decrypt with responder's link key
        let mut plaintext = vec![0u8; encrypted_data.len()];
        let len = responder.decrypt(encrypted_data, &mut plaintext).unwrap();
        plaintext.truncate(len);

        // Verify length and contents
        assert_eq!(len, 128);
        assert_eq!(&plaintext[..64], &public_key);
        assert_eq!(&plaintext[64..], &signature);

        // Verify signature
        let peer = Identity::from_public_key_bytes(&plaintext[..64]).unwrap();
        let mut check_data = [0u8; 80];
        check_data[..16].copy_from_slice(link_id.as_bytes());
        check_data[16..80].copy_from_slice(&plaintext[..64]);
        assert!(peer.verify(&check_data, &plaintext[64..]).unwrap());
        assert_eq!(peer.hash(), identity.hash());
    }

    #[test]
    fn test_link_identify_invalid_signature_rejected() {
        use crate::identity::Identity;

        let (initiator, responder) = setup_active_link_pair();
        let link_id = *initiator.id();

        // Create two identities, sign with one, claim to be the other
        let real_identity = Identity::generate(&mut OsRng);
        let fake_identity = Identity::generate(&mut OsRng);

        let public_key = fake_identity.public_key_bytes();

        // Sign with the REAL identity but include FAKE public key
        let mut signed_data = [0u8; 80];
        signed_data[..16].copy_from_slice(link_id.as_bytes());
        signed_data[16..80].copy_from_slice(&public_key);
        let signature = real_identity.sign(&signed_data).unwrap();

        let mut proof = [0u8; 128];
        proof[..64].copy_from_slice(&public_key);
        proof[64..].copy_from_slice(&signature);

        // Encrypt and transmit
        let packet = initiator
            .build_data_packet_with_context(&proof, PacketContext::LinkIdentify, &mut OsRng)
            .unwrap();
        let encrypted_data = &packet[19..];

        // Responder decrypts
        let mut plaintext = vec![0u8; encrypted_data.len()];
        let len = responder.decrypt(encrypted_data, &mut plaintext).unwrap();
        assert_eq!(len, 128);

        // Verify that the signature does NOT validate against the claimed identity
        let claimed = Identity::from_public_key_bytes(&plaintext[..64]).unwrap();
        let mut check_data = [0u8; 80];
        check_data[..16].copy_from_slice(link_id.as_bytes());
        check_data[16..80].copy_from_slice(&plaintext[..64]);
        // The signature was made by real_identity, not fake_identity, so verification fails
        assert!(!claimed.verify(&check_data, &plaintext[64..]).unwrap());
    }

    #[test]
    fn test_link_identify_wrong_length_rejected() {
        let (initiator, responder) = setup_active_link_pair();

        // Encrypt a 100-byte payload (not 128)
        let bad_payload = [0x42u8; 100];
        let packet = initiator
            .build_data_packet_with_context(&bad_payload, PacketContext::LinkIdentify, &mut OsRng)
            .unwrap();
        let encrypted_data = &packet[19..];

        // Responder can decrypt it...
        let mut plaintext = vec![0u8; encrypted_data.len()];
        let len = responder.decrypt(encrypted_data, &mut plaintext).unwrap();

        // ...but the length is wrong (not 128)
        assert_eq!(len, 100);
        assert_ne!(len, 128);
    }

    #[test]
    fn test_link_remote_identity_accessors() {
        use crate::identity::Identity;

        let (_, mut responder) = setup_active_link_pair();
        assert!(responder.remote_identity().is_none());

        let identity = Identity::generate(&mut OsRng);
        let hash = *identity.hash();
        responder.set_remote_identity(identity);

        assert!(responder.remote_identity().is_some());
        assert_eq!(responder.remote_identity().unwrap().hash(), &hash);
    }

    #[test]
    fn test_cached_resource_proof_roundtrip() {
        let (_, mut responder) = setup_active_link_pair();

        let proof_data = vec![0xAA; 64];
        let proof_pkt = responder
            .build_proof_packet_with_context(&proof_data, crate::packet::PacketContext::ResourcePrf)
            .unwrap();
        let ph = crate::packet::packet_hash(&proof_pkt);

        responder.cache_resource_proof(ph, proof_pkt.clone());

        // Matching hash returns cached proof
        let cached = responder.get_cached_resource_proof(&ph);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap(), &proof_pkt);

        // Non-matching hash returns None
        let unknown = [0xFF; 32];
        assert!(responder.get_cached_resource_proof(&unknown).is_none());
    }

    #[test]
    fn test_proof_packet_determinism() {
        // Both initiator and responder must produce identical proof packets
        // for the same proof_data and link_id, so the sender can reconstruct
        // the expected packet_hash for a CacheRequest.
        let (initiator, responder) = setup_active_link_pair();

        let proof_data = vec![0xBB; 64];

        let pkt_initiator = initiator
            .build_proof_packet_with_context(&proof_data, crate::packet::PacketContext::ResourcePrf)
            .unwrap();
        let pkt_responder = responder
            .build_proof_packet_with_context(&proof_data, crate::packet::PacketContext::ResourcePrf)
            .unwrap();

        // Both sides share the same link_id, so the packets (and their hashes) must match
        let hash_init = crate::packet::packet_hash(&pkt_initiator);
        let hash_resp = crate::packet::packet_hash(&pkt_responder);
        assert_eq!(
            hash_init, hash_resp,
            "proof packet_hash must be deterministic across link sides"
        );
        assert_eq!(
            pkt_initiator, pkt_responder,
            "raw proof packets must be identical"
        );
    }
}
