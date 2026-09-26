//! Announce message handling
//!
//! Announces are broadcast messages that inform the network about a destination.
//! They contain the destination's public key, name hash, and signature.
//!
//! # Announce Format (without ratchet)
//!
//! | Offset | Size | Field          |
//! |--------|------|----------------|
//! | 0      | 64   | public_key     | (32 bytes X25519 + 32 bytes Ed25519)
//! | 64     | 10   | name_hash      |
//! | 74     | 10   | random_hash    |
//! | 84     | 64   | signature      |
//! | 148    | var  | app_data       | (optional)
//!
//! # Announce Format (with ratchet)
//!
//! | Offset | Size | Field          |
//! |--------|------|----------------|
//! | 0      | 64   | public_key     |
//! | 64     | 10   | name_hash      |
//! | 74     | 10   | random_hash    |
//! | 84     | 32   | ratchet        |
//! | 116    | 64   | signature      |
//! | 180    | var  | app_data       | (optional)
//!
//! The signature covers: destination_hash + public_key + name_hash + random_hash + \[ratchet\] + app_data

use crate::constants::{
    slice_to_array, ED25519_SIGNATURE_SIZE, EMISSION_TIMESTAMP_MAX_SECS, HEADER_MINSIZE,
    IDENTITY_KEY_SIZE, IFAC_MIN_SIZE, MTU, NAME_HASHBYTES, RANDOM_HASHBYTES,
    RANDOM_HASH_RANDOM_SIZE, RANDOM_HASH_TIMESTAMP_OFFSET, RANDOM_HASH_TIMESTAMP_SIZE,
    RATCHET_SIZE, TRUNCATED_HASHBYTES,
};
use crate::destination::DestinationHash;

// Announce Format Offsets
// These define the byte layout of announce payloads.

/// Offset of public key in announce payload
const OFFSET_PUBLIC_KEY: usize = 0;
/// Offset of name hash in announce payload
const OFFSET_NAME_HASH: usize = IDENTITY_KEY_SIZE; // 64
/// Offset of random hash in announce payload
const OFFSET_RANDOM_HASH: usize = OFFSET_NAME_HASH + NAME_HASHBYTES; // 74
/// Offset of ratchet in ratcheted announces (or signature in non-ratcheted)
const OFFSET_RATCHET_OR_SIG: usize = OFFSET_RANDOM_HASH + RANDOM_HASHBYTES; // 84
/// Offset of signature in ratcheted announces
const OFFSET_SIG_RATCHETED: usize = OFFSET_RATCHET_OR_SIG + RATCHET_SIZE; // 116
/// Offset of app_data in non-ratcheted announces
const OFFSET_APP_DATA: usize = OFFSET_RATCHET_OR_SIG + ED25519_SIGNATURE_SIZE; // 148
/// Offset of app_data in ratcheted announces
const OFFSET_APP_DATA_RATCHETED: usize = OFFSET_SIG_RATCHETED + ED25519_SIGNATURE_SIZE; // 180

/// Fixed (non-`app_data`) length of an announce payload, in bytes.
///
/// This is exactly the `app_data` offset: everything ahead of `app_data` is
/// fixed-size (public key, name hash, random hash, optional ratchet, signature).
pub const fn announce_payload_fixed_len(with_ratchet: bool) -> usize {
    if with_ratchet {
        OFFSET_APP_DATA_RATCHETED // 180
    } else {
        OFFSET_APP_DATA // 148
    }
}

/// Bytes of `app_data` an announce can carry before it overruns the MTU.
///
/// The rest of the MTU is consumed by the packet header, the identity public
/// key, the name and random hashes, the optional ratchet, the signature, and a
/// reserved [`IFAC_MIN_SIZE`] byte for interface access codes:
///
/// ```text
/// MTU(500) − IFAC_MIN_SIZE(1) − HEADER_MINSIZE(19) − fixed_payload
///   = 332 bytes  (no ratchet, fixed_payload = 148)
///   = 300 bytes  (with ratchet, fixed_payload = 180)
/// ```
///
/// Announces always use a Type 1 header (they carry no transport ID), so
/// [`HEADER_MINSIZE`] is the exact header cost, not a lower bound.
///
/// Callers should gate on this at **compose** time. An announce that exceeds it
/// is refused by [`crate::destination::Destination::announce`] with
/// [`AnnounceError::PacketTooLarge`], which reports the byte counts.
///
/// ```
/// # use leviculum_core::announce_app_data_budget;
/// assert_eq!(announce_app_data_budget(false), 332);
/// assert_eq!(announce_app_data_budget(true), 300);
/// ```
pub const fn announce_app_data_budget(with_ratchet: bool) -> usize {
    MTU - IFAC_MIN_SIZE - HEADER_MINSIZE - announce_payload_fixed_len(with_ratchet)
}

