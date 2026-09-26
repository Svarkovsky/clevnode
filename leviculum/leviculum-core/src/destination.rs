//! Network endpoint addressing
//!
//! A Destination represents an addressable endpoint in the Reticulum network.
//! Destinations have:
//! - A unique hash derived from application name, aspects, and identity
//! - A type (SINGLE, GROUP, PLAIN, LINK) determining encryption behavior
//! - A direction (IN, OUT) determining receive/send capability
//!
//! # Ratchet Support
//!
//! Destinations can enable ratchets for forward secrecy. When enabled:
//! - The destination rotates X25519 key pairs periodically
//! - Announces include the current ratchet public key
//! - Incoming packets are decrypted by trying retained ratchets
//!
//! See [`Destination::enable_ratchets`] for more details.

use crate::announce::{build_announce_payload, AnnounceError};
use crate::constants::{
    IDENTITY_HASHBYTES, NAME_HASHBYTES, RATCHET_SIZE, TOKEN_KEY_SIZE, TRUNCATED_HASHBYTES,
};
use crate::crypto::{decrypt_token, encrypt_token, sha256, truncated_hash};
use crate::identity::{Identity, IdentityError};
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::ratchet::{Ratchet, DEFAULT_INTERVAL_MS, DEFAULT_RETAINED_RATCHETS};

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use rand_core::CryptoRngCore;

/// Error type for destination operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationError {
    /// PLAIN destination type cannot have an identity
    PlainCannotHaveIdentity,
    /// Outbound SINGLE/GROUP destinations require an identity
    OutboundRequiresIdentity,
    /// Operation requires an identity
    NoIdentity,
    /// Ratchets are required but no ratchet was used
    RatchetRequired,
    /// Decryption failed
    DecryptionFailed,
    /// Encryption failed
    EncryptionFailed,
    /// Cannot enable ratchets on OUT destination
    CannotEnableRatchetsOnOut,
    /// Invalid serialized ratchet data
    InvalidRatchetData,
    /// Ed25519 signature verification failed
    InvalidSignature,
    /// Operation is only valid on a GROUP destination
    NotGroupType,
    /// GROUP destination holds no symmetric key (call create/load first)
    NoGroupKey,
    /// Supplied GROUP key has an unsupported length (must be 64 bytes)
    InvalidGroupKeyLength,
    /// `app_name` contains a dot, the dotted-name separator (Codeberg #163,
    /// Python `Destination.py:102`/`:151`)
    DotInAppName,
    /// An aspect contains a dot, the dotted-name separator (Codeberg #163,
    /// Python `Destination.py:106`)
    DotInAspect,
}

impl core::fmt::Display for DestinationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DestinationError::PlainCannotHaveIdentity => {
                write!(f, "PLAIN destination type cannot hold an identity")
            }
            DestinationError::OutboundRequiresIdentity => {
                write!(f, "Outbound SINGLE/GROUP destinations require an identity")
            }
            DestinationError::NoIdentity => {
                write!(f, "Operation requires an identity")
            }
            DestinationError::RatchetRequired => {
                write!(f, "Ratchets are required but no ratchet was used")
            }
            DestinationError::DecryptionFailed => {
                write!(f, "Decryption failed")
            }
            DestinationError::EncryptionFailed => {
                write!(f, "Encryption failed")
            }
            DestinationError::CannotEnableRatchetsOnOut => {
                write!(f, "Cannot enable ratchets on OUT destination")
            }
            DestinationError::InvalidRatchetData => {
                write!(f, "Invalid serialized ratchet data")
            }
            DestinationError::InvalidSignature => {
                write!(f, "Ed25519 signature verification failed")
            }
            DestinationError::NotGroupType => {
                write!(f, "Operation is only valid on a GROUP destination")
            }
            DestinationError::NoGroupKey => {
                write!(f, "GROUP destination holds no symmetric key")
            }
            DestinationError::InvalidGroupKeyLength => {
                write!(f, "GROUP key must be 64 bytes")
            }
            DestinationError::DotInAppName => {
                write!(f, "Dots can't be used in app names")
            }
            DestinationError::DotInAspect => {
                write!(f, "Dots can't be used in aspects")
            }
        }
    }
}

impl From<IdentityError> for DestinationError {
    fn from(e: IdentityError) -> Self {
        match e {
            IdentityError::NoPrivateKey => DestinationError::NoIdentity,
            IdentityError::EncryptionFailed => DestinationError::EncryptionFailed,
            IdentityError::DecryptionFailed => DestinationError::DecryptionFailed,
            _ => DestinationError::DecryptionFailed,
        }
    }
}

/// Destination type determining encryption behavior
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DestinationType {
    /// Point-to-point with ephemeral encryption per packet
    Single = 0x00,
    /// Broadcast with pre-shared key
    Group = 0x01,
    /// Unencrypted
    Plain = 0x02,
    /// Link-based encryption (internal use)
    Link = 0x03,
}

impl TryFrom<u8> for DestinationType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(DestinationType::Single),
            0x01 => Ok(DestinationType::Group),
            0x02 => Ok(DestinationType::Plain),
            0x03 => Ok(DestinationType::Link),
            _ => Err(()),
        }
    }
}

/// Destination direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    /// Incoming - can receive announces and accept links
    In = 0x11,
    /// Outgoing - source address for packets
    Out = 0x12,
}

/// Proof generation strategy for incoming packets
///
/// Controls how the destination responds to requests for delivery proofs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ProofStrategy {
    /// Never generate proofs (default)
    #[default]
    None = 0x21,
    /// Ask application via ProofRequested event to decide
    App = 0x22,
    /// Automatically prove every packet
    All = 0x23,
}

/// A 16-byte destination hash (truncated hash of name_hash + identity_hash)
///
/// In the Reticulum protocol, destination hashes identify network endpoints.
/// They are computed as `truncated_hash(name_hash + identity_hash)` and
/// appear in packet headers and routing tables.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DestinationHash([u8; TRUNCATED_HASHBYTES]);

impl DestinationHash {
    /// Create a DestinationHash from raw bytes
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

impl From<[u8; TRUNCATED_HASHBYTES]> for DestinationHash {
    fn from(bytes: [u8; TRUNCATED_HASHBYTES]) -> Self {
        Self(bytes)
    }
}

impl From<DestinationHash> for [u8; TRUNCATED_HASHBYTES] {
    fn from(hash: DestinationHash) -> Self {
        hash.0
    }
}

impl AsRef<[u8]> for DestinationHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8; TRUNCATED_HASHBYTES]> for DestinationHash {
    fn as_ref(&self) -> &[u8; TRUNCATED_HASHBYTES] {
        &self.0
    }
}

impl core::borrow::Borrow<[u8; TRUNCATED_HASHBYTES]> for DestinationHash {
    fn borrow(&self) -> &[u8; TRUNCATED_HASHBYTES] {
        &self.0
    }
}

impl PartialEq<[u8; TRUNCATED_HASHBYTES]> for DestinationHash {
    fn eq(&self, other: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        self.0 == *other
    }
}

impl PartialEq<DestinationHash> for [u8; TRUNCATED_HASHBYTES] {
    fn eq(&self, other: &DestinationHash) -> bool {
        *self == other.0
    }
}

impl core::fmt::Debug for DestinationHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DestinationHash(")?;
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        write!(f, ")")
    }
}

impl core::fmt::Display for DestinationHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

/// A network destination (endpoint)
pub struct Destination {
    /// The destination hash (address)
    hash: DestinationHash,
    /// Name hash (truncated hash of app_name.aspects)
    name_hash: [u8; NAME_HASHBYTES],
    /// The dotted full name (`app_name.aspect1.aspect2...`) the name hash was
    /// computed from. Python keeps it as `Destination.name` (`expand_name`,
    /// Destination.py:96-110); we keep it so a name_hash seen in a foreign
    /// announce can be labelled when it matches an aspect this node itself
    /// registered (the identity listing RPC) — never by guessing.
    full_name: String,
    /// Associated identity (optional for PLAIN destinations)
    identity: Option<Identity>,
    /// Destination type
    dest_type: DestinationType,
    /// Direction
    direction: Direction,
    /// Whether to accept incoming links
    accepts_links: bool,
    /// Proof generation strategy for incoming packets
    proof_strategy: ProofStrategy,

    // Ratchet State (for IN destinations)
    /// Retained ratchets for decryption (newest first)
    ratchets: Vec<Ratchet>,
    /// Ratchet rotation interval in milliseconds
    ratchet_interval_ms: u64,
    /// Maximum number of ratchets to retain
    retained_ratchet_count: usize,
    /// Timestamp of last ratchet rotation
    last_ratchet_time_ms: u64,
    /// If true, reject packets not encrypted with a ratchet
    enforce_ratchets: bool,
    /// If true, ratchets are enabled for this destination
    ratchets_enabled: bool,
    /// If true, ratchet keys have changed since last persist
    ratchets_dirty: bool,

    /// Symmetric key for GROUP destinations (64 bytes: 32 signing + 32 AES).
    ///
    /// Mirrors Python `Destination.prv_bytes` for GROUP type. `None` until
    /// `create_group_key` or `load_group_key` is called. Only meaningful for
    /// [`DestinationType::Group`].
    group_key: Option<[u8; TOKEN_KEY_SIZE]>,

    /// The app_data a bare `announce(None, ..)` emits (Python `Destination.py`
    /// `default_app_data`). Updated by every announce carrying explicit
    /// app_data, so re-announce paths that cannot know the caller's payload
    /// (interface-up, scheduled re-announces) reproduce the last announce
    /// instead of stripping it.
    default_app_data: Option<Vec<u8>>,
    /// If true, `hash` was set to a caller-supplied override rather than
    /// computed from `truncated_hash(name_hash || identity_hash)`. Such a
    /// destination is reachable by direct link (the link path carries an
    /// opaque hash) but MUST NOT be announced — see [`Destination::announce`].
    explicit_hash: bool,
}

impl Destination {
    /// Create a new destination
    ///
    /// # Arguments
    /// * `identity` - The identity for this destination (required except for PLAIN)
    /// * `direction` - IN for receiving, OUT for sending
    /// * `dest_type` - The encryption type
    /// * `app_name` - Application name
    /// * `aspects` - Additional name components
    ///
    /// # Errors
    /// * `PlainCannotHaveIdentity` - PLAIN destinations cannot have an identity
    /// * `OutboundRequiresIdentity` - SINGLE/GROUP OUT destinations require an identity
    /// * `DotInAppName` / `DotInAspect` - a name component contains the dotted-name
    ///   separator (Codeberg #163)
    pub fn new(
        identity: Option<Identity>,
        direction: Direction,
        dest_type: DestinationType,
        app_name: &str,
        aspects: &[&str],
    ) -> Result<Self, DestinationError> {
        // A dot is the separator of the dotted-name form, so a dot inside a
        // component makes two different (app_name, aspects) tuples expand to
        // the same full name and collide on the same destination hash. The
        // reference rejects the input before anything else, in both
        // `expand_name` (Destination.py:102, :106) and `__init__`
        // (Destination.py:151) (Codeberg #163).
        Self::validate_name_components(app_name, aspects)?;

        // PLAIN destinations cannot have an identity (per Python Reticulum spec)
        if dest_type == DestinationType::Plain && identity.is_some() {
            return Err(DestinationError::PlainCannotHaveIdentity);
        }

        // SINGLE/GROUP OUT destinations require an identity for encryption
        if direction == Direction::Out && dest_type != DestinationType::Plain && identity.is_none()
        {
            return Err(DestinationError::OutboundRequiresIdentity);
        }

        let name_hash = Self::compute_name_hash(app_name, aspects);
        let mut full_name = String::from(app_name);
        for aspect in aspects {
            full_name.push('.');
            full_name.push_str(aspect);
        }

        let hash = match &identity {
            Some(id) => Self::compute_destination_hash(&name_hash, id.hash()),
            None => {
                // For PLAIN destinations without identity: hash = full_hash(name_hash)[:16]
                // This matches Python: RNS.Identity.full_hash(name_hash)[:TRUNCATED_HASHLENGTH//8]
                DestinationHash::new(truncated_hash(&name_hash))
            }
        };

        Ok(Self {
            hash,
            name_hash,
            full_name,
            identity,
            dest_type,
            direction,
            // Python parity: a fresh Destination accepts incoming links. The
            // OFF switch is `set_accepts_links(false)`.
            accepts_links: true,
            proof_strategy: ProofStrategy::None,
            // Ratchet fields - not enabled by default
            ratchets: Vec::new(),
            ratchet_interval_ms: DEFAULT_INTERVAL_MS,
            retained_ratchet_count: DEFAULT_RETAINED_RATCHETS,
            last_ratchet_time_ms: 0,
            enforce_ratchets: false,
            ratchets_enabled: false,
            ratchets_dirty: false,
            group_key: None,
            default_app_data: None,
            explicit_hash: false,
        })
    }

