//! Resource protocol for reliable transfer of large data over a [`Link`](crate::link::Link).
//!
//! While [`Channel`](crate::link::channel::Channel) handles small messages (up to ~400 bytes
//! per envelope), resources transfer kilobytes to megabytes by segmenting data
//! into parts and using a sliding window for throughput.
//!
//! # Transfer Flow
//!
//! 1. Sender creates a resource from raw data and a link
//! 2. Sender advertises the resource to the remote peer
//! 3. Receiver accepts (or rejects) the advertisement
//! 4. Parts are sent using a sliding window with automatic retransmission
//! 5. Receiver verifies the reassembled data against a hash
//! 6. Receiver sends a completion proof back to the sender
//!
//! # State Machine
//!
//! ```text
//! None → Queued → Advertised → Transferring → AwaitingProof → Complete
//!                                    ↓
//!                              Failed / Corrupt
//! ```
//!
//! # Current Status
//!
//! Phase 1: Core types, advertisement serialization, hashmap computation.
//! The full transfer state machine is planned for Phase 2.

pub(crate) mod compression;
pub(crate) mod hashmap;
pub(crate) mod incoming;
pub(crate) mod msgpack;
pub(crate) mod outgoing;
pub mod window;

pub use outgoing::{
    prepare_resource_send, PreparedResourceSend, ResourceCryptParams, ResourceSendParams,
};
pub use window::{RateSample, WindowPolicy};

use crate::constants::{
    HEADER_MAXSIZE, IFAC_MIN_SIZE, RESOURCE_HASHMAP_LEN, RESOURCE_WINDOW_MAX_FAST,
};
use alloc::vec::Vec;

// Constants
/// Overhead subtracted from negotiated MTU to get the Resource SDU.
/// HEADER_MAXSIZE(35) + IFAC_MIN_SIZE(1) = 36.
pub const RESOURCE_SDU_OVERHEAD: usize = HEADER_MAXSIZE + IFAC_MIN_SIZE;

/// Fixed overhead in a ResourceAdvertisement msgpack payload (bytes).
/// This is the size of the advertisement excluding the variable-length hashmap.
pub const RESOURCE_ADV_OVERHEAD: usize = 134;

/// Size of the random hash that makes each resource transfer unique.
pub const RESOURCE_RANDOM_HASH_SIZE: usize = 4;

/// Maximum efficient resource size: fits in 3-byte length encoding (0xFFFFFF).
pub const RESOURCE_MAX_EFFICIENT_SIZE: usize = 1_048_575;

/// Maximum encrypted transfer size (advertisement `t` field) accepted from a
/// peer, in bytes. Checked before any allocation, in
/// `IncomingResource::from_advertisement`.
///
/// This is a **per-advertisement** ceiling, not a per-transfer one. Derived
/// from the reference rather than picked for roundness:
///
/// - `Resource.__init__` segments *before* compression at `MAX_EFFICIENT_SIZE`
///   (Resource.py:116, 1 MiB - 1). A bytes payload above it is spilled to a
///   temporary file (:274-279) and read back one `MAX_EFFICIENT_SIZE` slice
///   per segment (:296-313), so a single segment's plaintext never exceeds
///   that ceiling. A larger transfer arrives as more segments, each with its
///   own advertisement.
/// - The sender prepends a 4-byte random hash (:405-412) and token-encrypts
///   the segment (:427), adding `TOKEN_OVERHEAD` (IV + HMAC) plus at most one
///   full AES padding block (`Link::encrypted_size`).
///
/// A stock Python sender therefore cannot emit a `t` above this sum
/// (1_048_643); anything larger is malformed or hostile. Before this had a
/// real value, `usize::MAX` meant the size check never fired and a peer chose
/// our allocation sizes (Codeberg #263).
///
/// Callers who talk to a peer that segments more coarsely can raise it — and
/// embedded targets lower it — via
/// `NodeCoreBuilder::max_incoming_resource_size()`.
pub const RESOURCE_MAX_INCOMING_SIZE: usize = RESOURCE_MAX_EFFICIENT_SIZE
    + RESOURCE_RANDOM_HASH_SIZE
    + crate::constants::TOKEN_OVERHEAD
    + crate::constants::AES_BLOCK_SIZE;

/// Maximum metadata size (16 MiB - 1).
pub const RESOURCE_METADATA_MAX_SIZE: usize = 16 * 1024 * 1024 - 1;

/// Maximum number of retries for sending a resource part.
pub const RESOURCE_MAX_RETRIES: usize = 16;