use crate::crypto::truncated_hash;
use crate::identity::{Identity, IdentityError};
use crate::packet::{Packet, PacketType};

use alloc::vec::Vec;
use rand_core::CryptoRngCore;

/// Build the signed data for announce signature creation and verification.
///
/// The signature covers: `destination_hash + public_key + name_hash + random_hash + [ratchet] + app_data`
///
/// This is used both when creating announces (in `build_announce_payload`) and
/// when verifying them (in `ReceivedAnnounce::verify_signature`).
fn build_signed_data(
    destination_hash: &[u8; TRUNCATED_HASHBYTES],
    public_key: &[u8; IDENTITY_KEY_SIZE],
    name_hash: &[u8; NAME_HASHBYTES],
    random_hash: &[u8; RANDOM_HASHBYTES],
    ratchet: Option<&[u8; RATCHET_SIZE]>,
    app_data: &[u8],
) -> Vec<u8> {
    let ratchet_len = if ratchet.is_some() { RATCHET_SIZE } else { 0 };
    let mut data = Vec::with_capacity(
        TRUNCATED_HASHBYTES
            + IDENTITY_KEY_SIZE
            + NAME_HASHBYTES
            + RANDOM_HASHBYTES
            + ratchet_len
            + app_data.len(),
    );
    data.extend_from_slice(destination_hash);
    data.extend_from_slice(public_key);
    data.extend_from_slice(name_hash);
    data.extend_from_slice(random_hash);
    if let Some(r) = ratchet {
        data.extend_from_slice(r);
    }
    data.extend_from_slice(app_data);
    data
}

/// Minimum announce payload size (without ratchet, without app_data)
/// public_key(64) + name_hash(10) + random_hash(10) + signature(64) = 148
pub(crate) const ANNOUNCE_MIN_SIZE: usize =
    IDENTITY_KEY_SIZE + NAME_HASHBYTES + RANDOM_HASHBYTES + ED25519_SIGNATURE_SIZE;

/// Minimum announce payload size with ratchet (without app_data)
/// public_key(64) + name_hash(10) + random_hash(10) + ratchet(32) + signature(64) = 180
pub(crate) const ANNOUNCE_RATCHETED_MIN_SIZE: usize = ANNOUNCE_MIN_SIZE + RATCHET_SIZE;

/// Generate a random hash for announces (5 random + 5 timestamp bytes).
///
/// The random hash ensures announce uniqueness even for the same destination.
/// Format: 5 bytes from truncated_hash(random_16) + 5 bytes from timestamp (seconds).
///
/// `emission_secs` must be unix seconds (Python writes
/// `int(time.time()).to_bytes(5, "big")`, Destination.py:282) — peers order
/// same-destination paths by this value across our restarts
/// (Transport.py:1772/1809), so a process-relative timebase breaks path
/// replacement (Codeberg #155). Callers obtain it from
/// `Transport::emission_secs`, never from the monotonic `Clock::now_ms`.
pub(crate) fn generate_random_hash(
    rng: &mut impl CryptoRngCore,
    emission_secs: u64,
) -> [u8; RANDOM_HASHBYTES] {
    let mut random_16 = [0u8; 16];
    rng.fill_bytes(&mut random_16);
    let random_part = truncated_hash(&random_16);

    // The field keeps only 8*RANDOM_HASH_TIMESTAMP_SIZE bits; a plain copy
    // of anything larger drops the high bits and emits a near-zero value
    // that sorts below every stored path entry (Codeberg #160).
    let timestamp_bytes = emission_secs.min(EMISSION_TIMESTAMP_MAX_SECS).to_be_bytes();

    let mut result = [0u8; RANDOM_HASHBYTES]; // 10 bytes
    result[..RANDOM_HASH_RANDOM_SIZE].copy_from_slice(&random_part[..RANDOM_HASH_RANDOM_SIZE]);
    let ts_end = RANDOM_HASH_TIMESTAMP_OFFSET + RANDOM_HASH_TIMESTAMP_SIZE;
    result[RANDOM_HASH_RANDOM_SIZE..]
        .copy_from_slice(&timestamp_bytes[RANDOM_HASH_TIMESTAMP_OFFSET..ts_end]);
    result
}

/// Extract the emission timebase from a random_hash.
///
/// The random_hash is 10 bytes: 5 random + 5 timestamp. This reads bytes 5..10
/// as a big-endian integer, matching Python's `int.from_bytes(random_blob[5:10], "big")`
/// (Transport.py:2935-2936). The result is a 40-bit value used only for
/// relative comparison, a truncated seconds-since-epoch timestamp.
pub(crate) fn emission_from_random_hash(random_hash: &[u8; RANDOM_HASHBYTES]) -> u64 {
    let ts = &random_hash[RANDOM_HASH_RANDOM_SIZE..];
    // Read 5 bytes big-endian into u64
    ((ts[0] as u64) << 32)
        | ((ts[1] as u64) << 24)
        | ((ts[2] as u64) << 16)
        | ((ts[3] as u64) << 8)
        | (ts[4] as u64)
}