    /// Create a destination whose `hash` is a caller-supplied 16-byte override
    /// rather than the standard `truncated_hash(name_hash || identity_hash)`.
    ///
    /// This exists for cross-transport addressing schemes that need a
    /// destination's routable hash to equal some externally-derived value
    /// (e.g. `sha256(federation_pubkey)[..16]`) that cannot be obtained as a
    /// SHA-256 preimage of the structural formula. Identity-based crypto is
    /// unaffected: link establishment, packet encryption, and the link proof
    /// all continue to use the destination's real `identity` — the override
    /// only changes the 16-byte routing index under which the destination is
    /// stored and matched.
    ///
    /// # Compatibility guard
    /// An explicit-hash destination is reachable **only via direct link**: the
    /// LINK_REQUEST carries the hash as opaque bytes and the receiver resolves
    /// it by a bare table lookup, so consenting peers interoperate. It MUST NOT
    /// be announced — [`Destination::announce`] returns
    /// [`AnnounceError::ExplicitHashCannotAnnounce`] for it — because an
    /// announce would advertise a destination_hash that every Python-RNS peer
    /// recomputes from `name_hash || identity_hash` and rejects on mismatch.
    /// This confines the wire-break to the opaque link path and keeps the
    /// announce stream Python-RNS compatible.
    ///
    /// Arguments mirror [`Destination::new`], plus `explicit_hash`: the exact
    /// 16 bytes to index this destination by.
    ///
    /// # Collisions
    /// The caller controls the index, so it can collide with the derived hash
    /// of another local destination or a known remote path. Registration is
    /// last-wins: `NodeCore::register_destination` replaces the previous
    /// entry under the same hash (and logs a warning when the displaced
    /// destination is a different one). Callers must derive the override from
    /// enough entropy (e.g. `sha256(pubkey)[..16]`) that collisions are as
    /// unlikely as for derived hashes.
    pub fn with_explicit_hash(
        identity: Option<Identity>,
        direction: Direction,
        dest_type: DestinationType,
        app_name: &str,
        aspects: &[&str],
        explicit_hash: [u8; TRUNCATED_HASHBYTES],
    ) -> Result<Self, DestinationError> {
        let mut dest = Self::new(identity, direction, dest_type, app_name, aspects)?;
        dest.hash = DestinationHash::new(explicit_hash);
        dest.explicit_hash = true;
        Ok(dest)
    }

    /// Whether this destination's hash is a caller-supplied override
    /// (constructed via [`Destination::with_explicit_hash`]). Such
    /// destinations are never announced.
    pub fn is_explicit_hash(&self) -> bool {
        self.explicit_hash
    }

    /// Get the destination hash
    pub fn hash(&self) -> &DestinationHash {
        &self.hash
    }

    /// Get the name hash
    pub fn name_hash(&self) -> &[u8; NAME_HASHBYTES] {
        &self.name_hash
    }

    /// The dotted full name (`app_name.aspect1.aspect2...`), Python's
    /// `Destination.name`.
    pub fn full_name(&self) -> &str {
        &self.full_name
    }

    /// Get the destination type
    pub fn dest_type(&self) -> DestinationType {
        self.dest_type
    }