/// Maximum number of retries for advertisement.
pub const RESOURCE_MAX_ADV_RETRIES: usize = 4;

/// Maximum window size for very slow links (LoRa-class).
pub const RESOURCE_WINDOW_MAX_VERY_SLOW: usize = 4;

/// Window flexibility for adaptive sizing.
pub const RESOURCE_WINDOW_FLEXIBILITY: usize = 4;

/// Fast rate threshold (bytes/sec). Above this, window_max = WINDOW_MAX_FAST.
pub const FAST_RATE_THRESHOLD: u64 = 50_000;

/// Slow rate threshold (bytes/sec). Below this, window_max = WINDOW_MAX_SLOW.
pub const SLOW_RATE_THRESHOLD: u64 = 15_000;

/// Very slow rate threshold (bytes/sec). Below this, window_max = WINDOW_MAX_VERY_SLOW.
pub const VERY_SLOW_RATE_THRESHOLD: u64 = 1_000;

/// Sender grace time before declaring failure (Python Resource.py:131 SENDER_GRACE_TIME = 10.0).
pub const SENDER_GRACE_TIME_MS: u64 = 10_000;

/// Additional delay per retry used, for progressive backoff (Python Resource.py:134 PER_RETRY_DELAY = 0.5).
pub const PER_RETRY_DELAY_MS: u64 = 500;

/// Small grace period added to each timeout check (Python Resource.py:133 RETRY_GRACE_TIME = 0.25).
pub const RETRY_GRACE_TIME_MS: u64 = 250;

/// Initial part timeout multiplier before first data/response (Python Resource.py:126 PART_TIMEOUT_FACTOR = 4).
pub const PART_TIMEOUT_FACTOR_INITIAL: u64 = 4;

/// Reduced part timeout multiplier after first data received (Python Resource.py:127 PART_TIMEOUT_FACTOR_AFTER_RTT = 2).
pub const PART_TIMEOUT_FACTOR_AFTER_RTT: u64 = 2;

/// Timeout multiplier for awaiting proof (Python Resource.py:128 PROOF_TIMEOUT_FACTOR = 3).
pub const PROOF_TIMEOUT_FACTOR: u64 = 3;

/// Processing grace for advertisement retransmit (Python Resource.py:132 PROCESSING_GRACE = 1.0).
pub const PROCESSING_GRACE_MS: u64 = 1_000;

/// Hashmap not exhausted flag in REQ packets.
pub const HASHMAP_IS_NOT_EXHAUSTED: u8 = 0x00;

/// Hashmap exhausted flag in REQ packets.
/// Must match Python's `Resource.HASHMAP_IS_EXHAUSTED = 0xFF`.
pub const HASHMAP_IS_EXHAUSTED: u8 = 0xFF;

// Derived constants
/// Standard Link MDU computed from the default MTU (500).
///
/// `floor((MTU - IFAC_MIN_SIZE - HEADER_MINSIZE - TOKEN_OVERHEAD) / AES_BLOCK_SIZE)
///  * AES_BLOCK_SIZE - 1 = floor(432/16)*16 - 1 = 431`
///
/// Python computes this as `Link.MDU`, a class constant that never changes,
/// even when Link MTU Discovery negotiates a higher MTU.
/// Resource hashmap segmentation MUST use this standard MDU, not the
/// negotiated link MDU, because Python's `ResourceAdvertisement.HASHMAP_MAX_LEN`
/// is a class constant derived from the default `Link.MDU`.
pub const STANDARD_LINK_MDU: usize = {
    use crate::constants::{AES_BLOCK_SIZE, HEADER_MINSIZE, IFAC_MIN_SIZE, MTU, TOKEN_OVERHEAD};
    let usable = MTU - IFAC_MIN_SIZE - HEADER_MINSIZE - TOKEN_OVERHEAD;
    (usable / AES_BLOCK_SIZE) * AES_BLOCK_SIZE - 1
};

/// Maximum number of hashmap entries per segment.
///
/// This is a protocol constant. Python's `ResourceAdvertisement.HASHMAP_MAX_LEN`
/// is a class constant `floor((Link.MDU - 134) / 4) = 74`.
/// Both sender and receiver MUST use the same value for segment boundary
/// alignment. Using the negotiated link_mdu instead of STANDARD_LINK_MDU
/// causes segment boundary misalignment and breaks HMU processing.
pub const HASHMAP_MAX_LEN: usize =
    (STANDARD_LINK_MDU - RESOURCE_ADV_OVERHEAD) / RESOURCE_HASHMAP_LEN;