/// Get the maximum emission timestamp from a list of random_hashes.
///
/// Matches Python `Transport.timebase_from_random_blobs()` (Transport.py:3182-3186).
pub(crate) fn max_emission_from_blobs(blobs: &[[u8; RANDOM_HASHBYTES]]) -> u64 {
    blobs
        .iter()
        .map(emission_from_random_hash)
        .max()
        .unwrap_or(0)
}

/// Build an announce payload (internal helper for Destination::announce).
///
/// # Payload format (without ratchet)
/// `public_key(64) + name_hash(10) + random_hash(10) + signature(64) + app_data`
///
/// # Payload format (with ratchet)
/// `public_key(64) + name_hash(10) + random_hash(10) + ratchet(32) + signature(64) + app_data`
///
/// # Signature
/// The signature covers: `destination_hash + public_key + name_hash + random_hash + [ratchet] + app_data`
pub(crate) fn build_announce_payload(
    identity: &Identity,
    destination_hash: &[u8; TRUNCATED_HASHBYTES],
    name_hash: &[u8; NAME_HASHBYTES],
    ratchet: Option<&[u8; RATCHET_SIZE]>,
    app_data: Option<&[u8]>,
    rng: &mut impl CryptoRngCore,
    emission_secs: u64,
) -> Result<Vec<u8>, AnnounceError> {
    let public_key = identity.public_key_bytes();
    let random_hash = generate_random_hash(rng, emission_secs);
    let app_data_bytes = app_data.unwrap_or(&[]);

    // Build signed data using helper
    let signed_data = build_signed_data(
        destination_hash,
        &public_key,
        name_hash,
        &random_hash,
        ratchet,
        app_data_bytes,
    );

    let signature = identity
        .sign(&signed_data)
        .map_err(|_| AnnounceError::SigningFailed)?;

    // Build payload: public_key + name_hash + random_hash + [ratchet] + signature + app_data
    let payload_size = if ratchet.is_some() {
        ANNOUNCE_RATCHETED_MIN_SIZE + app_data_bytes.len()
    } else {
        ANNOUNCE_MIN_SIZE + app_data_bytes.len()
    };
    let mut payload = Vec::with_capacity(payload_size);
    payload.extend_from_slice(&public_key);
    payload.extend_from_slice(name_hash);
    payload.extend_from_slice(&random_hash);
    if let Some(r) = ratchet {
        payload.extend_from_slice(r);
    }
    payload.extend_from_slice(&signature);
    payload.extend_from_slice(app_data_bytes);

    Ok(payload)
}

/// Error type for announce operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceError {
    /// Payload too short
    PayloadTooShort,
    /// Not an announce packet
    NotAnnounce,
    /// Invalid public key
    InvalidPublicKey,
    /// Signature verification failed
    InvalidSignature,
    /// Destination hash mismatch
    HashMismatch,
    /// Failed to sign announce (no private key)
    SigningFailed,
    /// Destination has no identity (PLAIN destination)
    NoIdentity,
    /// OUT destinations cannot announce (only IN)
    WrongDirection,
    /// Only SINGLE destinations can announce
    OnlySingleCanAnnounce,
    /// The announce does not fit the MTU, because `app_data` overran the
    /// budget reported by [`announce_app_data_budget`].
    ///
    /// Carries the numbers the caller needs to act: how large the announce
    /// would have been, the MTU it had to fit, how many `app_data` bytes were
    /// supplied, and how many were allowed.
    PacketTooLarge {
        /// Wire length the announce packet would have had, in bytes.
        packed: usize,
        /// The MTU it had to fit within, in bytes.
        mtu: usize,
        /// `app_data` bytes supplied by the caller.
        app_data: usize,
        /// `app_data` bytes actually available — [`announce_app_data_budget`].
        budget: usize,
    },
    /// Packing the announce packet failed for a reason other than size.
    Pack(crate::packet::PacketError),
    /// Destination not registered
    DestinationNotFound,
    /// Destination carries an explicit (override) hash and must never be
    /// announced. Announcing it would emit a destination_hash that does not
    /// match `truncated_hash(name_hash || identity_hash)`, which every
    /// Python-RNS peer recomputes and rejects — a wire/semantic break.
    /// Explicit-hash destinations are reachable only via direct link
    /// (the link path carries an opaque hash), never via the announce stream.
    ExplicitHashCannotAnnounce,
}