    /// Get the direction
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Get the associated identity
    pub fn identity(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    /// Check if this destination accepts incoming links
    pub fn accepts_links(&self) -> bool {
        self.accepts_links
    }

    /// Set whether to accept incoming links
    pub fn set_accepts_links(&mut self, accept: bool) {
        self.accepts_links = accept;
    }

    /// Get the proof generation strategy
    pub fn proof_strategy(&self) -> ProofStrategy {
        self.proof_strategy
    }

    /// Set the proof generation strategy for incoming packets
    ///
    /// # Arguments
    /// * `strategy` - The proof strategy to use:
    ///   - `ProofStrategy::None` - Never generate proofs (default)
    ///   - `ProofStrategy::App` - Emit ProofRequested event for app to decide
    ///   - `ProofStrategy::All` - Automatically prove every packet
    pub fn set_proof_strategy(&mut self, strategy: ProofStrategy) {
        self.proof_strategy = strategy;
    }

    // GROUP symmetric key management
    /// Generate a new symmetric key for this GROUP destination.
    ///
    /// Mirrors Python `Destination.create_keys()` for the GROUP branch, which
    /// calls `Token.generate_key()` with its default mode (`AES_256_CBC`),
    /// producing 64 random bytes: 32-byte HMAC-SHA256 signing key followed by a
    /// 32-byte AES-256 key. A GROUP destination must hold a key before it can
    /// [`encrypt`](Self::encrypt)/[`decrypt`](Self::decrypt).
    ///
    /// # Errors
    /// * `NotGroupType` - the destination is not a GROUP destination.
    pub fn create_group_key(
        &mut self,
        rng: &mut impl CryptoRngCore,
    ) -> Result<(), DestinationError> {
        if self.dest_type != DestinationType::Group {
            return Err(DestinationError::NotGroupType);
        }
        let mut key = [0u8; TOKEN_KEY_SIZE];
        rng.fill_bytes(&mut key);
        self.group_key = Some(key);
        Ok(())
    }

    /// Load a shared symmetric key into this GROUP destination.
    ///
    /// Mirrors Python `Destination.load_private_key(key)` for the GROUP branch.
    /// The key must be exactly 64 bytes (the `AES_256_CBC` token key size that
    /// `create_keys` generates): the first 32 bytes are the HMAC signing key and
    /// the last 32 bytes are the AES-256 key.
    ///
    /// # Errors
    /// * `NotGroupType` - the destination is not a GROUP destination.
    /// * `InvalidGroupKeyLength` - the key is not 64 bytes.
    pub fn load_group_key(&mut self, key: &[u8]) -> Result<(), DestinationError> {
        if self.dest_type != DestinationType::Group {
            return Err(DestinationError::NotGroupType);
        }
        if key.len() != TOKEN_KEY_SIZE {
            return Err(DestinationError::InvalidGroupKeyLength);
        }
        let mut stored = [0u8; TOKEN_KEY_SIZE];
        stored.copy_from_slice(key);
        self.group_key = Some(stored);
        Ok(())
    }

    /// Return the symmetric key held by this GROUP destination.
    ///
    /// Mirrors Python `Destination.get_private_key()` for the GROUP branch.
    /// Returns `None` if no key has been created or loaded.
    pub fn group_key(&self) -> Option<&[u8; TOKEN_KEY_SIZE]> {
        self.group_key.as_ref()
    }

    // Ratchet Management
    /// Enable ratchets for forward secrecy on this destination
    ///
    /// Ratchets provide forward secrecy by rotating X25519 key pairs
    /// periodically. When enabled, announces will include the current
    /// ratchet public key, and packets must be encrypted with that ratchet.
    ///
    /// # Arguments
    /// * `rng` - Random number generator
    /// * `now_ms` - Current timestamp in milliseconds
    ///
    /// # Errors
    /// Returns error if called on an OUT destination.
    ///
    /// # Example
    /// ```
    /// use leviculum_core::{Destination, DestinationType, Direction};
    /// use leviculum_core::identity::Identity;
    /// use rand_core::OsRng;
    ///
    /// let identity = Identity::generate(&mut OsRng);
    /// let mut dest = Destination::new(
    ///     Some(identity),
    ///     Direction::In,
    ///     DestinationType::Single,
    ///     "app",
    ///     &["echo"],
    /// ).unwrap();
    ///
    /// dest.enable_ratchets(&mut OsRng, 1704067200000).unwrap();
    /// assert!(dest.ratchets_enabled());
    /// ```
    pub fn enable_ratchets(
        &mut self,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
    ) -> Result<(), DestinationError> {
        if self.direction == Direction::Out {
            return Err(DestinationError::CannotEnableRatchetsOnOut);
        }

        // Generate initial ratchet
        let ratchet = Ratchet::generate(rng, now_ms);
        self.last_ratchet_time_ms = now_ms;
        self.ratchets.insert(0, ratchet);
        self.ratchets_enabled = true;
        self.ratchets_dirty = true;

        Ok(())
    }

    /// Check if ratchets are enabled for this destination
    pub fn ratchets_enabled(&self) -> bool {
        self.ratchets_enabled
    }

    /// Get the current ratchet public key (for announces)
    ///
    /// Returns None if ratchets are not enabled.
    pub fn current_ratchet_public(&self) -> Option<[u8; RATCHET_SIZE]> {
        if !self.ratchets_enabled {
            return None;
        }
        self.ratchets.first().map(|r| r.public_key_bytes())
    }

    /// Rotate ratchet if the interval has passed
    ///
    /// Call this before creating an announce to ensure the ratchet is current.
    ///
    /// # Arguments
    /// * `rng` - Random number generator
    /// * `now_ms` - Current timestamp in milliseconds
    ///
    /// # Returns
    /// True if a new ratchet was generated.
    pub fn rotate_ratchet_if_needed(&mut self, rng: &mut impl CryptoRngCore, now_ms: u64) -> bool {
        if !self.ratchets_enabled {
            return false;
        }

        let elapsed = now_ms.saturating_sub(self.last_ratchet_time_ms);

        if elapsed < self.ratchet_interval_ms {
            return false;
        }

        // Generate new ratchet and add to front
        let ratchet = Ratchet::generate(rng, now_ms);
        self.ratchets.insert(0, ratchet);
        self.last_ratchet_time_ms = now_ms;
        self.ratchets_dirty = true;

        // Trim old ratchets if exceeding limit
        if self.ratchets.len() > self.retained_ratchet_count {
            self.ratchets.truncate(self.retained_ratchet_count);
        }

        true
    }

    /// Minimum ratchet interval in milliseconds (1 second)
    const MIN_RATCHET_INTERVAL_MS: u64 = 1000;

    /// Set the ratchet rotation interval
    ///
    /// # Arguments
    /// * `interval_ms` - Rotation interval in milliseconds (default: 30 minutes, minimum: 1 second)
    pub fn set_ratchet_interval(&mut self, interval_ms: u64) {
        self.ratchet_interval_ms = interval_ms.max(Self::MIN_RATCHET_INTERVAL_MS);
    }

    /// Get the current ratchet rotation interval in milliseconds
    pub fn ratchet_interval(&self) -> u64 {
        self.ratchet_interval_ms
    }

    /// Set the maximum number of ratchets to retain
    ///
    /// # Arguments
    /// * `count` - Maximum retained ratchets (default: 512, minimum: 1)
    pub fn set_retained_ratchets(&mut self, count: usize) {
        // Ensure at least 1 ratchet is retained
        self.retained_ratchet_count = count.max(1);

        // Trim if needed
        if self.ratchets.len() > self.retained_ratchet_count {
            self.ratchets.truncate(self.retained_ratchet_count);
        }
    }

    /// Get the number of retained ratchets
    pub fn retained_ratchet_count(&self) -> usize {
        self.retained_ratchet_count
    }

    /// Get the current number of ratchets stored
    pub fn ratchet_count(&self) -> usize {
        self.ratchets.len()
    }

    /// Check if ratchet keys have changed since last persist.
    pub fn ratchets_dirty(&self) -> bool {
        self.ratchets_dirty
    }

    /// Clear the dirty flag (call after persisting ratchet keys).
    pub fn clear_ratchets_dirty(&mut self) {
        self.ratchets_dirty = false;
    }

    /// Serialize ratchet private keys as Python-compatible signed msgpack.
    ///
    /// Format: `msgpack({"signature": Ed25519(inner), "ratchets": inner})`
    /// where inner = `msgpack([key1, key2, ...])` and each key is 32 bytes
    /// (X25519 private key), newest first.
    ///
    /// Python ref: `Destination._persist_ratchets()` (Destination.py:210-225).
    /// Python stores only 32-byte private keys, no timestamps. Public keys
    /// are derived on load via `X25519PrivateKey.from_private_bytes()`.
    ///
    /// Returns None if destination has no identity (cannot sign).
    pub fn serialize_ratchets_signed(&self) -> Option<Vec<u8>> {
        let identity = self.identity.as_ref()?;

        // Build inner: msgpack array of N binary(32) entries
        let inner = msgpack_encode_ratchet_array(&self.ratchets);

        // Sign the inner bytes
        let signature = identity.sign(&inner).ok()?;

        // Build outer: msgpack fixmap(2) with "signature" and "ratchets" keys
        msgpack_encode_signed_ratchets(&signature, &inner)
    }

    /// Load ratchet private keys from Python-compatible signed msgpack.
    ///
    /// Verifies Ed25519 signature, then reconstructs Ratchet structs.
    /// Since Python doesn't store timestamps, loaded ratchets get
    /// `created_at_ms = 0`.
    ///
    /// Python ref: `Destination._reload_ratchets()` (Destination.py:426-464).
    pub fn load_ratchets_signed(&mut self, data: &[u8]) -> Result<usize, DestinationError> {
        let identity = self.identity.as_ref().ok_or(DestinationError::NoIdentity)?;

        // Parse outer map: extract "signature" and "ratchets" blobs
        let (signature, inner) =
            msgpack_parse_signed_ratchets(data).ok_or(DestinationError::InvalidRatchetData)?;

        // Verify signature
        let valid = identity
            .verify(inner, signature)
            .map_err(|_| DestinationError::InvalidSignature)?;
        if !valid {
            return Err(DestinationError::InvalidSignature);
        }

        // Parse inner array: extract 32-byte private key blobs
        let keys =
            msgpack_parse_ratchet_array(inner).ok_or(DestinationError::InvalidRatchetData)?;

        let mut ratchets = Vec::with_capacity(keys.len());
        for key_bytes in keys {
            let mut key = [0u8; RATCHET_SIZE];
            key.copy_from_slice(key_bytes);
            ratchets.push(Ratchet::from_private_key_bytes(key, 0));
        }

        let count = ratchets.len();
        self.ratchets = ratchets;
        self.ratchets_dirty = false;

        if count > 0 {
            self.ratchets_enabled = true;
        }

        Ok(count)
    }

    /// Set the last ratchet rotation time.
    ///
    /// Used after loading persisted ratchets whose timestamps are from a
    /// previous session's monotonic domain and would block rotation.
    pub fn set_last_ratchet_time(&mut self, time_ms: u64) {
        self.last_ratchet_time_ms = time_ms;
    }

    /// Enforce ratchet-only decryption
    ///
    /// When enabled, packets not encrypted with a ratchet will be rejected
    /// even if they could be decrypted with the identity key.
    pub fn set_enforce_ratchets(&mut self, enforce: bool) {
        self.enforce_ratchets = enforce;
    }

    /// Check if ratchet enforcement is enabled
    pub fn enforces_ratchets(&self) -> bool {
        self.enforce_ratchets
    }

    // Encryption/Decryption
    /// Decrypt data received at this destination
    ///
    /// Tries decryption with each retained ratchet, then falls back to
    /// the identity key (unless ratchets are enforced).
    ///
    /// # Arguments
    /// * `ciphertext` - Encrypted data from an incoming packet
    ///
    /// # Returns
    /// Decrypted plaintext on success.
    ///
    /// # Errors
    /// * `NoIdentity` - Destination has no identity
    /// * `DecryptionFailed` - No ratchet or identity key could decrypt
    /// * `RatchetRequired` - Ratchets enforced but decrypted with identity key
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, DestinationError> {
        // GROUP destinations decrypt with the shared symmetric RNS Token.
        if self.dest_type == DestinationType::Group {
            return self.group_decrypt(ciphertext);
        }

        let identity = self.identity.as_ref().ok_or(DestinationError::NoIdentity)?;

        // Always try with identity fallback to know if ratchet was used
        let (plaintext, ratchet_id) =
            identity.decrypt_with_ratchets(ciphertext, &self.ratchets, true)?;

        // If ratchets are enforced and we decrypted without a ratchet, reject
        if self.enforce_ratchets && ratchet_id.is_none() {
            crate::tracing::debug!(
                "Packet rejected: ratchet enforcement active, \
                 packet encrypted without ratchet key"
            );
            return Err(DestinationError::RatchetRequired);
        }

        Ok(plaintext)
    }

    /// Snapshot everything needed to decrypt inbound packets for this Single
    /// destination WITHOUT holding a borrow of the node (leviculum#29 stages
    /// 2-3): the identity and the retained ratchets. `None` for non-Single
    /// destinations or ones without an identity.
    ///
    /// The snapshot decrypt is self-authenticating (the token HMAC must
    /// verify), so a stale snapshot can only FAIL to decrypt — the caller then
    /// falls back to the in-lock path against fresh state. It can never
    /// produce a wrong plaintext. Deliberately NOT snapshotted: the
    /// enforce-ratchets policy — the memo consume site applies the LIVE
    /// policy via the `ratchet_used` tag, so a stale snapshot cannot bypass
    /// an enforcement change either.
    pub fn export_decryptor(&self) -> Option<SingleDestDecryptor> {
        if self.dest_type != DestinationType::Single {
            return None;
        }
        let identity = self.identity.as_ref()?.clone();
        Some(SingleDestDecryptor {
            identity,
            ratchets: self.ratchets.clone(),
        })
    }

    /// Encrypt data for sending to another destination
    ///
    /// For OUT destinations, encrypts using the target identity and
    /// optionally a known ratchet public key.
    ///
    /// # Arguments
    /// * `plaintext` - Data to encrypt
    /// * `ratchet_public` - Optional ratchet public key from target's announce
    /// * `rng` - Random number generator
    ///
    /// # Returns
    /// Ciphertext that can be decrypted by the target destination.
    ///
    /// # Errors
    /// * `NoIdentity` - Destination has no identity to encrypt for
    pub fn encrypt(
        &self,
        plaintext: &[u8],
        ratchet_public: Option<&[u8; RATCHET_SIZE]>,
        rng: &mut impl CryptoRngCore,
    ) -> Result<Vec<u8>, DestinationError> {
        // GROUP destinations encrypt with the shared symmetric RNS Token. The
        // ratchet key is not used for GROUP; the random IV comes from `rng`.
        if self.dest_type == DestinationType::Group {
            return self.group_encrypt(plaintext, rng);
        }

        let identity = self.identity.as_ref().ok_or(DestinationError::NoIdentity)?;

        Ok(identity.encrypt_for_destination(plaintext, ratchet_public, rng)?)
    }

    /// Encrypt with the GROUP shared key, producing RNS Token wire bytes.
    ///
    /// Layout (Python `RNS.Cryptography.Token`): `IV(16) || AES-256-CBC(PKCS7)
    /// || HMAC-SHA256(32)`, HMAC computed over `IV || ciphertext`.
    fn group_encrypt(
        &self,
        plaintext: &[u8],
        rng: &mut impl CryptoRngCore,
    ) -> Result<Vec<u8>, DestinationError> {
        let key = self
            .group_key
            .as_ref()
            .ok_or(DestinationError::NoGroupKey)?;

        let mut iv = [0u8; 16];
        rng.fill_bytes(&mut iv);

        // token length = IV(16) + padded ciphertext + HMAC(32)
        let padded_len = ((plaintext.len() / 16) + 1) * 16;
        let mut out = vec![0u8; 16 + padded_len + 32];

        let n = encrypt_token(key, &iv, plaintext, &mut out)
            .map_err(|_| DestinationError::EncryptionFailed)?;
        out.truncate(n);
        Ok(out)
    }

    /// Decrypt RNS Token wire bytes with the GROUP shared key.
    fn group_decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, DestinationError> {
        let key = self
            .group_key
            .as_ref()
            .ok_or(DestinationError::NoGroupKey)?;

        // Plaintext is never longer than the ciphertext body.
        let mut out = vec![0u8; ciphertext.len()];
        let n = decrypt_token(key, ciphertext, &mut out)
            .map_err(|_| DestinationError::DecryptionFailed)?;
        out.truncate(n);
        Ok(out)
    }

    /// Reject a dot in `app_name` or in any aspect (Codeberg #163).
    ///
    /// Mirrors the reference's two `ValueError`s in `Destination.expand_name`
    /// (`reference/Reticulum/RNS/Destination.py:102` for the app name, `:106`
    /// for each aspect; `__init__` repeats the app-name check at `:151`).
    /// Public so callers assembling names from runtime input can validate
    /// before reaching [`Self::compute_name_hash`], which stays infallible for
    /// the compile-time-constant well-known names it is used with in-tree.
    pub fn validate_name_components(
        app_name: &str,
        aspects: &[&str],
    ) -> Result<(), DestinationError> {
        if app_name.contains('.') {
            return Err(DestinationError::DotInAppName);
        }
        for aspect in aspects {
            if aspect.contains('.') {
                return Err(DestinationError::DotInAspect);
            }
        }
        Ok(())
    }

    /// Compute the name hash from app_name and aspects.
    ///
    /// The name hash is the first 10 bytes of SHA256(app_name.aspect1.aspect2...).
    ///
    /// Does not validate the components: the reference's equivalent
    /// (`Destination.expand_name`) raises on a dot, but every in-tree caller
    /// passes compile-time-constant well-known names. Validate runtime-supplied
    /// components with [`Self::validate_name_components`] first (Codeberg #163).
    pub fn compute_name_hash(app_name: &str, aspects: &[&str]) -> [u8; NAME_HASHBYTES] {
        let mut full_name = String::from(app_name);
        for aspect in aspects {
            full_name.push('.');
            full_name.push_str(aspect);
        }

        let hash = sha256(full_name.as_bytes());
        let mut name_hash = [0u8; NAME_HASHBYTES];
        name_hash.copy_from_slice(&hash[..NAME_HASHBYTES]);
        name_hash
    }

    /// Compute the destination hash from name_hash and identity_hash.
    ///
    /// destination_hash = truncated_hash(name_hash + identity_hash)
    pub fn compute_destination_hash(
        name_hash: &[u8; NAME_HASHBYTES],
        identity_hash: &[u8; IDENTITY_HASHBYTES],
    ) -> DestinationHash {
        let mut combined = [0u8; NAME_HASHBYTES + IDENTITY_HASHBYTES];
        combined[..NAME_HASHBYTES].copy_from_slice(name_hash);
        combined[NAME_HASHBYTES..].copy_from_slice(identity_hash);
        DestinationHash::new(truncated_hash(&combined))
    }

    /// Create a signed announce packet for this destination.
    ///
    /// Announces inform the network about this destination's presence.
    /// Only IN destinations can announce (they receive traffic).
    ///
    /// If ratchets are enabled, the announce will include the current ratchet
    /// public key and the context_flag will be set. The ratchet will be rotated
    /// if the rotation interval has passed.
    ///
    /// # Arguments
    /// * `app_data` - Optional application-specific data (max ~350 bytes)
    /// * `rng` - Random number generator
    /// * `now_ms` - Monotonic timestamp in milliseconds (ratchet rotation)
    /// * `emission_secs` - Wire emission timestamp in unix seconds. Peers
    ///   order same-destination paths by this field across our restarts
    ///   (Python Transport.py:1772/1809), so it must come from
    ///   `Transport::emission_secs` (wall clock or learned timebase), never
    ///   from the monotonic clock (Codeberg #155).
    ///
    /// # Errors
    /// * `OnlySingleCanAnnounce` - Only SINGLE destinations can announce
    /// * `WrongDirection` - OUT destinations cannot announce
    /// * `NoIdentity` - Destination has no identity
    /// * `SigningFailed` - Signature could not be created
    ///
    /// # Example
    /// ```
    /// use leviculum_core::{Destination, DestinationType, Direction};
    /// use leviculum_core::identity::Identity;
    /// use rand_core::OsRng;
    ///
    /// let identity = Identity::generate(&mut OsRng);
    /// let mut dest = Destination::new(
    ///     Some(identity),
    ///     Direction::In,
    ///     DestinationType::Single,
    ///     "app",
    ///     &["echo"],
    /// ).unwrap();
    ///
    /// let packet = dest.announce(Some(b"my-data"), &mut OsRng, 12_000, 1704067200).unwrap();
    /// assert_eq!(packet.destination_hash, *dest.hash());
    /// ```
    pub fn announce(
        &mut self,
        app_data: Option<&[u8]>,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
        emission_secs: u64,
    ) -> Result<Packet, AnnounceError> {
        // Explicit-hash destinations must never be announced: the announce would
        // carry a destination_hash that does not match
        // truncated_hash(name_hash || identity_hash), which every Python-RNS
        // peer recomputes and rejects. Reachability is preserved via direct
        // link (opaque hash on the wire); only the announce path is refused.
        if self.explicit_hash {
            return Err(AnnounceError::ExplicitHashCannotAnnounce);
        }

        // Only SINGLE destinations can announce (per Python Reticulum spec)
        if self.dest_type != DestinationType::Single {
            return Err(AnnounceError::OnlySingleCanAnnounce);
        }

        // Only IN destinations can announce
        if self.direction != Direction::In {
            return Err(AnnounceError::WrongDirection);
        }

        // Check identity exists early
        if self.identity.is_none() {
            return Err(AnnounceError::NoIdentity);
        }

        // Explicit app_data becomes the default; `None` falls back to it
        // (Python `Destination.py` default_app_data). Re-announce paths pass
        // `None` and must reproduce the destination's last announce, not
        // strip it.
        if let Some(data) = app_data {
            if self.default_app_data.as_deref() != Some(data) {
                self.default_app_data = Some(data.to_vec());
            }
        }
        let effective_app_data = self.default_app_data.clone();
        let app_data = effective_app_data.as_deref();

        // Rotate ratchet if needed
        self.rotate_ratchet_if_needed(rng, now_ms);

        // Get current ratchet (if enabled)
        let ratchet = self.current_ratchet_public();
        let has_ratchet = ratchet.is_some();

        // Reject an over-budget announce here, at compose time, with the byte
        // counts the caller needs. Packing would otherwise fail with a
        // buffer-relative `PacketError::TooShort` — named from the buffer's
        // point of view, the exact opposite of the caller's problem.
        let app_data_len = app_data.map_or(0, <[u8]>::len);
        let budget = crate::announce::announce_app_data_budget(has_ratchet);
        if app_data_len > budget {
            return Err(AnnounceError::PacketTooLarge {
                packed: crate::constants::HEADER_MINSIZE
                    + crate::announce::announce_payload_fixed_len(has_ratchet)
                    + app_data_len,
                mtu: crate::constants::MTU,
                app_data: app_data_len,
                budget,
            });
        }

        // Get identity (safe because we checked above)
        let identity = self.identity.as_ref().unwrap();

        // Build the signed payload with optional ratchet
        let payload = build_announce_payload(
            identity,
            self.hash.as_bytes(),
            &self.name_hash,
            ratchet.as_ref(),
            app_data,
            rng,
            emission_secs,
        )?;

        // Create the packet
        let packet = Packet {
            flags: PacketFlags {
                ifac_flag: false,
                header_type: HeaderType::Type1,
                context_flag: has_ratchet, // Set if ratchet is present
                transport_type: TransportType::Broadcast,
                dest_type: self.dest_type,
                packet_type: PacketType::Announce,
            },
            hops: 0,
            transport_id: None,
            destination_hash: self.hash.into_bytes(),
            context: PacketContext::None,
            data: PacketData::Owned(payload),
        };

        Ok(packet)
    }
}

// Hand-rolled msgpack helpers (no_std compatible)
//
// These encode/decode only the exact msgpack types needed for the signed
// ratchet format. Not a general-purpose msgpack library.

/// Encode an array of ratchet private keys as msgpack.
///
/// Format: array header + N × bin8(32).
fn msgpack_encode_ratchet_array(ratchets: &[Ratchet]) -> Vec<u8> {
    let n = ratchets.len();
    // Each entry: 1 (bin8) + 1 (len=32) + 32 (data) = 34 bytes
    let mut buf = Vec::with_capacity(3 + n * 34);

    // Array header
    if n <= 15 {
        buf.push(0x90 | (n as u8));
    } else {
        buf.push(0xdc); // array16
        buf.extend_from_slice(&(n as u16).to_be_bytes());
    }

    // Elements: bin8(32) for each private key
    for ratchet in ratchets {
        buf.push(0xc4); // bin8
        buf.push(RATCHET_SIZE as u8); // 32
        buf.extend_from_slice(&ratchet.private_key_bytes());
    }

    buf
}

/// Encode the outer signed ratchets map as msgpack.
///
/// Format: fixmap(2) { "signature": bin8(64), "ratchets": bin16/bin32(inner) }
fn msgpack_encode_signed_ratchets(signature: &[u8; 64], inner: &[u8]) -> Option<Vec<u8>> {
    // Outer map size: 1 (fixmap) + 10 (fixstr "signature") + 2+64 (bin8 sig)
    //               + 9 (fixstr "ratchets") + 3..5+inner.len() (bin16/bin32)
    let mut buf = Vec::with_capacity(1 + 10 + 66 + 9 + 5 + inner.len());

    buf.push(0x82); // fixmap(2)

    // Key: "signature" (9 bytes)
    buf.push(0xa9); // fixstr(9)
    buf.extend_from_slice(b"signature");

    // Value: bin8(64). Ed25519 signature
    buf.push(0xc4); // bin8
    buf.push(64);
    buf.extend_from_slice(signature);

    // Key: "ratchets" (8 bytes)
    buf.push(0xa8); // fixstr(8)
    buf.extend_from_slice(b"ratchets");

    // Value: bin16 or bin32, inner msgpack array
    let inner_len = inner.len();
    if inner_len <= 0xFFFF {
        buf.push(0xc5); // bin16
        buf.extend_from_slice(&(inner_len as u16).to_be_bytes());
    } else {
        buf.push(0xc6); // bin32
        buf.extend_from_slice(&(inner_len as u32).to_be_bytes());
    }
    buf.extend_from_slice(inner);

    Some(buf)
}

/// Parse the outer signed ratchets map from msgpack.
///
/// Returns (signature_bytes, inner_ratchets_bytes) or None if malformed.
fn msgpack_parse_signed_ratchets(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut pos = 0;

    // fixmap(2)
    if read_byte(data, &mut pos)? != 0x82 {
        return None;
    }

    let mut signature: Option<&[u8]> = None;
    let mut ratchets: Option<&[u8]> = None;

    for _ in 0..2 {
        let key = read_msgpack_str(data, &mut pos)?;
        let value = read_msgpack_bin(data, &mut pos)?;

        if key == b"signature" {
            signature = Some(value);
        } else if key == b"ratchets" {
            ratchets = Some(value);
        }
    }

    Some((signature?, ratchets?))
}

/// Parse an array of 32-byte binary blobs from msgpack.
///
/// Returns slices into the original data.
fn msgpack_parse_ratchet_array(data: &[u8]) -> Option<Vec<&[u8]>> {
    let mut pos = 0;
    let count = read_msgpack_array_len(data, &mut pos)?;

    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        let blob = read_msgpack_bin(data, &mut pos)?;
        if blob.len() != RATCHET_SIZE {
            return None;
        }
        keys.push(blob);
    }