/// Size of the collision guard: number of sequence numbers reserved to
/// prevent hashmap collisions.
pub const COLLISION_GUARD_SIZE: usize = 2 * RESOURCE_WINDOW_MAX_FAST + HASHMAP_MAX_LEN;

/// Compute the Resource SDU (Service Data Unit) for a given negotiated MTU.
///
/// This is the maximum payload size for RESOURCE context data packets.
/// Resource packets skip per-packet encryption (the entire data blob is
/// encrypted in bulk before segmentation).
///
/// Standard MTU 500 → 500 - 35 - 1 = 464.
pub fn resource_sdu(negotiated_mtu: u32) -> usize {
    (negotiated_mtu as usize).saturating_sub(RESOURCE_SDU_OVERHEAD)
}

// Types
/// Status of a resource transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResourceStatus {
    None = 0x00,
    Queued = 0x01,
    Advertised = 0x02,
    Transferring = 0x03,
    AwaitingProof = 0x04,
    Assembling = 0x05,
    Complete = 0x06,
    Failed = 0x07,
    Corrupt = 0x08,
}

/// Strategy for accepting incoming resource advertisements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceStrategy {
    /// Reject all incoming resources.
    AcceptNone,
    /// Accept all incoming resources.
    AcceptAll,
    /// Ask the application via callback.
    AcceptApp,
}

/// Bit flags carried in a resource advertisement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceFlags {
    pub encrypted: bool,
    pub compressed: bool,
    pub split: bool,
    pub is_request: bool,
    pub is_response: bool,
    pub has_metadata: bool,
}

impl ResourceFlags {
    /// Encode flags into a single byte.
    pub fn to_u8(self) -> u8 {
        (self.encrypted as u8)
            | ((self.compressed as u8) << 1)
            | ((self.split as u8) << 2)
            | ((self.is_request as u8) << 3)
            | ((self.is_response as u8) << 4)
            | ((self.has_metadata as u8) << 5)
    }

    /// Decode flags from a single byte.
    pub fn from_u8(v: u8) -> Self {
        Self {
            encrypted: v & 0x01 != 0,
            compressed: (v >> 1) & 0x01 != 0,
            split: (v >> 2) & 0x01 != 0,
            is_request: (v >> 3) & 0x01 != 0,
            is_response: (v >> 4) & 0x01 != 0,
            has_metadata: (v >> 5) & 0x01 != 0,
        }
    }
}

/// Error type for resource operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceError {
    /// Advertisement data is malformed or missing required fields.
    InvalidAdvertisement,
    /// Hashmap data is malformed.
    InvalidHashmap,
    /// Data hash does not match expected value.
    HashMismatch,
    /// Transfer timed out.
    Timeout,
    /// Transfer was cancelled.
    Cancelled,
    /// Maximum retry count exceeded.
    MaxRetriesExceeded,
    /// Link is not in Active state.
    LinkNotActive,
    /// Link was torn down while the transfer was in flight.
    LinkClosed,
    /// Compression feature not available but compressed resource received.
    CompressionUnsupported,
    /// Compression failed.
    CompressionFailed,
    /// Decompression failed.
    DecompressionFailed,
    /// Encryption or decryption failed.
    CryptoError,
    /// A resource transfer is already in progress on this link.
    TransferInProgress,
    /// No pending resource advertisement to accept/reject.
    NoPendingResource,
    /// Invalid proof data.
    InvalidProof,
    /// Invalid request data.
    InvalidRequest,
    /// Advertised resource exceeds the configured size limit.
    ResourceTooLarge,
    /// The link's state changed between an off-lock resource build and its
    /// commit (e.g. a #66 establishment-retry re-key replaced the token key),
    /// so the prepared ciphertext is no longer decryptable by the peer.
    /// Retryable: re-run params -> prepare -> commit against the fresh state.
    LinkStateChanged,
}

impl core::fmt::Display for ResourceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidAdvertisement => write!(f, "invalid resource advertisement"),
            Self::InvalidHashmap => write!(f, "invalid resource hashmap"),
            Self::HashMismatch => write!(f, "resource hash mismatch"),
            Self::Timeout => write!(f, "resource transfer timed out"),
            Self::Cancelled => write!(f, "resource transfer cancelled"),
            Self::MaxRetriesExceeded => write!(f, "resource max retries exceeded"),
            Self::LinkNotActive => write!(f, "link not active for resource transfer"),
            Self::LinkClosed => write!(f, "link closed during resource transfer"),
            Self::CompressionUnsupported => write!(f, "compression not supported"),
            Self::CompressionFailed => write!(f, "compression failed"),
            Self::DecompressionFailed => write!(f, "decompression failed"),
            Self::CryptoError => write!(f, "resource encryption/decryption failed"),
            Self::TransferInProgress => write!(f, "resource transfer already in progress"),
            Self::NoPendingResource => write!(f, "no pending resource advertisement"),
            Self::InvalidProof => write!(f, "invalid resource proof"),
            Self::InvalidRequest => write!(f, "invalid resource request"),
            Self::ResourceTooLarge => write!(f, "resource exceeds size limit"),
            Self::LinkStateChanged => {
                write!(f, "link state changed during off-lock resource build")
            }
        }
    }
}