impl core::fmt::Display for AnnounceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AnnounceError::PayloadTooShort => write!(f, "Announce payload too short"),
            AnnounceError::NotAnnounce => write!(f, "Not an announce packet"),
            AnnounceError::InvalidPublicKey => write!(f, "Invalid public key in announce"),
            AnnounceError::InvalidSignature => write!(f, "Signature verification failed"),
            AnnounceError::HashMismatch => write!(f, "Destination hash mismatch"),
            AnnounceError::SigningFailed => write!(f, "Failed to sign announce"),
            AnnounceError::NoIdentity => write!(f, "Destination has no identity"),
            AnnounceError::WrongDirection => write!(f, "OUT destinations cannot announce"),
            AnnounceError::OnlySingleCanAnnounce => {
                write!(f, "Only SINGLE destinations can announce")
            }
            AnnounceError::PacketTooLarge {
                packed,
                mtu,
                app_data,
                budget,
            } => write!(
                f,
                "Announce is {packed} B against a {mtu} B MTU: app_data is {app_data} B of a {budget} B budget ({} B over)",
                app_data.saturating_sub(*budget)
            ),
            AnnounceError::Pack(e) => write!(f, "Failed to pack announce packet: {e}"),
            AnnounceError::DestinationNotFound => {
                write!(f, "Destination not registered on this node")
            }
            AnnounceError::ExplicitHashCannotAnnounce => {
                write!(f, "Explicit-hash destinations cannot be announced")
            }
        }
    }
}

/// Policy hook controlling which destinations are emitted on the mesh-wide
/// announce stream.
///
/// Install one with [`crate::node::NodeCore::set_announce_control`]. The node
/// consults it on every scheduled announce (the periodic management tick and
/// the interface-recovery re-announce) before emitting; a destination for
/// which [`should_suppress_announce`](AnnounceControl::should_suppress_announce)
/// returns `true` stays fully **routable** (it remains registered, links and
/// data still reach it) but is never gossiped.
///
/// This is a pure local policy — it does not change the announce wire format.
/// With no policy installed the node announces everything, the historical
/// default. The intended consumer is membership-privacy: group-scoped
/// destinations whose announce would leak a membership delta to mesh
/// observers are suppressed, while peers learn them out-of-band.
///
/// The hook is consulted synchronously inside the node's single-threaded
/// event loop, so implementations must not block; `should_suppress_announce`
/// is expected to be a fast lookup.
pub trait AnnounceControl: Send + Sync {
    /// Return `true` to suppress mesh-wide announces for this destination.
    fn should_suppress_announce(&self, destination_hash: &DestinationHash) -> bool;
}

/// A received announce message parsed from a packet
///
/// This struct holds the parsed components of an announce and provides
/// methods to verify the signature and compute hashes.
///
/// `Clone` exists because [`crate::NodeEvent`] is `Clone` and carries one.
#[derive(Clone)]
pub struct ReceivedAnnounce {
    /// Destination hash from packet header
    destination_hash: DestinationHash,
    /// Combined public key (32 bytes X25519 + 32 bytes Ed25519)
    public_key: [u8; IDENTITY_KEY_SIZE],
    /// Name hash (truncated hash of app_name.aspects)
    name_hash: [u8; NAME_HASHBYTES],
    /// Random hash (for announce uniqueness)
    random_hash: [u8; RANDOM_HASHBYTES],
    /// Ratchet key (if present)
    ratchet: Option<[u8; RATCHET_SIZE]>,
    /// Ed25519 signature
    signature: [u8; ED25519_SIGNATURE_SIZE],
    /// Application-specific data
    app_data: Vec<u8>,
}

impl ReceivedAnnounce {
    /// Parse an announce from a packet
    ///
    /// Returns an error if the packet is not an announce or the payload is malformed.
    ///
    /// # Ratchet Detection
    ///
    /// The `context_flag` in the packet header indicates whether a ratchet is present.
    /// This is the authoritative indicator per the Reticulum protocol specification.
    pub(crate) fn from_packet(packet: &Packet) -> Result<Self, AnnounceError> {
        if packet.flags.packet_type != PacketType::Announce {
            return Err(AnnounceError::NotAnnounce);
        }

        let payload = packet.data.as_slice();

        // Use context_flag as the authoritative indicator for ratchet presence
        let has_ratchet = packet.flags.context_flag;

        // Check minimum size based on announce type
        let min_size = if has_ratchet {
            ANNOUNCE_RATCHETED_MIN_SIZE
        } else {
            ANNOUNCE_MIN_SIZE
        };
        if payload.len() < min_size {
            return Err(AnnounceError::PayloadTooShort);
        }

        // Parse common fields (same offsets for both formats)
        let public_key = slice_to_array(payload, OFFSET_PUBLIC_KEY);
        let name_hash = slice_to_array(payload, OFFSET_NAME_HASH);
        let random_hash = slice_to_array(payload, OFFSET_RANDOM_HASH);

        // Parse ratchet, signature, and app_data based on format
        let (ratchet, signature, app_data_offset) = if has_ratchet {
            (
                Some(slice_to_array(payload, OFFSET_RATCHET_OR_SIG)),
                slice_to_array(payload, OFFSET_SIG_RATCHETED),
                OFFSET_APP_DATA_RATCHETED,
            )
        } else {
            (
                None,
                slice_to_array(payload, OFFSET_RATCHET_OR_SIG),
                OFFSET_APP_DATA,
            )
        };

        Ok(Self {
            destination_hash: DestinationHash::new(packet.destination_hash),
            public_key,
            name_hash,
            random_hash,
            ratchet,
            signature,
            app_data: payload[app_data_offset..].to_vec(),
        })
    }