    Some(keys)
}

// Low-level msgpack readers
/// Take `n` bytes at `*pos`, advancing `*pos` past them.
///
/// Same guard, and the same reason, as `resource::msgpack::take` (Codeberg
/// #267): `read_msgpack_bin` below takes a full `u32` off a bin32 header, and
/// on the 32-bit firmware target the old `*pos + len > data.len()` wrapped
/// below `data.len()`, so the guard passed and the slice behind it panicked.
///
/// This decoder is fed from local storage rather than the wire
/// (`load_ratchets_signed`, via `NodeCore` at destination registration), and
/// the outer map is parsed *before* the Ed25519 signature is verified — so a
/// corrupt or hostile ratchet store reaches it unauthenticated.
fn take<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = (*pos).checked_add(n)?;
    let taken = data.get(*pos..end)?;
    *pos = end;
    Some(taken)
}

fn read_byte(data: &[u8], pos: &mut usize) -> Option<u8> {
    let b = *data.get(*pos)?;
    *pos += 1;
    Some(b)
}

fn read_be_u16(data: &[u8], pos: &mut usize) -> Option<u16> {
    if *pos + 2 > data.len() {
        return None;
    }
    let val = u16::from_be_bytes([data[*pos], data[*pos + 1]]);
    *pos += 2;
    Some(val)
}

fn read_be_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > data.len() {
        return None;
    }
    let val = u32::from_be_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    Some(val)
}

fn read_msgpack_str<'a>(data: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let tag = read_byte(data, pos)?;
    let len = if tag & 0xe0 == 0xa0 {
        // fixstr: 0xa0..0xbf, length in lower 5 bits
        (tag & 0x1f) as usize
    } else if tag == 0xd9 {
        // str8
        read_byte(data, pos)? as usize
    } else if tag == 0xda {
        // str16
        read_be_u16(data, pos)? as usize
    } else {
        return None;
    };

    if *pos + len > data.len() {
        return None;
    }
    let s = &data[*pos..*pos + len];
    *pos += len;
    Some(s)
}

fn read_msgpack_bin<'a>(data: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let tag = read_byte(data, pos)?;
    let len = if tag == 0xc4 {
        // bin8
        read_byte(data, pos)? as usize
    } else if tag == 0xc5 {
        // bin16
        read_be_u16(data, pos)? as usize
    } else if tag == 0xc6 {
        // bin32
        read_be_u32(data, pos)? as usize
    } else {
        return None;
    };

    take(data, pos, len)
}

fn read_msgpack_array_len(data: &[u8], pos: &mut usize) -> Option<usize> {
    let tag = read_byte(data, pos)?;
    if tag & 0xf0 == 0x90 {
        // fixarray: 0x90..0x9f
        Some((tag & 0x0f) as usize)
    } else if tag == 0xdc {
        // array16
        Some(read_be_u16(data, pos)? as usize)
    } else if tag == 0xdd {
        // array32
        Some(read_be_u32(data, pos)? as usize)
    } else {
        None
    }
}

/// A detachable decrypt context for one Single destination — the key material
/// [`Destination::export_decryptor`] snapshots so the CPU-heavy X25519 ECDH +
/// HKDF decrypt can run off the node lock (leviculum#29). `Send + Sync`; cheap
/// to clone per use via `Arc`.
pub struct SingleDestDecryptor {
    identity: Identity,
    ratchets: Vec<Ratchet>,
}