// ResourceAdvertisement
/// A resource advertisement message.
///
/// Sent by the sender to propose a resource transfer. The receiver may accept
/// or reject it. Serialized as a msgpack fixmap(11) with single-char string
/// keys matching the Python implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceAdvertisement {
    /// "t", encrypted data size in bytes.
    pub transfer_size: u64,
    /// "d", uncompressed data size in bytes.
    pub data_size: u64,
    /// "n", number of data parts.
    pub num_parts: u32,
    /// "h". SHA-256 hash of the resource data.
    pub resource_hash: [u8; 32],
    /// "r", random hash unique to this transfer.
    pub random_hash: [u8; RESOURCE_RANDOM_HASH_SIZE],
    /// "o", original hash (before encryption/compression).
    pub original_hash: [u8; 32],
    /// "i", segment index (1-based).
    pub segment_index: u32,
    /// "l", total number of segments.
    pub total_segments: u32,
    /// "q", request ID (None encodes as msgpack nil).
    pub request_id: Option<Vec<u8>>,
    /// "f", resource flags.
    pub flags: ResourceFlags,
    /// "m", first segment of the hashmap (4 bytes per entry).
    pub hashmap_data: Vec<u8>,
}

impl ResourceAdvertisement {
    /// Serialize to msgpack.
    ///
    /// Produces a fixmap(11) with single-char string keys in Python-compatible
    /// order: t, d, n, h, r, o, i, l, q, f, m.
    pub fn pack(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(RESOURCE_ADV_OVERHEAD + self.hashmap_data.len());

        msgpack::write_fixmap_header(&mut buf, 11);

        msgpack::write_fixstr(&mut buf, "t");
        msgpack::write_uint(&mut buf, self.transfer_size);

        msgpack::write_fixstr(&mut buf, "d");
        msgpack::write_uint(&mut buf, self.data_size);

        msgpack::write_fixstr(&mut buf, "n");
        msgpack::write_uint(&mut buf, self.num_parts as u64);

        msgpack::write_fixstr(&mut buf, "h");
        msgpack::write_bin(&mut buf, &self.resource_hash);

        msgpack::write_fixstr(&mut buf, "r");
        msgpack::write_bin(&mut buf, &self.random_hash);

        msgpack::write_fixstr(&mut buf, "o");
        msgpack::write_bin(&mut buf, &self.original_hash);

        msgpack::write_fixstr(&mut buf, "i");
        msgpack::write_uint(&mut buf, self.segment_index as u64);

        msgpack::write_fixstr(&mut buf, "l");
        msgpack::write_uint(&mut buf, self.total_segments as u64);

        msgpack::write_fixstr(&mut buf, "q");
        match &self.request_id {
            Some(id) => msgpack::write_bin(&mut buf, id),
            None => msgpack::write_nil(&mut buf),
        }

        msgpack::write_fixstr(&mut buf, "f");
        msgpack::write_uint(&mut buf, self.flags.to_u8() as u64);

        msgpack::write_fixstr(&mut buf, "m");
        msgpack::write_bin(&mut buf, &self.hashmap_data);

        buf
    }