    /// Get the destination hash from the packet header
    pub fn destination_hash(&self) -> &DestinationHash {
        &self.destination_hash
    }

    /// Get the combined public key (X25519 + Ed25519)
    pub fn public_key(&self) -> &[u8; IDENTITY_KEY_SIZE] {
        &self.public_key
    }

    /// Get the name hash
    pub fn name_hash(&self) -> &[u8; NAME_HASHBYTES] {
        &self.name_hash
    }

    /// Get the random hash
    pub fn random_hash(&self) -> &[u8; RANDOM_HASHBYTES] {
        &self.random_hash
    }

    /// Get the ratchet key if present
    pub fn ratchet(&self) -> Option<&[u8; RATCHET_SIZE]> {
        self.ratchet.as_ref()
    }

    /// Check if this is a ratcheted announce
    pub fn has_ratchet(&self) -> bool {
        self.ratchet.is_some()
    }

    /// Get the signature
    pub fn signature(&self) -> &[u8; ED25519_SIGNATURE_SIZE] {
        &self.signature
    }

    /// Get the app_data
    pub fn app_data(&self) -> &[u8] {
        &self.app_data
    }

    /// Get app_data as a string if valid UTF-8
    pub fn app_data_string(&self) -> Option<&str> {
        core::str::from_utf8(&self.app_data).ok()
    }

    /// Compute the identity hash from the public key
    ///
    /// identity_hash = truncated_hash(public_key)
    pub fn computed_identity_hash(&self) -> [u8; TRUNCATED_HASHBYTES] {
        truncated_hash(&self.public_key)
    }

    /// Compute the destination hash from name_hash and identity_hash
    ///
    /// dest_hash = truncated_hash(name_hash + identity_hash)
    pub fn computed_destination_hash(&self) -> DestinationHash {
        let identity_hash = self.computed_identity_hash();
        let mut hash_material = [0u8; NAME_HASHBYTES + TRUNCATED_HASHBYTES];
        hash_material[..NAME_HASHBYTES].copy_from_slice(&self.name_hash);
        hash_material[NAME_HASHBYTES..].copy_from_slice(&identity_hash);
        DestinationHash::new(truncated_hash(&hash_material))
    }

    /// Verify that the destination hash in the packet matches the computed hash
    pub fn verify_destination_hash(&self) -> bool {
        self.destination_hash == self.computed_destination_hash()
    }

    /// Compute the signed data for signature verification
    ///
    /// Python RNS signs: destination_hash + public_key + name_hash + random_hash + [ratchet] + app_data
    fn signed_data(&self) -> Vec<u8> {
        build_signed_data(
            self.destination_hash.as_bytes(),
            &self.public_key,
            &self.name_hash,
            &self.random_hash,
            self.ratchet.as_ref(),
            &self.app_data,
        )
    }

    /// Verify the announce signature
    ///
    /// Returns `Ok(true)` if the signature is valid, `Ok(false)` if invalid,
    /// or an error if the public key is malformed.
    pub fn verify_signature(&self) -> Result<bool, IdentityError> {
        let identity = Identity::from_public_key_bytes(&self.public_key)?;
        let signed_data = self.signed_data();
        identity.verify(&signed_data, &self.signature)
    }

    /// Create an Identity from the announce's public key
    ///
    /// This creates a public-only identity that can be used for encryption
    /// and signature verification, but not signing or decryption.
    pub fn to_identity(&self) -> Result<Identity, IdentityError> {
        Identity::from_public_key_bytes(&self.public_key)
    }