impl SingleDestDecryptor {
    /// Decrypt exactly as [`Destination::decrypt`] would for a Single
    /// destination, against the snapshotted keys. Returns the plaintext and
    /// whether a ratchet key was used — the memo consume site needs the tag
    /// to apply the LIVE enforce-ratchets policy. `None` on any failure —
    /// including a post-snapshot ratchet rotation — in which case the caller
    /// must fall back to the in-lock decrypt.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Option<(Vec<u8>, bool)> {
        let (plaintext, ratchet_id) = self
            .identity
            .decrypt_with_ratchets(ciphertext, &self.ratchets, true)
            .ok()?;
        Some((plaintext, ratchet_id.is_some()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::announce::ReceivedAnnounce;
    use rand_core::OsRng;

    const TEST_TIME_MS: u64 = 1704067200000; // 2024-01-01 00:00:00 UTC

    #[test]
    fn test_destination_creation() {
        let identity = Identity::generate(&mut OsRng);
        let dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["echo"],
        )
        .unwrap();

        assert_eq!(dest.dest_type(), DestinationType::Single);
        assert_eq!(dest.direction(), Direction::In);
        assert_eq!(dest.hash().as_bytes().len(), TRUNCATED_HASHBYTES);
    }

    #[test]
    fn test_name_hash() {
        let hash1 = Destination::compute_name_hash("app", &["aspect1"]);
        let hash2 = Destination::compute_name_hash("app", &["aspect2"]);
        let hash3 = Destination::compute_name_hash("app", &["aspect1"]);

        assert_ne!(hash1, hash2);
        assert_eq!(hash1, hash3);
    }

    #[test]
    fn test_destination_type_conversion() {
        assert_eq!(DestinationType::try_from(0x00), Ok(DestinationType::Single));
        assert_eq!(DestinationType::try_from(0x01), Ok(DestinationType::Group));
        assert_eq!(DestinationType::try_from(0x02), Ok(DestinationType::Plain));
        assert_eq!(DestinationType::try_from(0x03), Ok(DestinationType::Link));
        assert_eq!(DestinationType::try_from(0x04), Err(()));
    }

    #[test]
    fn test_destination_announce_creates_valid_packet() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["echo"],
        )
        .unwrap();

        let packet = dest
            .announce(
                Some(b"hello"),
                &mut OsRng,
                TEST_TIME_MS,
                TEST_TIME_MS / 1000,
            )
            .unwrap();

        // Verify packet structure
        assert_eq!(packet.flags.packet_type, PacketType::Announce);
        assert_eq!(packet.flags.transport_type, TransportType::Broadcast);
        assert_eq!(packet.flags.dest_type, DestinationType::Single);
        assert_eq!(packet.flags.header_type, HeaderType::Type1);
        assert!(!packet.flags.context_flag); // No ratchet
        assert_eq!(packet.hops, 0);
        assert_eq!(packet.destination_hash, *dest.hash());