    /// Deserialize from msgpack.
    ///
    /// Parses a fixmap by iterating key-value pairs and matching on key strings.
    /// Key order is not assumed (Python dicts are unordered in msgpack).
    pub fn unpack(data: &[u8]) -> Result<Self, ResourceError> {
        let mut pos = 0;

        let map_len =
            msgpack::read_fixmap_len(data, &mut pos).ok_or(ResourceError::InvalidAdvertisement)?;

        let mut transfer_size: Option<u64> = None;
        let mut data_size: Option<u64> = None;
        let mut num_parts: Option<u64> = None;
        let mut resource_hash: Option<[u8; 32]> = None;
        let mut random_hash: Option<[u8; RESOURCE_RANDOM_HASH_SIZE]> = None;
        let mut original_hash: Option<[u8; 32]> = None;
        let mut segment_index: Option<u64> = None;
        let mut total_segments: Option<u64> = None;
        let mut request_id: Option<Option<Vec<u8>>> = None;
        let mut flags: Option<u8> = None;
        let mut hashmap_data: Option<Vec<u8>> = None;

        for _ in 0..map_len {
            let key = msgpack::read_msgpack_str(data, &mut pos)
                .ok_or(ResourceError::InvalidAdvertisement)?;

            match key {
                b"t" => {
                    transfer_size = Some(
                        msgpack::read_msgpack_uint(data, &mut pos)
                            .ok_or(ResourceError::InvalidAdvertisement)?,
                    );
                }
                b"d" => {
                    data_size = Some(
                        msgpack::read_msgpack_uint(data, &mut pos)
                            .ok_or(ResourceError::InvalidAdvertisement)?,
                    );
                }
                b"n" => {
                    num_parts = Some(
                        msgpack::read_msgpack_uint(data, &mut pos)
                            .ok_or(ResourceError::InvalidAdvertisement)?,
                    );
                }
                b"h" => {
                    let bin = msgpack::read_msgpack_bin(data, &mut pos)
                        .ok_or(ResourceError::InvalidAdvertisement)?;
                    if bin.len() != 32 {
                        return Err(ResourceError::InvalidAdvertisement);
                    }
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(bin);
                    resource_hash = Some(arr);
                }
                b"r" => {
                    let bin = msgpack::read_msgpack_bin(data, &mut pos)
                        .ok_or(ResourceError::InvalidAdvertisement)?;
                    if bin.len() != RESOURCE_RANDOM_HASH_SIZE {
                        return Err(ResourceError::InvalidAdvertisement);
                    }
                    let mut arr = [0u8; RESOURCE_RANDOM_HASH_SIZE];
                    arr.copy_from_slice(bin);
                    random_hash = Some(arr);
                }
                b"o" => {
                    let bin = msgpack::read_msgpack_bin(data, &mut pos)
                        .ok_or(ResourceError::InvalidAdvertisement)?;
                    if bin.len() != 32 {
                        return Err(ResourceError::InvalidAdvertisement);
                    }
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(bin);
                    original_hash = Some(arr);
                }
                b"i" => {
                    segment_index = Some(
                        msgpack::read_msgpack_uint(data, &mut pos)
                            .ok_or(ResourceError::InvalidAdvertisement)?,
                    );
                }
                b"l" => {
                    total_segments = Some(
                        msgpack::read_msgpack_uint(data, &mut pos)
                            .ok_or(ResourceError::InvalidAdvertisement)?,
                    );
                }
                b"q" => {
                    let val = msgpack::read_msgpack_bin_or_nil(data, &mut pos)
                        .ok_or(ResourceError::InvalidAdvertisement)?;
                    request_id = Some(val.map(|b| b.to_vec()));
                }
                b"f" => {
                    let val = msgpack::read_msgpack_uint(data, &mut pos)
                        .ok_or(ResourceError::InvalidAdvertisement)?;
                    flags = Some(val as u8);
                }
                b"m" => {
                    let bin = msgpack::read_msgpack_bin(data, &mut pos)
                        .ok_or(ResourceError::InvalidAdvertisement)?;
                    hashmap_data = Some(bin.to_vec());
                }
                _ => {
                    // Unknown key, skip value for forward compatibility
                    msgpack::skip_msgpack_value(data, &mut pos)
                        .ok_or(ResourceError::InvalidAdvertisement)?;
                }
            }
        }

        // All 11 fields are required
        Ok(Self {
            transfer_size: transfer_size.ok_or(ResourceError::InvalidAdvertisement)?,
            data_size: data_size.ok_or(ResourceError::InvalidAdvertisement)?,
            // Reject rather than silently truncate a u64 that does not fit u32
            // (a malformed/hostile ADV could otherwise smuggle e.g. 2^32+5 as 5
            // past the consistency check).
            num_parts: u32::try_from(num_parts.ok_or(ResourceError::InvalidAdvertisement)?)
                .map_err(|_| ResourceError::InvalidAdvertisement)?,
            resource_hash: resource_hash.ok_or(ResourceError::InvalidAdvertisement)?,
            random_hash: random_hash.ok_or(ResourceError::InvalidAdvertisement)?,
            original_hash: original_hash.ok_or(ResourceError::InvalidAdvertisement)?,
            segment_index: u32::try_from(segment_index.ok_or(ResourceError::InvalidAdvertisement)?)
                .map_err(|_| ResourceError::InvalidAdvertisement)?,
            total_segments: u32::try_from(
                total_segments.ok_or(ResourceError::InvalidAdvertisement)?,
            )
            .map_err(|_| ResourceError::InvalidAdvertisement)?,
            request_id: request_id.ok_or(ResourceError::InvalidAdvertisement)?,
            flags: ResourceFlags::from_u8(flags.ok_or(ResourceError::InvalidAdvertisement)?),
            hashmap_data: hashmap_data.ok_or(ResourceError::InvalidAdvertisement)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    // ResourceFlags
    #[test]
    fn test_flags_individual_bits() {
        assert_eq!(
            ResourceFlags {
                encrypted: true,
                ..Default::default()
            }
            .to_u8(),
            0x01
        );
        assert_eq!(
            ResourceFlags {
                compressed: true,
                ..Default::default()
            }
            .to_u8(),
            0x02
        );
        assert_eq!(
            ResourceFlags {
                split: true,
                ..Default::default()
            }
            .to_u8(),
            0x04
        );
        assert_eq!(
            ResourceFlags {
                is_request: true,
                ..Default::default()
            }
            .to_u8(),
            0x08
        );
        assert_eq!(
            ResourceFlags {
                is_response: true,
                ..Default::default()
            }
            .to_u8(),
            0x10
        );
        assert_eq!(
            ResourceFlags {
                has_metadata: true,
                ..Default::default()
            }
            .to_u8(),
            0x20
        );
    }

    #[test]
    fn test_flags_combined() {
        let flags = ResourceFlags {
            encrypted: true,
            compressed: true,
            split: false,
            is_request: true,
            is_response: false,
            has_metadata: true,
        };
        let byte = flags.to_u8();
        assert_eq!(byte, 0x01 | 0x02 | 0x08 | 0x20);
        assert_eq!(byte, 0x2B);
    }

    #[test]
    fn test_flags_roundtrip() {
        let test_values: &[u8] = &[0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x3F, 0x2B];
        for &val in test_values {
            let flags = ResourceFlags::from_u8(val);
            assert_eq!(flags.to_u8(), val, "roundtrip failed for 0x{val:02x}");
        }
    }

    #[test]
    fn test_flags_from_u8() {
        let flags = ResourceFlags::from_u8(0x3F);
        assert!(flags.encrypted);
        assert!(flags.compressed);
        assert!(flags.split);
        assert!(flags.is_request);
        assert!(flags.is_response);
        assert!(flags.has_metadata);

        let flags = ResourceFlags::from_u8(0x00);
        assert!(!flags.encrypted);
        assert!(!flags.compressed);
    }

    // ResourceAdvertisement
    fn make_test_adv(request_id: Option<Vec<u8>>) -> ResourceAdvertisement {
        ResourceAdvertisement {
            transfer_size: 464,
            data_size: 100,
            num_parts: 1,
            resource_hash: [0xAA; 32],
            random_hash: [0xBB; RESOURCE_RANDOM_HASH_SIZE],
            original_hash: [0xCC; 32],
            segment_index: 1,
            total_segments: 1,
            request_id,
            flags: ResourceFlags {
                encrypted: true,
                ..Default::default()
            },
            hashmap_data: vec![0x11, 0x22, 0x33, 0x44],
        }
    }

    #[test]
    fn test_advertisement_pack_unpack_roundtrip() {
        let adv = make_test_adv(None);
        let packed = adv.pack();
        let unpacked = ResourceAdvertisement::unpack(&packed).unwrap();
        assert_eq!(adv, unpacked);
    }

    #[test]
    fn test_advertisement_pack_unpack_with_request_id() {
        let adv = make_test_adv(Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        let packed = adv.pack();
        let unpacked = ResourceAdvertisement::unpack(&packed).unwrap();
        assert_eq!(adv, unpacked);
    }

    #[test]
    fn test_advertisement_pack_structure() {
        // Verify the packed format starts with fixmap(11) = 0x8b
        let adv = make_test_adv(None);
        let packed = adv.pack();
        assert_eq!(packed[0], 0x8b); // fixmap(11)

        // Second byte should be fixstr(1) for "t"
        assert_eq!(packed[1], 0xa1); // fixstr(1)
        assert_eq!(packed[2], b't');
    }

    #[test]
    fn test_advertisement_unpack_missing_field() {
        // Empty map
        let data = [0x80]; // fixmap(0)
        assert_eq!(
            ResourceAdvertisement::unpack(&data),
            Err(ResourceError::InvalidAdvertisement)
        );
    }

    /// Regression (Codeberg #23, found by fuzzing): a resource advertisement
    /// whose unknown-key value is a deeply nested msgpack container must be
    /// rejected as malformed, not recurse until the stack overflows. This is
    /// the exact wire-level reproducer of the DoS the fuzzer found — an
    /// attacker-supplied advertisement arriving over an established link.
    #[test]
    fn test_advertisement_unpack_rejects_deeply_nested_value() {
        // fixmap(1): unknown key "z" -> deeply nested fixarray chain.
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x81u8, 0xa1, b'z']);
        buf.resize(buf.len() + 100_000, 0x91u8);
        buf.push(0x90); // innermost: empty array
        assert_eq!(
            ResourceAdvertisement::unpack(&buf),
            Err(ResourceError::InvalidAdvertisement)
        );
    }

    #[test]
    fn test_advertisement_unpack_wrong_hash_size() {
        // Build a minimal map with wrong-sized "h" field
        let mut buf = Vec::new();
        msgpack::write_fixmap_header(&mut buf, 1);
        msgpack::write_fixstr(&mut buf, "h");
        msgpack::write_bin(&mut buf, &[0u8; 16]); // wrong: 16 instead of 32
        assert_eq!(
            ResourceAdvertisement::unpack(&buf),
            Err(ResourceError::InvalidAdvertisement)
        );
    }

    #[test]
    fn test_advertisement_unpack_rejects_num_parts_over_u32() {
        // A well-formed ADV whose num_parts is encoded as a u64 beyond u32::MAX
        // must be REJECTED, not silently truncated (2^32+5 would otherwise pass
        // the consistency check as 5).
        let mut buf = Vec::new();
        msgpack::write_fixmap_header(&mut buf, 11);
        msgpack::write_fixstr(&mut buf, "t");
        msgpack::write_uint(&mut buf, 1000);
        msgpack::write_fixstr(&mut buf, "d");
        msgpack::write_uint(&mut buf, 1000);
        msgpack::write_fixstr(&mut buf, "n");
        msgpack::write_uint(&mut buf, (u32::MAX as u64) + 5);
        msgpack::write_fixstr(&mut buf, "h");
        msgpack::write_bin(&mut buf, &[0u8; 32]);
        msgpack::write_fixstr(&mut buf, "r");
        msgpack::write_bin(&mut buf, &[0u8; RESOURCE_RANDOM_HASH_SIZE]);
        msgpack::write_fixstr(&mut buf, "o");
        msgpack::write_bin(&mut buf, &[0u8; 32]);
        msgpack::write_fixstr(&mut buf, "i");
        msgpack::write_uint(&mut buf, 0);
        msgpack::write_fixstr(&mut buf, "l");
        msgpack::write_uint(&mut buf, 1);
        msgpack::write_fixstr(&mut buf, "q");
        msgpack::write_nil(&mut buf);
        msgpack::write_fixstr(&mut buf, "f");
        msgpack::write_uint(&mut buf, 0);
        msgpack::write_fixstr(&mut buf, "m");
        msgpack::write_bin(&mut buf, &[]);
        assert!(
            matches!(
                ResourceAdvertisement::unpack(&buf),
                Err(ResourceError::InvalidAdvertisement)
            ),
            "num_parts > u32::MAX must be rejected, not truncated"
        );
    }

    #[test]
    fn test_advertisement_large_values() {
        let adv = ResourceAdvertisement {
            transfer_size: 1_000_000_000, // 1 GB
            data_size: 900_000_000,
            num_parts: 2_000_000,
            resource_hash: [0xFF; 32],
            random_hash: [0xFF; RESOURCE_RANDOM_HASH_SIZE],
            original_hash: [0xFF; 32],
            segment_index: 5,
            total_segments: 10,
            request_id: Some(vec![0u8; 64]),
            flags: ResourceFlags {
                encrypted: true,
                compressed: true,
                split: true,
                is_request: false,
                is_response: false,
                has_metadata: true,
            },
            hashmap_data: vec![0u8; 296], // 74 entries × 4 bytes
        };

        let packed = adv.pack();
        let unpacked = ResourceAdvertisement::unpack(&packed).unwrap();
        assert_eq!(adv, unpacked);
    }

    #[test]
    fn test_advertisement_python_interop_vector() {
        // Manually construct expected msgpack bytes for a minimal advertisement
        // and verify pack() produces the exact same output.
        let adv = ResourceAdvertisement {
            transfer_size: 100, // fits in fixint
            data_size: 50,
            num_parts: 1,
            resource_hash: [0u8; 32],
            random_hash: [0u8; RESOURCE_RANDOM_HASH_SIZE],
            original_hash: [0u8; 32],
            segment_index: 1,
            total_segments: 1,
            request_id: None,
            flags: ResourceFlags::default(), // all false = 0
            hashmap_data: vec![0xAA, 0xBB, 0xCC, 0xDD],
        };

        let packed = adv.pack();

        // Build expected bytes manually
        let mut expected = Vec::new();
        expected.push(0x8b); // fixmap(11)

        // "t" => 100
        expected.extend_from_slice(&[0xa1, b't', 100]);

        // "d" => 50
        expected.extend_from_slice(&[0xa1, b'd', 50]);

        // "n" => 1
        expected.extend_from_slice(&[0xa1, b'n', 1]);

        // "h" => bin8(32) of zeros
        expected.extend_from_slice(&[0xa1, b'h', 0xc4, 32]);
        expected.extend_from_slice(&[0u8; 32]);

        // "r" => bin8(4) of zeros
        expected.extend_from_slice(&[0xa1, b'r', 0xc4, 4]);
        expected.extend_from_slice(&[0u8; 4]);

        // "o" => bin8(32) of zeros
        expected.extend_from_slice(&[0xa1, b'o', 0xc4, 32]);
        expected.extend_from_slice(&[0u8; 32]);

        // "i" => 1
        expected.extend_from_slice(&[0xa1, b'i', 1]);

        // "l" => 1
        expected.extend_from_slice(&[0xa1, b'l', 1]);

        // "q" => nil
        expected.extend_from_slice(&[0xa1, b'q', 0xc0]);

        // "f" => 0
        expected.extend_from_slice(&[0xa1, b'f', 0]);

        // "m" => bin8(4) [0xAA, 0xBB, 0xCC, 0xDD]
        expected.extend_from_slice(&[0xa1, b'm', 0xc4, 4, 0xAA, 0xBB, 0xCC, 0xDD]);

        assert_eq!(packed, expected);

        // Verify unpack produces the same advertisement
        let unpacked = ResourceAdvertisement::unpack(&expected).unwrap();
        assert_eq!(adv, unpacked);
    }

    // resource_sdu()
    #[test]
    fn test_resource_sdu_standard_mtu() {
        assert_eq!(resource_sdu(500), 464);
    }

    #[test]
    fn test_resource_sdu_large_mtu() {
        assert_eq!(resource_sdu(262144), 262108);
    }

    #[test]
    fn test_resource_sdu_zero_saturates() {
        assert_eq!(resource_sdu(0), 0);
    }

    #[test]
    fn test_resource_sdu_below_overhead() {
        assert_eq!(resource_sdu(30), 0);
    }

    // HASHMAP_MAX_LEN / COLLISION_GUARD_SIZE constants
    #[test]
    fn test_standard_link_mdu() {
        assert_eq!(STANDARD_LINK_MDU, 431);
    }

    #[test]
    fn test_hashmap_max_len_constant() {
        // (431 - 134) / 4 = 74
        assert_eq!(HASHMAP_MAX_LEN, 74);
    }

    #[test]
    fn test_collision_guard_size_constant() {
        // 2 * 75 + 74 = 224
        assert_eq!(COLLISION_GUARD_SIZE, 2 * RESOURCE_WINDOW_MAX_FAST + 74);
    }

    /// Codeberg #263 item 2: the default incoming-size ceiling must not refuse
    /// anything a stock Python sender can emit. The largest single
    /// advertisement it can produce carries one full segment's plaintext
    /// (`MAX_EFFICIENT_SIZE`, Resource.py:116 and the `MAX_EFFICIENT_SIZE`
    /// slice read at :296-313) plus the 4-byte random hash (:405-412), token-
    /// encrypted (:427). Anything below that sum would be an interop break
    /// traded for a denial of service, which is not a fix.
    #[test]
    fn default_incoming_limit_admits_the_largest_python_advertisement() {
        let largest_plaintext = RESOURCE_MAX_EFFICIENT_SIZE + RESOURCE_RANDOM_HASH_SIZE;
        let largest_on_the_wire = crate::link::Link::encrypted_size(largest_plaintext);

        assert!(
            largest_on_the_wire <= RESOURCE_MAX_INCOMING_SIZE,
            "default ceiling {RESOURCE_MAX_INCOMING_SIZE} refuses a stock Python \
             segment of {largest_on_the_wire} wire bytes"
        );
        const {
            assert!(
                RESOURCE_MAX_INCOMING_SIZE < usize::MAX,
                "an unbounded ceiling lets a peer choose our allocation size"
            )
        };
    }
}