    /// Perform full validation of the announce
    ///
    /// This checks:
    /// 1. Destination hash matches computed hash
    /// 2. Signature is valid
    ///
    /// Returns `Ok(())` if valid, or an error describing what failed.
    pub(crate) fn validate(&self) -> Result<(), AnnounceError> {
        if !self.verify_destination_hash() {
            return Err(AnnounceError::HashMismatch);
        }

        match self.verify_signature() {
            Ok(true) => Ok(()),
            Ok(false) => Err(AnnounceError::InvalidSignature),
            Err(_) => Err(AnnounceError::InvalidPublicKey),
        }
    }
}

/// Fully verify an announce packet's signature and destination hash — a pure
/// function of the packet bytes (the announce carries its own public key), so
/// a driver can run it OFF the node lock and pass the result in as a
/// [`crate::node::PrecomputedRx`] memo (leviculum#29 stages 2-3). Returns
/// `false` for non-announce packets and for any parse/verify failure; the
/// in-lock path then re-checks and drops exactly as before, so a wrong memo
/// can only cost duplicate work, never skip verification.
pub fn verify_announce_packet(packet: &Packet) -> bool {
    match ReceivedAnnounce::from_packet(packet) {
        Ok(a) => a.validate().is_ok(),
        Err(_) => false,
    }
}