        // Parse and verify the announce
        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();
        assert!(announce.verify_destination_hash());
        assert!(announce.verify_signature().unwrap());
        assert_eq!(announce.app_data(), b"hello");
    }

    // #16: with_explicit_hash overrides the routing index but the destination
    // can never be announced (guards Python-RNS announce compatibility).
    #[test]
    fn test_with_explicit_hash_sets_hash_and_blocks_announce() {
        let identity = Identity::generate(&mut OsRng);
        let explicit = [0x7Au8; 16];
        let mut dest = Destination::with_explicit_hash(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "cirisedge",
            &["fed"],
            explicit,
        )
        .unwrap();

        assert!(dest.is_explicit_hash());
        assert_eq!(dest.hash().as_bytes(), &explicit);
        // The override differs from the structural hash this dest would
        // otherwise carry (name_hash || identity_hash is not a preimage of it).
        let structural = Destination::compute_destination_hash(
            dest.name_hash(),
            dest.identity().unwrap().hash(),
        );
        assert_ne!(dest.hash(), &structural);

        assert!(matches!(
            dest.announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000),
            Err(AnnounceError::ExplicitHashCannotAnnounce)
        ));
    }

    #[test]
    fn test_destination_announce_out_direction_fails() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::Out, // OUT cannot announce
            DestinationType::Single,
            "testapp",
            &["echo"],
        )
        .unwrap();

        let result = dest.announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000);

        assert!(matches!(result, Err(AnnounceError::WrongDirection)));
    }

    #[test]
    fn test_destination_announce_plain_type_fails() {
        // PLAIN destinations cannot announce (only SINGLE can)
        let mut dest = Destination::new(
            None, // No identity (valid for PLAIN)
            Direction::In,
            DestinationType::Plain,
            "testapp",
            &["echo"],
        )
        .unwrap();

        let result = dest.announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000);

        // PLAIN destinations can't announce - OnlySingleCanAnnounce takes priority
        assert!(matches!(result, Err(AnnounceError::OnlySingleCanAnnounce)));
    }

    #[test]
    fn test_destination_announce_without_app_data() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["echo"],
        )
        .unwrap();

        let packet = dest
            .announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000)
            .unwrap();

        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();
        assert!(announce.app_data().is_empty());
        assert!(announce.validate().is_ok());
    }

    #[test]
    fn test_destination_announce_validates_correctly() {
        // Create destination and announce
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "myapp",
            &["service", "v1"],
        )
        .unwrap();

        let packet = dest
            .announce(
                Some(b"app-data"),
                &mut OsRng,
                TEST_TIME_MS,
                TEST_TIME_MS / 1000,
            )
            .unwrap();

        // Full validation should pass
        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();
        assert!(announce.validate().is_ok());

        // Computed hashes should match
        assert_eq!(announce.computed_destination_hash(), *dest.hash());
    }

    // Spec Compliance Tests
    #[test]
    fn test_plain_destination_cannot_have_identity() {
        let identity = Identity::generate(&mut OsRng);
        let result = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Plain,
            "test",
            &["echo"],
        );
        assert!(matches!(
            result,
            Err(DestinationError::PlainCannotHaveIdentity)
        ));
    }

    /// Codeberg #163: a dot inside `app_name` or an aspect is the dotted-name
    /// separator, so two different `(app_name, aspects)` tuples would expand to
    /// the same full name and collide on the same name hash — and therefore the
    /// same destination hash. The reference rejects the input instead, raising
    /// `ValueError("Dots can't be used in app names")` /
    /// `ValueError("Dots can't be used in aspects")`
    /// (`reference/Reticulum/RNS/Destination.py:102`, `:106`, and again at
    /// `:151` in `__init__`). Our analogue of a raised exception is an `Err`
    /// return from the only constructor.
    #[test]
    fn dotted_app_name_or_aspect_is_rejected_like_the_reference() {
        let identity = Identity::generate(&mut OsRng);
        assert!(matches!(
            Destination::new(
                Some(identity.clone()),
                Direction::In,
                DestinationType::Single,
                "test.app",
                &["echo"],
            ),
            Err(DestinationError::DotInAppName)
        ));
        assert!(matches!(
            Destination::new(
                Some(identity.clone()),
                Direction::In,
                DestinationType::Single,
                "test",
                &["echo.reply"],
            ),
            Err(DestinationError::DotInAspect)
        ));
        // Second aspect too: the reference checks every aspect in its loop.
        assert!(matches!(
            Destination::new(
                Some(identity.clone()),
                Direction::In,
                DestinationType::Single,
                "test",
                &["echo", "re.ply"],
            ),
            Err(DestinationError::DotInAspect)
        ));
        // The check must not reject legitimate names.
        assert!(Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["echo", "reply"],
        )
        .is_ok());
    }

    /// Codeberg #163: the collision the rejection prevents. Without it,
    /// `("a.b", ["c"])` and `("a", ["b", "c"])` expand to the same full name
    /// `a.b.c` (`Destination.py:104-107`) and therefore produce the same
    /// destination hash for the same identity.
    #[test]
    fn dotted_components_would_collide_on_the_same_destination_hash() {
        let identity = Identity::generate(&mut OsRng);
        let split = Destination::compute_name_hash("a", &["b", "c"]);
        let merged = Destination::compute_name_hash("a.b", &["c"]);
        assert_eq!(
            split, merged,
            "the dotted form collides — which is why the constructor rejects it"
        );
        assert_eq!(
            Destination::compute_destination_hash(&split, identity.hash()),
            Destination::compute_destination_hash(&merged, identity.hash()),
        );
    }

    #[test]
    fn test_plain_destination_without_identity_succeeds() {
        let result = Destination::new(
            None,
            Direction::In,
            DestinationType::Plain,
            "test",
            &["echo"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_single_out_requires_identity() {
        let result = Destination::new(
            None,
            Direction::Out,
            DestinationType::Single,
            "test",
            &["echo"],
        );
        assert!(matches!(
            result,
            Err(DestinationError::OutboundRequiresIdentity)
        ));
    }

    #[test]
    fn test_group_out_requires_identity() {
        let result = Destination::new(
            None,
            Direction::Out,
            DestinationType::Group,
            "test",
            &["echo"],
        );
        assert!(matches!(
            result,
            Err(DestinationError::OutboundRequiresIdentity)
        ));
    }

    #[test]
    fn test_only_single_can_announce() {
        let identity = Identity::generate(&mut OsRng);

        // GROUP destination cannot announce
        let mut group_dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Group,
            "test",
            &["echo"],
        )
        .unwrap();

        let result = group_dest.announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000);
        assert!(matches!(result, Err(AnnounceError::OnlySingleCanAnnounce)));
    }

    #[test]
    fn test_single_in_can_announce() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["echo"],
        )
        .unwrap();

        let result = dest.announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000);
        assert!(result.is_ok());
    }

    #[test]
    fn test_plain_out_allowed_without_identity() {
        // PLAIN OUT is valid without identity (for local broadcast)
        let result = Destination::new(
            None,
            Direction::Out,
            DestinationType::Plain,
            "test",
            &["broadcast"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_single_in_without_identity_allowed() {
        // SINGLE IN can exist without identity initially
        // (auto-creation would happen in a full implementation)
        let result = Destination::new(
            None,
            Direction::In,
            DestinationType::Single,
            "test",
            &["echo"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_group_in_allowed_with_identity() {
        let identity = Identity::generate(&mut OsRng);
        let result = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Group,
            "test",
            &["broadcast"],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_link_type_allowed_without_identity() {
        // LINK type is internal use, allowing various configs
        let result = Destination::new(
            None,
            Direction::In,
            DestinationType::Link,
            "test",
            &["link"],
        );
        assert!(result.is_ok());
    }

    // Ratchet Tests
    #[test]
    fn test_enable_ratchets() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        assert!(!dest.ratchets_enabled());
        assert!(dest.current_ratchet_public().is_none());
        assert_eq!(dest.ratchet_count(), 0);

        dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();

        assert!(dest.ratchets_enabled());
        assert!(dest.current_ratchet_public().is_some());
        assert_eq!(dest.ratchet_count(), 1);
    }

    #[test]
    fn test_enable_ratchets_on_out_fails() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::Out,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        let result = dest.enable_ratchets(&mut OsRng, TEST_TIME_MS);

        assert!(matches!(
            result,
            Err(DestinationError::CannotEnableRatchetsOnOut)
        ));
    }

    #[test]
    fn test_ratchet_rotation() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        // Use a shorter interval for testing
        dest.set_ratchet_interval(1000); // 1 second

        let mut time_ms = TEST_TIME_MS;
        dest.enable_ratchets(&mut OsRng, time_ms).unwrap();

        let first_ratchet = dest.current_ratchet_public().unwrap();

        // No rotation needed yet
        assert!(!dest.rotate_ratchet_if_needed(&mut OsRng, time_ms));
        assert_eq!(dest.current_ratchet_public().unwrap(), first_ratchet);

        // Advance time past interval
        time_ms += 2000; // 2 seconds later

        // Rotation should happen
        assert!(dest.rotate_ratchet_if_needed(&mut OsRng, time_ms));
        let second_ratchet = dest.current_ratchet_public().unwrap();
        assert_ne!(first_ratchet, second_ratchet);

        // Old ratchet should still be retained
        assert_eq!(dest.ratchet_count(), 2);
    }

    #[test]
    fn test_retained_ratchet_limit() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        // Set small limit for testing
        dest.set_retained_ratchets(3);
        dest.set_ratchet_interval(1000);

        let mut time_ms = TEST_TIME_MS;
        dest.enable_ratchets(&mut OsRng, time_ms).unwrap();
        assert_eq!(dest.ratchet_count(), 1);

        // Rotate 5 times
        for _ in 1..=5 {
            time_ms += 2000;
            dest.rotate_ratchet_if_needed(&mut OsRng, time_ms);
        }

        // Should only have 3 ratchets (limit enforced)
        assert_eq!(dest.ratchet_count(), 3);
    }

    #[test]
    fn test_encrypt_decrypt_with_ratchet() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();

        // Get ratchet public key (simulating what a sender would get from announce)
        let ratchet_pub = dest.current_ratchet_public().unwrap();

        // Encrypt using the destination's identity and ratchet
        let plaintext = b"Hello with forward secrecy!";
        let ciphertext = dest
            .encrypt(plaintext, Some(&ratchet_pub), &mut OsRng)
            .unwrap();

        // Decrypt
        let decrypted = dest.decrypt(&ciphertext).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_decrypt_with_old_ratchet() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        dest.set_ratchet_interval(1000);

        let mut time_ms = TEST_TIME_MS;
        dest.enable_ratchets(&mut OsRng, time_ms).unwrap();

        // Encrypt with current ratchet
        let first_ratchet = dest.current_ratchet_public().unwrap();
        let plaintext = b"Message encrypted with first ratchet";
        let ciphertext = dest
            .encrypt(plaintext, Some(&first_ratchet), &mut OsRng)
            .unwrap();

        // Rotate ratchet
        time_ms += 2000;
        dest.rotate_ratchet_if_needed(&mut OsRng, time_ms);

        // Should still be able to decrypt with old ratchet
        let decrypted = dest.decrypt(&ciphertext).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_enforce_ratchets() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();
        dest.set_enforce_ratchets(true);

        // Encrypt WITHOUT ratchet (using identity key)
        let plaintext = b"No ratchet used";
        let ciphertext = dest.encrypt(plaintext, None, &mut OsRng).unwrap();

        // Should fail because ratchets are enforced
        let result = dest.decrypt(&ciphertext);
        assert!(matches!(result, Err(DestinationError::RatchetRequired)));
    }

    #[test]
    fn test_decrypt_without_ratchet_when_not_enforced() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();

        // Encrypt WITHOUT ratchet (using identity key)
        let plaintext = b"No ratchet used";
        let ciphertext = dest.encrypt(plaintext, None, &mut OsRng).unwrap();

        // Should succeed because ratchets are not enforced
        let decrypted = dest.decrypt(&ciphertext).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_ratchet_settings() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        // Test default settings
        assert_eq!(dest.ratchet_interval(), DEFAULT_INTERVAL_MS);
        assert_eq!(dest.retained_ratchet_count(), DEFAULT_RETAINED_RATCHETS);
        assert!(!dest.enforces_ratchets());

        // Change settings
        dest.set_ratchet_interval(60000);
        dest.set_retained_ratchets(100);
        dest.set_enforce_ratchets(true);

        assert_eq!(dest.ratchet_interval(), 60000);
        assert_eq!(dest.retained_ratchet_count(), 100);
        assert!(dest.enforces_ratchets());
    }

    #[test]
    fn test_no_rotation_when_ratchets_disabled() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        // Don't enable ratchets
        let far_future = TEST_TIME_MS + 100000000;
        assert!(!dest.rotate_ratchet_if_needed(&mut OsRng, far_future));
    }

    // Ratcheted Announce Tests
    #[test]
    fn test_announce_with_ratchet() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();

        let packet = dest
            .announce(
                Some(b"test-data"),
                &mut OsRng,
                TEST_TIME_MS,
                TEST_TIME_MS / 1000,
            )
            .unwrap();

        // context_flag should be set
        assert!(packet.flags.context_flag);

        // Parse and verify
        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();
        assert!(announce.has_ratchet());
        assert!(announce.validate().is_ok());
        assert_eq!(announce.app_data(), b"test-data");

        // Ratchet in announce should match destination's current ratchet
        assert_eq!(
            announce.ratchet().unwrap(),
            &dest.current_ratchet_public().unwrap()
        );
    }

    #[test]
    fn test_announce_without_ratchet_when_disabled() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["noratchet"],
        )
        .unwrap();

        // Don't enable ratchets

        let packet = dest
            .announce(
                Some(b"test-data"),
                &mut OsRng,
                TEST_TIME_MS,
                TEST_TIME_MS / 1000,
            )
            .unwrap();

        // context_flag should NOT be set
        assert!(!packet.flags.context_flag);

        // Parse and verify
        let announce = ReceivedAnnounce::from_packet(&packet).unwrap();
        assert!(!announce.has_ratchet());
        assert!(announce.ratchet().is_none());
        assert!(announce.validate().is_ok());
    }

    #[test]
    fn test_announce_rotates_ratchet() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["ratchet"],
        )
        .unwrap();

        dest.set_ratchet_interval(1000);

        let mut time_ms = TEST_TIME_MS;
        dest.enable_ratchets(&mut OsRng, time_ms).unwrap();

        let first_ratchet = dest.current_ratchet_public().unwrap();

        // First announce - uses first ratchet
        let packet1 = dest
            .announce(None, &mut OsRng, time_ms, time_ms / 1000)
            .unwrap();
        let announce1 = ReceivedAnnounce::from_packet(&packet1).unwrap();
        assert_eq!(announce1.ratchet().unwrap(), &first_ratchet);

        // Advance time to trigger rotation
        time_ms += 2000;

        // Second announce - should rotate and use new ratchet
        let packet2 = dest
            .announce(None, &mut OsRng, time_ms, time_ms / 1000)
            .unwrap();
        let announce2 = ReceivedAnnounce::from_packet(&packet2).unwrap();

        let second_ratchet = dest.current_ratchet_public().unwrap();
        assert_ne!(first_ratchet, second_ratchet);
        assert_eq!(announce2.ratchet().unwrap(), &second_ratchet);
    }

    /// #159 tranche 1: pin the SEMANTICS of the generated `name_hash` and the
    /// derived destination hash against values computed by the vendored Python
    /// reference, not merely against our own reader. A peer recomputes
    /// `truncated_hash(name_hash + truncated_hash(public_key))` from the
    /// announce and drops it on mismatch (`Identity.validate_announce`; our
    /// `ReceivedAnnounce::validate`), so these exact bytes decide announce
    /// acceptance mesh-wide.
    ///
    /// Reference vectors generated 2026-08-02 by executing the vendored
    /// `reference/Reticulum` (RNS importable, values confirmed by
    /// `RNS.Destination.expand_name` / `RNS.Destination.hash` /
    /// `RNS.Identity.full_hash`):
    ///
    /// ```text
    /// full_hash("audit.kat.field")[:10]        = 836710f8649dd9fed860
    /// full_hash("audit.käse".utf8)[:10]        = 921ee2ed9768519128dd
    /// pub = bytes(range(64))
    /// full_hash(pub)[:16]                      = fdeab9acf3710362bd2658cdc9a29e8f
    /// full_hash(name_hash + id_hash)[:16]      = f000a6e0bcdb026f6dbc6eed918fab21
    /// ```
    ///
    /// Pinned rules (Destination.py:120/:130/:189): the name-hash material is
    /// the dot-joined `app_name.aspects` WITHOUT the identity hexhash
    /// (`expand_name(None, ...)`), encoded as UTF-8; the destination hash
    /// appends the 16-byte identity hash to the 10-byte name hash.
    #[test]
    fn announce_name_and_destination_hash_match_python_reference() {
        let name_hash = Destination::compute_name_hash("audit", &["kat", "field"]);
        assert_eq!(
            name_hash,
            [0x83, 0x67, 0x10, 0xf8, 0x64, 0x9d, 0xd9, 0xfe, 0xd8, 0x60],
            "name_hash must match the Python reference value"
        );

        // Non-ASCII aspect: Python hashes the UTF-8 encoding of the name.
        assert_eq!(
            Destination::compute_name_hash("audit", &["käse"]),
            [0x92, 0x1e, 0xe2, 0xed, 0x97, 0x68, 0x51, 0x91, 0x28, 0xdd],
            "name_hash must hash the UTF-8 bytes of the full name"
        );

        // Identity hash: truncated_hash over the 64-byte X25519||Ed25519 key.
        let mut pubkey = [0u8; 64];
        for (i, b) in pubkey.iter_mut().enumerate() {
            *b = i as u8;
        }
        let identity_hash = truncated_hash(&pubkey);
        assert_eq!(
            identity_hash,
            [
                0xfd, 0xea, 0xb9, 0xac, 0xf3, 0x71, 0x03, 0x62, 0xbd, 0x26, 0x58, 0xcd, 0xc9, 0xa2,
                0x9e, 0x8f
            ],
            "identity hash must match the Python reference value"
        );

        let dest_hash = Destination::compute_destination_hash(&name_hash, &identity_hash);
        assert_eq!(
            dest_hash.as_bytes(),
            &[
                0xf0, 0x00, 0xa6, 0xe0, 0xbc, 0xdb, 0x02, 0x6f, 0x6d, 0xbc, 0x6e, 0xed, 0x91, 0x8f,
                0xab, 0x21
            ],
            "destination hash must match the Python reference value"
        );
    }

    /// #159 tranche 1: pin the announce signature's covered bytes by
    /// reconstructing them INDEPENDENTLY from the wire bytes, in the exact
    /// order the reference composes them (Destination.py:297-298:
    /// `hash + public_key + name_hash + random_hash + ratchet [+ app_data]`),
    /// and verifying with raw Ed25519 against the key half at payload bytes
    /// 32..64. A Python peer rejects the announce if any field is missing,
    /// reordered, or the Ed25519 half sits elsewhere — the existing
    /// `verify_signature` tests share `build_signed_data` with the writer and
    /// would stay green if writer and reader drifted together (the #155
    /// failure class).
    #[test]
    fn announce_signature_covers_reference_byte_order_on_the_wire() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        for with_ratchet in [false, true] {
            let identity = Identity::generate(&mut OsRng);
            let mut dest = Destination::new(
                Some(identity),
                Direction::In,
                DestinationType::Single,
                "audit",
                &["sig"],
            )
            .unwrap();
            if with_ratchet {
                dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();
            }

            let app_data = b"sig-audit";
            let packet = dest
                .announce(
                    Some(app_data),
                    &mut OsRng,
                    TEST_TIME_MS,
                    TEST_TIME_MS / 1000,
                )
                .unwrap();
            let mut buf = [0u8; 500];
            let len = packet.pack(&mut buf).unwrap();
            let wire = &buf[..len];

            // Type1 wire layout: flags(1) + hops(1) + dest_hash(16) + context(1).
            let header_dest_hash = &wire[2..18];
            let payload = &wire[19..];

            // Reference payload layout (Destination.py:301):
            // public_key(64) + name_hash(10) + random_hash(10) + [ratchet(32)]
            // + signature(64) + app_data.
            let (ratchet, signature, wire_app_data): (&[u8], &[u8], &[u8]) = if with_ratchet {
                (&payload[84..116], &payload[116..180], &payload[180..])
            } else {
                (&[], &payload[84..148], &payload[148..])
            };
            assert_eq!(wire_app_data, app_data, "app_data must ride verbatim");

            let mut signed = Vec::new();
            signed.extend_from_slice(header_dest_hash);
            signed.extend_from_slice(&payload[..64]); // public_key
            signed.extend_from_slice(&payload[64..74]); // name_hash
            signed.extend_from_slice(&payload[74..84]); // random_hash
            signed.extend_from_slice(ratchet);
            signed.extend_from_slice(wire_app_data);

            let ed25519_half: [u8; 32] = payload[32..64].try_into().unwrap();
            let vk = VerifyingKey::from_bytes(&ed25519_half)
                .expect("payload bytes 32..64 must be the Ed25519 verifying key");
            let sig = Signature::from_bytes(signature.try_into().unwrap());
            vk.verify(&signed, &sig).unwrap_or_else(|_| {
                panic!("reference-composed signed data must verify (with_ratchet={with_ratchet})")
            });

            // The pin must bite: dropping the destination hash from the front
            // (what a writer forgetting the header hash would sign) must fail.
            assert!(
                vk.verify(&signed[16..], &sig).is_err(),
                "signature must actually cover the destination hash"
            );
        }
    }

    /// #159 tranche 1: pin the dedup semantics of the random half of the
    /// announce `random_hash`. A peer keeps the last 64 announce blobs per
    /// destination and refuses to re-adopt a path whose 10-byte blob it has
    /// already seen (Transport.py:1772/1797/1810, MAX_RANDOM_BLOBS at :98) —
    /// two same-second announces with equal blobs would make every peer treat
    /// the second one as a replay. So consecutive emissions in the SAME
    /// emission second must differ in the random half (bytes 0..5) while the
    /// timestamp half (bytes 5..10, pinned by the #155/#160/#161 chain) stays
    /// equal.
    #[test]
    fn announce_random_blob_is_a_fresh_dedup_key_each_emission() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "audit",
            &["blob"],
        )
        .unwrap();

        let emission_secs = TEST_TIME_MS / 1000;
        let blob = |dest: &mut Destination| -> [u8; crate::constants::RANDOM_HASHBYTES] {
            let packet = dest
                .announce(None, &mut OsRng, TEST_TIME_MS, emission_secs)
                .unwrap();
            let mut buf = [0u8; 500];
            let _ = packet.pack(&mut buf).unwrap();
            // random_hash on the wire: header(19) + public_key(64) + name_hash(10).
            buf[19 + 74..19 + 84].try_into().unwrap()
        };

        let first = blob(&mut dest);
        let second = blob(&mut dest);

        assert_ne!(
            first[..5],
            second[..5],
            "same-second announces must carry distinct dedup blobs"
        );
        assert_eq!(
            first[5..],
            second[5..],
            "timestamp half must be identical for the same emission second"
        );
        assert_ne!(first, second, "the full 10-byte peer dedup key must differ");
    }

    // Proof Strategy Tests
    #[test]
    fn test_proof_strategy_default() {
        let identity = Identity::generate(&mut OsRng);
        let dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["proof"],
        )
        .unwrap();

        assert_eq!(dest.proof_strategy(), ProofStrategy::None);
    }

    #[test]
    fn test_proof_strategy_setter() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "test",
            &["proof"],
        )
        .unwrap();

        dest.set_proof_strategy(ProofStrategy::All);
        assert_eq!(dest.proof_strategy(), ProofStrategy::All);

        dest.set_proof_strategy(ProofStrategy::App);
        assert_eq!(dest.proof_strategy(), ProofStrategy::App);

        dest.set_proof_strategy(ProofStrategy::None);
        assert_eq!(dest.proof_strategy(), ProofStrategy::None);
    }

    #[test]
    fn test_proof_strategy_enum_values() {
        // Verify the enum values match the protocol constants
        assert_eq!(ProofStrategy::None as u8, 0x21);
        assert_eq!(ProofStrategy::App as u8, 0x22);
        assert_eq!(ProofStrategy::All as u8, 0x23);
    }

    // DestinationHash Tests
    #[test]
    fn test_destination_hash_construction() {
        let bytes = [0x42u8; 16];
        let hash = DestinationHash::new(bytes);
        assert_eq!(*hash.as_bytes(), bytes);
    }

    #[test]
    fn test_destination_hash_into_bytes_roundtrip() {
        let bytes = [0xAB; 16];
        let hash = DestinationHash::new(bytes);
        assert_eq!(hash.into_bytes(), bytes);
    }

    #[test]
    fn test_destination_hash_from_array() {
        let bytes = [0x01; 16];
        let hash: DestinationHash = bytes.into();
        assert_eq!(*hash.as_bytes(), bytes);

        let back: [u8; 16] = hash.into();
        assert_eq!(back, bytes);
    }

    #[test]
    fn test_destination_hash_display() {
        let hash = DestinationHash::new([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef,
        ]);
        let display = alloc::format!("{}", hash);
        assert_eq!(display, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn test_destination_hash_debug() {
        let hash = DestinationHash::new([0xAA; 16]);
        let debug = alloc::format!("{:?}", hash);
        assert!(debug.starts_with("DestinationHash("));
        assert!(debug.contains("aa"));
    }

    #[test]
    fn test_destination_hash_equality_with_raw() {
        let bytes = [0x42; 16];
        let hash = DestinationHash::new(bytes);
        assert_eq!(hash, bytes);
        assert_eq!(bytes, hash);
    }

    #[test]
    fn test_destination_hash_inequality() {
        let hash1 = DestinationHash::new([0x01; 16]);
        let hash2 = DestinationHash::new([0x02; 16]);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_destination_hash_as_ref() {
        let hash = DestinationHash::new([0x42; 16]);
        let slice: &[u8] = hash.as_ref();
        assert_eq!(slice.len(), 16);
        assert_eq!(slice[0], 0x42);

        let arr_ref: &[u8; 16] = hash.as_ref();
        assert_eq!(*arr_ref, [0x42; 16]);
    }

    #[test]
    fn test_destination_hash_borrow() {
        use core::borrow::Borrow;
        let hash = DestinationHash::new([0x42; 16]);
        let borrowed: &[u8; 16] = hash.borrow();
        assert_eq!(*borrowed, [0x42; 16]);
    }

    #[test]
    fn test_destination_hash_ord() {
        let hash1 = DestinationHash::new([0x01; 16]);
        let hash2 = DestinationHash::new([0x02; 16]);
        assert!(hash1 < hash2);
    }

    #[test]
    fn test_destination_hash_copy() {
        let hash1 = DestinationHash::new([0x42; 16]);
        let hash2 = hash1; // Copy
        assert_eq!(hash1, hash2); // Original still usable
    }

    #[test]
    fn test_destination_hash_is_different_type_from_link_id() {
        use crate::link::LinkId;
        let bytes = [0x42; 16];
        let _dest_hash = DestinationHash::new(bytes);
        let _link_id = LinkId::new(bytes);
        // These are different types - compiler prevents mixing them
        // (No PartialEq<LinkId> for DestinationHash or vice versa)
    }

    #[test]
    fn test_destination_hash_btreemap_key() {
        use alloc::collections::BTreeMap;
        let mut map = BTreeMap::new();
        let hash = DestinationHash::new([0x42; 16]);
        map.insert(hash, "test");
        assert_eq!(map.get(&hash), Some(&"test"));
        // Lookup via raw bytes via Borrow
        assert_eq!(map.get(&[0x42u8; 16]), Some(&"test"));
    }

    // Signed Ratchet Serialization Tests
    #[test]
    fn test_serialize_ratchets_signed_roundtrip() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["ratchetrt"],
        )
        .unwrap();

        dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();
        // Generate a few more ratchets by forcing rotation
        dest.set_ratchet_interval(1000); // 1 second
        assert!(dest.rotate_ratchet_if_needed(&mut OsRng, TEST_TIME_MS + 2000));
        assert!(dest.rotate_ratchet_if_needed(&mut OsRng, TEST_TIME_MS + 4000));
        let original_count = dest.ratchet_count();
        assert_eq!(original_count, 3);

        // Collect original public keys
        let original_keys: Vec<[u8; RATCHET_SIZE]> =
            dest.ratchets.iter().map(|r| r.public_key_bytes()).collect();

        let signed = dest.serialize_ratchets_signed().unwrap();

        // Load into a fresh destination with the same identity
        let id_bytes = dest.identity().unwrap().private_key_bytes().unwrap();
        let identity2 = Identity::from_private_key_bytes(&id_bytes).unwrap();
        let mut dest2 = Destination::new(
            Some(identity2),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["ratchetrt"],
        )
        .unwrap();

        let loaded = dest2.load_ratchets_signed(&signed).unwrap();
        assert_eq!(loaded, original_count);
        assert_eq!(dest2.ratchet_count(), original_count);
        assert!(dest2.ratchets_enabled());

        // Verify same public keys (derived from same private keys)
        let loaded_keys: Vec<[u8; RATCHET_SIZE]> = dest2
            .ratchets
            .iter()
            .map(|r| r.public_key_bytes())
            .collect();
        assert_eq!(original_keys, loaded_keys);

        // Loaded ratchets preserve keys (no timestamps in Python format)
        assert_eq!(dest2.ratchets.len(), original_keys.len());
    }

    /// Regression (Codeberg #267, second instance found by the audit the issue
    /// asked for): this module carries its own copy of the msgpack readers, and
    /// its `read_msgpack_bin` had the same wrapping `*pos + len > data.len()`
    /// guard on the bin32 arm.
    ///
    /// The store is local rather than wire-borne, but the outer map is parsed
    /// before the signature is verified, so a corrupt ratchet file panicked a
    /// 32-bit node at destination registration. Width-independent assertion:
    /// `Err`, not a panic, on both widths.
    #[test]
    fn ratchet_store_bin32_length_near_usize_max_is_rejected() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["bin32"],
        )
        .unwrap();

        // fixmap(2) + fixstr "signature" + bin32 claiming u32::MAX bytes.
        let mut store = alloc::vec![0x82u8, 0xa9];
        store.extend_from_slice(b"signature");
        store.push(0xc6);
        store.extend_from_slice(&u32::MAX.to_be_bytes());

        assert_eq!(
            dest.load_ratchets_signed(&store),
            Err(DestinationError::InvalidRatchetData)
        );
    }

    #[test]
    fn test_load_ratchets_signed_bad_signature() {
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["badsig"],
        )
        .unwrap();

        dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();
        let mut signed = dest.serialize_ratchets_signed().unwrap();

        // Flip a byte in the signature (which is at a known offset after the key)
        // The signature comes after: 0x82 + 0xa9 + "signature" + 0xc4 + 0x40 = 13 bytes
        signed[13] ^= 0xff;

        let id_bytes = dest.identity().unwrap().private_key_bytes().unwrap();
        let identity2 = Identity::from_private_key_bytes(&id_bytes).unwrap();
        let mut dest2 = Destination::new(
            Some(identity2),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["badsig"],
        )
        .unwrap();

        let result = dest2.load_ratchets_signed(&signed);
        assert_eq!(result, Err(DestinationError::InvalidSignature));
    }

    #[test]
    fn test_load_ratchets_signed_python_compat() {
        // Test vector generated by scripts/gen_ratchet_test_vector.py using
        // Python's umsgpack.packb() and Ed25519PrivateKey.sign(), the exact
        // same code path as Destination._persist_ratchets().

        // Identity private key bytes: [x25519_prv(32) | ed25519_prv(32)]
        #[rustfmt::skip]
        let identity_prv: [u8; 64] = [
            // X25519 private key
            96, 156, 116, 115, 83, 108, 6, 122, 130, 102, 19, 240, 158, 159, 111, 48,
            143, 152, 128, 101, 110, 18, 55, 203, 191, 42, 184, 208, 141, 75, 215, 76,
            // Ed25519 signing private key
            190, 174, 16, 135, 56, 204, 86, 145, 169, 175, 200, 150, 128, 74, 169, 110,
            166, 94, 141, 91, 116, 6, 94, 141, 196, 208, 179, 117, 237, 118, 121, 46,
        ];

        // Signed ratchet file bytes (123 bytes), output of Python's
        // umsgpack.packb({"signature": sign(packed), "ratchets": packed})
        // where packed = umsgpack.packb([bytes(0x42 * 32)])
        #[rustfmt::skip]
        let python_file_bytes: [u8; 123] = [
            0x82, 0xa9, 0x73, 0x69, 0x67, 0x6e, 0x61, 0x74, 0x75, 0x72, 0x65,
            0xc4, 0x40,
            0x48, 0xe4, 0x54, 0x9d, 0x60, 0xcf, 0xea, 0xfd, 0xc3, 0x87, 0xa7,
            0x27, 0xa9, 0x7c, 0xdd, 0x0e, 0x88, 0x18, 0xde, 0x28, 0xd1, 0x21,
            0xd8, 0x80, 0x6d, 0x29, 0x15, 0xfc, 0x48, 0xff, 0x7f, 0x93, 0xfe,
            0xd2, 0x1b, 0x29, 0x7e, 0x75, 0xbf, 0x86, 0x10, 0x72, 0x15, 0xc4,
            0xc6, 0x2c, 0xb2, 0xc5, 0xb7, 0x69, 0x44, 0x00, 0xaa, 0x29, 0xed,
            0x90, 0x9c, 0xb1, 0x84, 0x70, 0xc4, 0x9b, 0x62, 0x08,
            0xa8, 0x72, 0x61, 0x74, 0x63, 0x68, 0x65, 0x74, 0x73,
            0xc4, 0x23,
            0x91, 0xc4, 0x20,
            0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
            0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
            0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        ];

        // Expected ratchet public key (derived by Python from the 0x42-filled private key)
        #[rustfmt::skip]
        let expected_ratchet_pub: [u8; 32] = [
            19, 44, 68, 43, 224, 16, 251, 213, 126, 114, 96, 51, 40, 170, 118, 231,
            31, 204, 193, 80, 58, 174, 33, 147, 39, 209, 77, 156, 153, 147, 244, 114,
        ];

        // Reconstruct the identity from private key bytes
        let identity = Identity::from_private_key_bytes(&identity_prv).unwrap();

        // Load the Python-generated signed ratchet file
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["pycompat"],
        )
        .unwrap();

        let loaded = dest.load_ratchets_signed(&python_file_bytes).unwrap();
        assert_eq!(loaded, 1);

        // Verify the loaded ratchet derives the same public key Python computed
        assert_eq!(dest.ratchets[0].public_key_bytes(), expected_ratchet_pub);
    }

    #[test]
    fn test_serialize_ratchets_signed_no_identity() {
        // PLAIN destination has no identity, serialize should return None
        let dest =
            Destination::new(None, Direction::In, DestinationType::Plain, "plain", &[]).unwrap();

        assert!(dest.serialize_ratchets_signed().is_none());
    }

    // GROUP shared-key crypto tests
    fn group_dest() -> Destination {
        // GROUP IN destinations auto-scope like Python, but for crypto the
        // identity is irrelevant. We attach one so the address is well-formed.
        let identity = Identity::generate(&mut OsRng);
        Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Group,
            "levgroup",
            &["interop"],
        )
        .unwrap()
    }

    #[test]
    fn test_group_create_key_size() {
        let mut dest = group_dest();
        assert!(dest.group_key().is_none());
        dest.create_group_key(&mut OsRng).unwrap();
        // Matches Python create_keys -> Token.generate_key() default (AES-256): 64 bytes.
        assert_eq!(dest.group_key().unwrap().len(), TOKEN_KEY_SIZE);
        assert_eq!(TOKEN_KEY_SIZE, 64);
    }

    #[test]
    fn test_group_get_and_load_key() {
        let mut src = group_dest();
        src.create_group_key(&mut OsRng).unwrap();
        let key = *src.group_key().unwrap();

        let mut dst = group_dest();
        dst.load_group_key(&key).unwrap();
        assert_eq!(dst.group_key().unwrap(), &key);
    }

    #[test]
    fn test_group_roundtrip() {
        let mut sender = group_dest();
        sender.create_group_key(&mut OsRng).unwrap();
        let key = *sender.group_key().unwrap();

        let mut receiver = group_dest();
        receiver.load_group_key(&key).unwrap();

        let plaintext = b"group broadcast message";
        let ciphertext = sender.encrypt(plaintext, None, &mut OsRng).unwrap();
        // Ciphertext must not contain the plaintext.
        assert!(!ciphertext.windows(plaintext.len()).any(|w| w == plaintext));

        let decrypted = receiver.decrypt(&ciphertext).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_group_empty_plaintext_roundtrip() {
        let mut dest = group_dest();
        dest.create_group_key(&mut OsRng).unwrap();
        let ct = dest.encrypt(b"", None, &mut OsRng).unwrap();
        let pt = dest.decrypt(&ct).unwrap();
        assert!(pt.is_empty());
    }

    #[test]
    fn test_group_wrong_key_fails() {
        let mut sender = group_dest();
        sender.create_group_key(&mut OsRng).unwrap();
        let ciphertext = sender.encrypt(b"secret", None, &mut OsRng).unwrap();

        let mut wrong = group_dest();
        wrong.create_group_key(&mut OsRng).unwrap();
        let result = wrong.decrypt(&ciphertext);
        assert_eq!(result, Err(DestinationError::DecryptionFailed));
    }

    #[test]
    fn test_group_encrypt_without_key_fails() {
        let dest = group_dest();
        let result = dest.encrypt(b"x", None, &mut OsRng);
        assert_eq!(result, Err(DestinationError::NoGroupKey));
    }

    #[test]
    fn test_group_key_ops_reject_non_group() {
        let identity = Identity::generate(&mut OsRng);
        let mut single = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "app",
            &["s"],
        )
        .unwrap();
        assert_eq!(
            single.create_group_key(&mut OsRng),
            Err(DestinationError::NotGroupType)
        );
        assert_eq!(
            single.load_group_key(&[0u8; TOKEN_KEY_SIZE]),
            Err(DestinationError::NotGroupType)
        );
    }

    #[test]
    fn test_group_load_key_wrong_length() {
        let mut dest = group_dest();
        assert_eq!(
            dest.load_group_key(&[0u8; 32]),
            Err(DestinationError::InvalidGroupKeyLength)
        );
    }

    /// Known-answer test pinning the GROUP wire format to Python-RNS.
    ///
    /// Source: vendored Python-RNS `RNS.Destination` GROUP branch
    /// (`dest.load_private_key(key)`, `dest.encrypt`/`dest.decrypt`), which
    /// routes through `RNS.Cryptography.Token` in `AES_256_CBC` mode. Captured
    /// with `os.urandom` patched to emit the fixed IV below so the encrypt is
    /// deterministic.
    ///
    /// Fixed 64-byte key (0x00..0x3f), fixed 16-byte IV (0x00..0x0f),
    /// plaintext b"Reticulum GROUP KAT" (19 bytes). Reference token is 80 bytes:
    /// IV(16) || AES-256-CBC ciphertext(32) || HMAC-SHA256(32).
    ///
    /// Asserts both directions:
    ///   - our decrypt(reference token) == plaintext, and
    ///   - our encrypt(key, iv, plaintext) == reference token, byte-for-byte.
    #[test]
    fn kat_group_python_rns_vector() {
        let key: [u8; 64] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29,
            0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
        ];
        let iv: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        // b"Reticulum GROUP KAT"
        let plaintext: [u8; 19] = [
            0x52, 0x65, 0x74, 0x69, 0x63, 0x75, 0x6c, 0x75, 0x6d, 0x20, 0x47, 0x52, 0x4f, 0x55,
            0x50, 0x20, 0x4b, 0x41, 0x54,
        ];
        // Reference token captured from Python-RNS GROUP Destination.encrypt.
        let reference_token: [u8; 80] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x02, 0xd6, 0x54, 0x59, 0x85, 0x2c, 0xf1, 0xef, 0xe0, 0xed, 0xcf, 0xd8,
            0x31, 0xf6, 0x85, 0xa6, 0x10, 0x0d, 0x7a, 0x4c, 0x4c, 0x5c, 0x8c, 0x87, 0x77, 0x6a,
            0xee, 0xd6, 0x7c, 0x3b, 0x2e, 0x9a, 0x7d, 0xbe, 0xa6, 0x7c, 0x10, 0x09, 0xb9, 0xb6,
            0xbf, 0x76, 0x36, 0x1d, 0x9b, 0x4f, 0x6f, 0x3e, 0xc6, 0xf2, 0xcf, 0x21, 0x41, 0xa9,
            0xe9, 0x68, 0xa6, 0x21, 0xaa, 0x8f, 0xa1, 0xdc, 0xe6, 0xfa,
        ];

        // Our decrypt of the fixed Python-RNS GROUP token recovers the plaintext.
        let mut dest = group_dest();
        dest.load_group_key(&key).unwrap();
        let decrypted = dest.decrypt(&reference_token).unwrap();
        assert_eq!(
            &decrypted[..],
            &plaintext,
            "our GROUP decrypt of the Python-RNS token must recover the plaintext"
        );

        // Our encrypt with the same key + IV reproduces the exact token bytes.
        // (group_encrypt draws the IV from rng, so exercise the token layer with
        // the pinned IV directly, matching the same code path.)
        let padded_len = ((plaintext.len() / 16) + 1) * 16;
        let mut tok = vec![0u8; 16 + padded_len + 32];
        let n = crate::crypto::encrypt_token(&key, &iv, &plaintext, &mut tok).unwrap();
        tok.truncate(n);
        assert_eq!(
            &tok[..],
            &reference_token,
            "our GROUP token bytes must match the Python-RNS reference token"
        );
    }

    fn announceable_dest(with_ratchet: bool) -> Destination {
        let mut dest = Destination::new(
            Some(Identity::generate(&mut OsRng)),
            Direction::In,
            DestinationType::Single,
            "testapp",
            &["budget"],
        )
        .unwrap();
        if with_ratchet {
            dest.enable_ratchets(&mut OsRng, TEST_TIME_MS).unwrap();
        }
        dest
    }

    /// The exported budget must be exactly the largest `app_data` that packs:
    /// `budget` bytes fit the MTU, and the resulting packet's true wire length
    /// never exceeds it.
    #[test]
    fn test_announce_at_app_data_budget_packs_within_mtu() {
        for with_ratchet in [false, true] {
            let mut dest = announceable_dest(with_ratchet);
            let budget = crate::announce::announce_app_data_budget(with_ratchet);
            let app_data = alloc::vec![0xAAu8; budget];

            let packet = dest
                .announce(
                    Some(&app_data),
                    &mut OsRng,
                    TEST_TIME_MS,
                    TEST_TIME_MS / 1000,
                )
                .unwrap_or_else(|e| {
                    panic!("announce at budget must succeed (ratchet={with_ratchet}): {e}")
                });

            let mut buf = [0u8; crate::constants::MTU];
            let written = packet.pack(&mut buf).unwrap_or_else(|e| {
                panic!("announce at budget must pack (ratchet={with_ratchet}): {e}")
            });

            assert!(
                written <= crate::constants::MTU,
                "announce at budget overran the MTU (ratchet={with_ratchet}): {written} B"
            );
        }
    }

    /// One byte over budget is refused at compose time — and the error carries
    /// the numbers, rather than an unactionable "too large for MTU".
    #[test]
    fn test_announce_over_budget_reports_byte_counts() {
        for with_ratchet in [false, true] {
            let mut dest = announceable_dest(with_ratchet);
            let budget = crate::announce::announce_app_data_budget(with_ratchet);
            let oversized = alloc::vec![0xAAu8; budget + 1];

            let err = dest
                .announce(
                    Some(&oversized),
                    &mut OsRng,
                    TEST_TIME_MS,
                    TEST_TIME_MS / 1000,
                )
                .expect_err("announce one byte over budget must be refused");

            match err {
                AnnounceError::PacketTooLarge {
                    packed,
                    mtu,
                    app_data,
                    budget: reported_budget,
                } => {
                    assert_eq!(app_data, budget + 1);
                    assert_eq!(reported_budget, budget);
                    assert_eq!(mtu, crate::constants::MTU);
                    // `packed` is the true wire length the announce would have had.
                    assert_eq!(
                        packed,
                        crate::constants::HEADER_MINSIZE
                            + crate::announce::announce_payload_fixed_len(with_ratchet)
                            + app_data
                    );
                }
                other => panic!("expected PacketTooLarge, got {other:?}"),
            }
        }
    }
}