impl core::fmt::Debug for ReceivedAnnounce {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReceivedAnnounce")
            .field(
                "destination_hash",
                &format_args!("{:02x?}", &self.destination_hash.as_bytes()[..4]),
            )
            .field("has_ratchet", &self.has_ratchet())
            .field("app_data_len", &self.app_data.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::DestinationType;
    use crate::packet::{HeaderType, PacketContext, PacketData, PacketFlags, TransportType};
    use alloc::vec;
    use rand_core::OsRng;

    const TEST_EMISSION_SECS: u64 = 1704067200; // 2024-01-01 00:00:00 UTC

    fn create_test_announce_payload(with_ratchet: bool) -> Vec<u8> {
        let mut payload = Vec::new();

        // Public key (64 bytes) - just test data
        payload.extend_from_slice(&[0x01; IDENTITY_KEY_SIZE]);

        // Name hash (10 bytes)
        payload.extend_from_slice(&[0x02; NAME_HASHBYTES]);

        // Random hash (10 bytes)
        payload.extend_from_slice(&[0x03; RANDOM_HASHBYTES]);

        if with_ratchet {
            // Ratchet (32 bytes)
            payload.extend_from_slice(&[0x04; RATCHET_SIZE]);
        }

        // Signature (64 bytes)
        payload.extend_from_slice(&[0x05; ED25519_SIGNATURE_SIZE]);

        // App data
        payload.extend_from_slice(b"test.app");

        payload
    }

    #[test]
    fn test_parse_announce_without_ratchet() {
        let payload = create_test_announce_payload(false);

        let packet = Packet {
            flags: PacketFlags {
                ifac_flag: false,
                header_type: HeaderType::Type1,
                context_flag: false,
                transport_type: TransportType::Broadcast,
                dest_type: DestinationType::Single,
                packet_type: PacketType::Announce,
            },
            hops: 0,
            transport_id: None,
            destination_hash: [0xaa; TRUNCATED_HASHBYTES],
            context: PacketContext::None,
            data: PacketData::Owned(payload),
        };

        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();

        assert!(!announce.has_ratchet());
        assert_eq!(announce.public_key(), &[0x01; IDENTITY_KEY_SIZE]);
        assert_eq!(announce.name_hash(), &[0x02; NAME_HASHBYTES]);
        assert_eq!(announce.random_hash(), &[0x03; RANDOM_HASHBYTES]);
        assert_eq!(announce.signature(), &[0x05; ED25519_SIGNATURE_SIZE]);
        assert_eq!(announce.app_data(), b"test.app");
        assert_eq!(announce.app_data_string(), Some("test.app"));
    }

    #[test]
    fn test_parse_announce_with_ratchet() {
        let payload = create_test_announce_payload(true);

        let packet = Packet {
            flags: PacketFlags {
                ifac_flag: false,
                header_type: HeaderType::Type1,
                context_flag: true, // Indicates ratchet is present
                transport_type: TransportType::Broadcast,
                dest_type: DestinationType::Single,
                packet_type: PacketType::Announce,
            },
            hops: 0,
            transport_id: None,
            destination_hash: [0xaa; TRUNCATED_HASHBYTES],
            context: PacketContext::None,
            data: PacketData::Owned(payload),
        };

        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();

        assert!(announce.has_ratchet());
        assert_eq!(announce.ratchet(), Some(&[0x04; RATCHET_SIZE]));
        assert_eq!(announce.app_data(), b"test.app");
    }

    #[test]
    fn test_not_announce_packet() {
        let packet = Packet {
            flags: PacketFlags {
                ifac_flag: false,
                header_type: HeaderType::Type1,
                context_flag: false,
                transport_type: TransportType::Broadcast,
                dest_type: DestinationType::Single,
                packet_type: PacketType::Data, // Not an announce
            },
            hops: 0,
            transport_id: None,
            destination_hash: [0xaa; TRUNCATED_HASHBYTES],
            context: PacketContext::None,
            data: PacketData::Owned(vec![0; 200]),
        };

        let result = ReceivedAnnounce::from_packet(&packet);
        assert!(matches!(result, Err(AnnounceError::NotAnnounce)));
    }

    #[test]
    fn test_payload_too_short() {
        let packet = Packet {
            flags: PacketFlags {
                ifac_flag: false,
                header_type: HeaderType::Type1,
                context_flag: false,
                transport_type: TransportType::Broadcast,
                dest_type: DestinationType::Single,
                packet_type: PacketType::Announce,
            },
            hops: 0,
            transport_id: None,
            destination_hash: [0xaa; TRUNCATED_HASHBYTES],
            context: PacketContext::None,
            data: PacketData::Owned(vec![0; 100]), // Too short
        };

        let result = ReceivedAnnounce::from_packet(&packet);
        assert!(matches!(result, Err(AnnounceError::PayloadTooShort)));
    }

    #[test]
    fn test_computed_hashes() {
        // Create a minimal valid announce with known test data
        let payload = create_test_announce_payload(false);

        let packet = Packet {
            flags: PacketFlags {
                ifac_flag: false,
                header_type: HeaderType::Type1,
                context_flag: false,
                transport_type: TransportType::Broadcast,
                dest_type: DestinationType::Single,
                packet_type: PacketType::Announce,
            },
            hops: 0,
            transport_id: None,
            destination_hash: [0xaa; TRUNCATED_HASHBYTES],
            context: PacketContext::None,
            data: PacketData::Owned(payload),
        };

        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();

        // Identity hash is truncated_hash(public_key)
        let identity_hash = announce.computed_identity_hash();
        assert_eq!(identity_hash.len(), TRUNCATED_HASHBYTES);

        // Destination hash is truncated_hash(name_hash + identity_hash)
        let dest_hash = announce.computed_destination_hash();
        assert_eq!(dest_hash.as_bytes().len(), TRUNCATED_HASHBYTES);

        // These won't match because destination_hash in packet is fake
        assert!(!announce.verify_destination_hash());
    }

    #[test]
    fn test_real_announce_creation_and_verification() {
        use crate::destination::{Destination, Direction};
        use rand_core::OsRng;

        // Create a real identity and destination
        let identity = Identity::generate(&mut OsRng);
        let dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["echo"],
        )
        .expect("Failed to create destination");

        // Get the identity from the destination for signing
        let identity = dest.identity().expect("destination should have identity");

        // Create random hash
        let random_hash = [0x42u8; RANDOM_HASHBYTES];

        // Build announce payload
        let mut payload = Vec::new();
        payload.extend_from_slice(&identity.public_key_bytes());
        payload.extend_from_slice(dest.name_hash());
        payload.extend_from_slice(&random_hash);

        // App data
        let app_data = b"testapp.echo";

        // Compute signed data
        let mut signed_data = Vec::new();
        signed_data.extend_from_slice(dest.hash().as_bytes());
        signed_data.extend_from_slice(&identity.public_key_bytes());
        signed_data.extend_from_slice(dest.name_hash());
        signed_data.extend_from_slice(&random_hash);
        signed_data.extend_from_slice(app_data);

        // Sign
        let signature = identity.sign(&signed_data).unwrap();
        payload.extend_from_slice(&signature);
        payload.extend_from_slice(app_data);

        // Create packet
        let packet = Packet {
            flags: PacketFlags {
                ifac_flag: false,
                header_type: HeaderType::Type1,
                context_flag: false,
                transport_type: TransportType::Broadcast,
                dest_type: DestinationType::Single,
                packet_type: PacketType::Announce,
            },
            hops: 0,
            transport_id: None,
            destination_hash: dest.hash().into_bytes(),
            context: PacketContext::None,
            data: PacketData::Owned(payload),
        };

        // Parse and verify
        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();

        assert!(announce.verify_destination_hash());
        assert!(announce.verify_signature().unwrap());
        assert!(announce.validate().is_ok());
    }

    #[test]
    fn test_generate_random_hash_format() {
        let hash = generate_random_hash(&mut OsRng, TEST_EMISSION_SECS);

        // Should be 10 bytes
        assert_eq!(hash.len(), RANDOM_HASHBYTES);

        // Last 5 bytes carry the emission timestamp verbatim, in unix
        // seconds (Codeberg #155): 1704067200 = 0x65920080,
        // to_be_bytes() gives [0x00, 0x00, 0x00, 0x00, 0x65, 0x92, 0x00, 0x80],
        // bytes 3..8 are [0x00, 0x65, 0x92, 0x00, 0x80]
        assert_eq!(&hash[5..10], &[0x00, 0x65, 0x92, 0x00, 0x80]);
    }

    #[test]
    fn test_generate_random_hash_different_each_call() {
        let hash1 = generate_random_hash(&mut OsRng, TEST_EMISSION_SECS);
        let hash2 = generate_random_hash(&mut OsRng, TEST_EMISSION_SECS);

        // First 5 bytes (random) should be different
        // (with overwhelming probability)
        assert_ne!(&hash1[0..5], &hash2[0..5]);

        // Last 5 bytes (timestamp) should be the same (same emission)
        assert_eq!(&hash1[5..10], &hash2[5..10]);
    }

    #[test]
    fn test_generate_random_hash_saturates_at_field_max() {
        // Codeberg #160: an emission just past 2^40 would truncate to 99 in
        // a naive byte copy — the encoder must saturate at the field
        // maximum so the emitted ordering never collapses.
        use crate::constants::EMISSION_TIMESTAMP_MAX_SECS;
        let hash = generate_random_hash(&mut OsRng, EMISSION_TIMESTAMP_MAX_SECS + 100);
        assert_eq!(
            emission_from_random_hash(&hash),
            EMISSION_TIMESTAMP_MAX_SECS
        );
    }

    #[test]
    fn test_emission_from_random_hash_consistency() {
        // emission_from_random_hash extracts a 40-bit timebase, not the full
        // u64 timestamp. Two hashes with the same emission produce the same
        // timebase, and different timestamps produce ordered timebases.
        let h1 = generate_random_hash(&mut OsRng, TEST_EMISSION_SECS);
        let h2 = generate_random_hash(&mut OsRng, TEST_EMISSION_SECS);
        assert_eq!(
            emission_from_random_hash(&h1),
            emission_from_random_hash(&h2),
            "Same timestamp should produce same timebase"
        );
    }

    #[test]
    fn test_emission_from_random_hash_ordering() {
        let t1 = 1_000_000u64;
        let t2 = 2_000_000u64;
        let h1 = generate_random_hash(&mut OsRng, t1);
        let h2 = generate_random_hash(&mut OsRng, t2);
        assert!(
            emission_from_random_hash(&h2) > emission_from_random_hash(&h1),
            "Later timestamp should produce larger timebase"
        );
    }

    #[test]
    fn test_max_emission_from_blobs_empty() {
        let blobs: &[[u8; RANDOM_HASHBYTES]] = &[];
        assert_eq!(max_emission_from_blobs(blobs), 0);
    }

    #[test]
    fn test_max_emission_from_blobs_picks_latest() {
        let h1 = generate_random_hash(&mut OsRng, 1_000_000);
        let h2 = generate_random_hash(&mut OsRng, 3_000_000);
        let h3 = generate_random_hash(&mut OsRng, 2_000_000);
        let blobs = [h1, h2, h3];
        // Emission timestamps are stored verbatim in unix seconds
        assert_eq!(max_emission_from_blobs(&blobs), 3_000_000);
    }

    /// Regression guard (Codeberg #108, fuzz `announce_from_packet`): the
    /// field-slicer slices fixed-size fields out of the payload after the
    /// `ANNOUNCE_MIN_SIZE` / `ANNOUNCE_RATCHETED_MIN_SIZE` gate. Feed every
    /// payload length around both boundaries, for both ratchet framings, and
    /// require it to reject (or accept) without panicking on an out-of-bounds
    /// slice. The fuzzer found no crash here; this locks that in.
    #[test]
    fn from_packet_never_panics_around_size_boundaries() {
        fn packet(len: usize, context_flag: bool) -> Packet {
            Packet {
                flags: PacketFlags {
                    ifac_flag: false,
                    header_type: HeaderType::Type1,
                    context_flag,
                    transport_type: TransportType::Broadcast,
                    dest_type: DestinationType::Single,
                    packet_type: PacketType::Announce,
                },
                hops: 0,
                transport_id: None,
                destination_hash: [0u8; TRUNCATED_HASHBYTES],
                context: PacketContext::None,
                data: PacketData::Owned(vec![0u8; len]),
            }
        }

        for context_flag in [false, true] {
            let boundary = if context_flag {
                ANNOUNCE_RATCHETED_MIN_SIZE
            } else {
                ANNOUNCE_MIN_SIZE
            };
            for len in 0..=(boundary + 2) {
                let pkt = packet(len, context_flag);
                // Must not panic; short payloads reject, exact/over accept.
                let parsed = ReceivedAnnounce::from_packet(&pkt);
                assert_eq!(parsed.is_ok(), len >= boundary);
            }
        }
    }
}
