//! Protocol constants defining the wire format, timing, and sizing for
//! Reticulum.
//!
//! Values match the Python Reticulum reference implementation to ensure
//! interoperability between the two stacks.
//!
//! # Constant Groups
//!
//! | Section | Covers |
//! |---------|--------|
//! | Protocol | MTU, header sizes, MDU, hop timeout |
//! | Keys | X25519, Ed25519, AES, HMAC sizes |
//! | Ratchet | Ratchet size, expiry, rotation interval |
//! | Transport | Max hops, path expiry, request grace |
//! | Link | Keepalive intervals, stale detection, pending timeout |
//! | Channel | Window sizes, RTT thresholds, retry/backoff parameters |
//! | Proof | Proof strategies, data sizes, receipt timeout |
//! | Resource | Window sizing, hash map length, auto-compress limit |
//! | Stream | Stream data message type, ID range, header, flags |
//! | IFAC | Interface Access Code sizes and salt |
//! | CRC | CRC-16-CCITT initial value, polynomial helpers |
//! | Token | Token encryption key sizes |
//! | Signaling | Link signaling bit masks and shifts |
//!
//! # Key Relationships
//!
//! ```text
//! MDU = MTU - HEADER_MAXSIZE - IFAC_MIN_SIZE
//!     = 500 - 35            - 1
//!     = 464 bytes
//! ```

// Utility Functions
/// Convert a slice to a fixed-size array by copying bytes from the given offset.
///
/// # Panics
/// Panics if `src.len() < offset + N`.
#[inline]
pub(crate) fn slice_to_array<const N: usize>(src: &[u8], offset: usize) -> [u8; N] {
    let mut arr = [0u8; N];
    arr.copy_from_slice(&src[offset..offset + N]);
    arr
}

// Protocol Constants
/// Physical layer MTU in bytes
pub const MTU: usize = 500;

/// Link encryption mode: AES-256-CBC (default)
pub const MODE_AES256_CBC: u8 = 0x01;

/// Truncated hash length in bits (for destination addresses)
pub const TRUNCATED_HASHLENGTH: usize = 128;

/// Truncated hash length in bytes
pub const TRUNCATED_HASHBYTES: usize = TRUNCATED_HASHLENGTH / 8; // 16 bytes

/// Identity hash length in bytes
pub const IDENTITY_HASHBYTES: usize = 16;

/// Name hash length in bytes
pub const NAME_HASHBYTES: usize = 10;

/// Random hash length in bytes (for announces)
pub const RANDOM_HASHBYTES: usize = 10;

/// Minimum header size (Header Type 1)
/// flags(1) + hops(1) + destination_hash(16) + context(1) = 19
pub const HEADER_MINSIZE: usize = 2 + 1 + TRUNCATED_HASHBYTES;

/// Maximum header size (Header Type 2 with transport ID)
/// flags(1) + hops(1) + transport_id(16) + destination_hash(16) + context(1) = 35
pub const HEADER_MAXSIZE: usize = 2 + 1 + TRUNCATED_HASHBYTES * 2;

/// Maximum data unit (payload after headers)
/// Formula: MTU - HEADER_MAXSIZE - IFAC_MIN_SIZE = 500 - 35 - 1 = 464 bytes
/// The IFAC_MIN_SIZE accounts for the minimum Interface Access Code size that
/// may be present in packets transmitted over authenticated interfaces.
pub const MDU: usize = MTU - HEADER_MAXSIZE - IFAC_MIN_SIZE;

/// Default per-hop timeout in seconds
pub const DEFAULT_PER_HOP_TIMEOUT: u64 = 6;

/// Key sizes
pub const X25519_KEY_SIZE: usize = 32;
pub const ED25519_KEY_SIZE: usize = 32;
pub const ED25519_SIGNATURE_SIZE: usize = 64;
pub const IDENTITY_KEY_SIZE: usize = X25519_KEY_SIZE + ED25519_KEY_SIZE; // 64 bytes

/// AES block size
pub const AES_BLOCK_SIZE: usize = 16;

/// AES-256 key size
pub const AES256_KEY_SIZE: usize = 32;

/// HMAC-SHA256 output size
pub const HMAC_SIZE: usize = 32;

/// Token overhead (IV + HMAC)
pub const TOKEN_OVERHEAD: usize = AES_BLOCK_SIZE + HMAC_SIZE; // 48 bytes

/// Ratchet constants
pub const RATCHET_SIZE: usize = 32;
pub const RATCHET_EXPIRY_SECS: u64 = 60 * 60 * 24 * 30; // 30 days
pub const RATCHET_INTERVAL_SECS: u64 = 60 * 30; // 30 minutes

/// Transport constants
pub const PATHFINDER_MAX_HOPS: u8 = 128;
/// Matches Python's PATHFINDER_R (reference/Reticulum/RNS/Transport.py:67).
/// Upper bound on the retries counter in the announce retry scheduler.
/// Combined with LOCAL_REBROADCASTS_MAX=2 this makes a received
/// non-local-client announce fire twice from the scheduler and a
/// local-client-sourced announce fire once. See
/// docs/src/architecture-broadcast-python-parity.md for the full walk.
pub const PATHFINDER_RETRIES: u8 = 1;
pub const PATHFINDER_EXPIRY_SECS: u64 = 60 * 60 * 24 * 2; // 2 days

/// Path expiry for destinations learned through an `AccessPoint`-mode
/// interface (Python `Transport.AP_PATH_TIME`, Transport.py:72 = 1 day).
pub const AP_PATH_TIME_SECS: u64 = 60 * 60 * 24; // 1 day
/// Path expiry for destinations learned through a `Roaming`-mode interface
/// (Python `Transport.ROAMING_PATH_TIME`, Transport.py:73 = 6 hours).
pub const ROAMING_PATH_TIME_SECS: u64 = 60 * 60 * 6; // 6 hours

/// Retry grace period for announce rebroadcast (milliseconds)
pub const PATHFINDER_G_MS: u64 = 5_000;
/// Random window added to retry grace period (milliseconds, Python PATHFINDER_RW = 0.5)
pub const PATHFINDER_RW_MS: u64 = 500;

/// Multiplier applied to the worst-case interface airtime when computing
/// the announce jitter window. Synchronous startups (multiple nodes
/// powered up at the same time) issue their first announce within
/// milliseconds of each other; the jitter window must be wide enough that
/// retries drift apart by more than one transmit duration, otherwise
/// every retry collides at the same air-time and the announce burst
/// fails as a whole. A factor of 3 means each retry's window spans
/// roughly three packet airtimes, sufficient drift on slow LoRa modes
/// where a single transmit takes several seconds.
pub const JITTER_AIRTIME_FACTOR: u64 = 3;

/// Maximum number of local (neighbor) rebroadcasts before suppressing
pub const LOCAL_REBROADCASTS_MAX: u8 = 2;

/// Link table entry timeout for validated links (milliseconds, 15 minutes)
pub const LINK_TIMEOUT_MS: u64 = 900_000;

/// Grace period before sending a path response (milliseconds)
/// Equivalent to Python's PATH_REQUEST_GRACE = 0.4 (seconds)
pub const PATH_REQUEST_GRACE_MS: u64 = 400;

/// Extra grace period a roaming-mode interface waits before answering a path
/// request, on top of `PATH_REQUEST_GRACE_MS`, so more well-connected peers can
/// respond first (Codeberg #104). Equivalent to Python's `PATH_REQUEST_RG = 1.5`
/// (seconds), Transport.py:82.
pub const PATH_REQUEST_RG_MS: u64 = 1500;

/// Delay before rebroadcasting a local client's first announce (milliseconds).
/// Batches multiple registrations during startup. Python Transport.py:2232.
pub const LOCAL_CLIENT_ANNOUNCE_DELAY_MS: u64 = 250;

/// Interval between management announces (probe destination, etc.).
/// Python: `mgmt_announce_interval = 2*60*60` (2 hours).
pub const MGMT_ANNOUNCE_INTERVAL_MS: u64 = 2 * 60 * 60 * 1000;

/// Expiry time for local client destination tracking (milliseconds).
/// 3 × management announce interval (6h). A client that hasn't re-announced
/// in 6 hours is considered gone.
pub const LOCAL_CLIENT_DEST_EXPIRY_MS: u64 = 3 * MGMT_ANNOUNCE_INTERVAL_MS;

/// Timeout for path request round-trip (milliseconds)
pub const PATH_REQUEST_TIMEOUT_MS: u64 = 15_000;

/// Timeout for discovery path request entries (milliseconds).
/// Longer than PATH_REQUEST_TIMEOUT_MS to allow multiple retries over slow
/// links (LoRa). With 5s retry interval: 6 attempts in 30s window.
pub const DISCOVERY_TIMEOUT_MS: u64 = 30_000;

/// Interval between discovery path request retries (milliseconds).
/// Each retry sends a fresh path request with a new random tag.
pub const DISCOVERY_RETRY_INTERVAL_MS: u64 = 5_000;

/// Lifetime of a pending local path request entry (Codeberg #171): a network
/// interface waiting for a local client's fresh PATH_RESPONSE. The reference
/// keeps `Transport.pending_local_path_requests` entries until the requesting
/// interface disappears (Transport.py:645-655); the additional time bound is
/// a deliberate, wire-invisible deviation that keeps the map bounded when a
/// client never answers. Sized to Python's `PATH_REQUEST_TIMEOUT = 15`
/// (Transport.py:79) — the requester itself gives up then, so a later
/// forward serves nobody.
pub const PENDING_LOCAL_PR_EXPIRY_MS: u64 = PATH_REQUEST_TIMEOUT_MS;

/// Maximum number of path request dedup tags to retain
pub const MAX_PATH_REQUEST_TAGS: usize = 8_000;

/// Minimum interval between path requests for the same destination (milliseconds)
pub const PATH_REQUEST_MIN_INTERVAL_MS: u64 = 20_000;

/// Link constants
pub const LINK_KEEPALIVE_SECS: u64 = 360; // 6 minutes (default/max)
pub const LINK_KEEPALIVE_MIN_SECS: u64 = 5; // Minimum keepalive interval
pub const LINK_KEEPALIVE_MAX_RTT: f64 = 1.75; // RTT at which max keepalive interval is reached
pub const LINK_STALE_FACTOR: u64 = 2;
pub const LINK_STALE_TIME_SECS: u64 = LINK_STALE_FACTOR * LINK_KEEPALIVE_SECS;
pub const LINK_KEEPALIVE_TIMEOUT_FACTOR: u64 = 4;
pub const LINK_STALE_GRACE_SECS: u64 = 5; // Grace period after stale before closing

/// Timeout for pending link handshakes (milliseconds)
pub const LINK_PENDING_TIMEOUT_MS: u64 = 30_000;

/// Per-hop timeout for link establishment (milliseconds).
/// Python: ESTABLISHMENT_TIMEOUT_PER_HOP = DEFAULT_PER_HOP_TIMEOUT = 6s
pub const ESTABLISHMENT_TIMEOUT_PER_HOP_MS: u64 = DEFAULT_PER_HOP_TIMEOUT * 1000;

/// Conservative bitrate assumption (bps) for multi-hop paths when the actual
/// next-hop bitrate is unknown. Covers LoRa SF12/BW125kHz (~293 bps).
///
/// Python solves this via `get_first_hop_timeout()` RPC to the transport daemon.
/// Rust clients that connect via TCP to a transport daemon don't have this RPC,
/// so they use this conservative estimate to avoid premature link timeout on
/// paths that traverse slow links (LoRa, etc.).
pub const UNKNOWN_BITRATE_ASSUMPTION_BPS: u32 = 300;

/// Responder establishment bonus (milliseconds).
/// With RTT retry (5 attempts at 10s intervals = ~50s budget), the
/// responder no longer needs Python's 360s bonus. 54s gives 4s margin
/// above the initiator's retry budget; reaping of dead links is handled
/// by the keepalive/stale watchdog, not the retry budget.
pub const ESTABLISHMENT_RESPONDER_BONUS_MS: u64 = 54_000;

/// Upper bound (permille of the base timeout) for the per-link random
/// establishment jitter (Codeberg #129). Two nodes that initiate links to
/// each other simultaneously otherwise time out and retransmit their
/// LinkRequests in lockstep (the base timeout is deterministic), colliding
/// again on a shared half-duplex medium — a phase-lock that kills both
/// handshakes. Up to +25% of the base spreads concurrent initiators apart
/// while scaling with the medium (the base already scales with bitrate and
/// hops). ADDITIVE only: the jittered timeout never drops below the base,
/// so it can never fire early. Local timing only, never on the wire, so it
/// is fully Python-RNS compatible. Tunable.
pub const MAX_ESTABLISHMENT_JITTER_PERMILLE: u16 = 250;

/// RTT packet retry: max additional attempts after the initial send.
/// This only bounds redelivery of the RTT packet; it never closes the link.
/// Reaping is handled by the keepalive/stale watchdog (Python-parity).
pub const RTT_RETRY_MAX_ATTEMPTS: u8 = 5;

/// Link request retry: minimum number of retries after the initial attempt.
/// The actual retry count is `max(this, hops)`, multi-hop paths get more
/// retries because each hop is an independent loss opportunity.
/// Total attempts = 1 + effective_retries.
/// Python Reticulum has no link request retry (single attempt only).
pub const LINK_REQUEST_MAX_RETRIES: u8 = 2;

/// RTT packet retry: minimum interval between attempts (milliseconds).
/// Actual interval is max(rtt_ms * RTT_RETRY_INTERVAL_MULTIPLIER, this).
pub const RTT_RETRY_MIN_INTERVAL_MS: u64 = 10_000;

/// RTT packet retry: interval = rtt_ms * this multiplier (floored by min).
pub const RTT_RETRY_INTERVAL_MULTIPLIER: u64 = 3;

/// Cleanup timeout for Channel packet receipts awaiting proofs (milliseconds).
///
/// Plain Link packets use the RTT-derived Python `PacketReceipt` timeout
/// instead; see [`TRAFFIC_TIMEOUT_FACTOR`] and [`TRAFFIC_TIMEOUT_MIN_MS`].
pub const DATA_RECEIPT_TIMEOUT_MS: u64 = 30_000;

/// How long a receipt in a terminal state (`Delivered` or `Failed`) is kept
/// after its timeout window before it is reaped (milliseconds).
///
/// A delivered or failed receipt has already reached the application as an
/// event, so nothing polls its status afterwards; it is retained only so a
/// duplicate or late proof arriving just after the outcome still finds the
/// receipt to validate against rather than looking like an unknown packet.
/// A proof round trip is seconds, so 30 s is several multiples of the window
/// in which a straggler can still arrive. Without this the `MemoryStorage`
/// receipt map grew unbounded for the daemon's lifetime, every proved send
/// leaking one entry (Codeberg #275); `EmbeddedStorage` capped at 8 instead,
/// so the two storages did not share the invariant the trait requires.
pub const RECEIPT_RETENTION_MS: u64 = 30_000;

/// Default announce rate limit interval (milliseconds)
pub const ANNOUNCE_RATE_LIMIT_MS: u64 = 2_000;

/// Default announce rate grace: number of violations allowed before blocking (Python default: 0)
pub const ANNOUNCE_RATE_GRACE: u8 = 0;

/// Default announce rate penalty: additional blocking time in milliseconds (Python default: 0)
pub const ANNOUNCE_RATE_PENALTY_MS: u64 = 0;

/// Maximum number of random blobs to retain per destination (matches Python MAX_RANDOM_BLOBS)
pub const MAX_RANDOM_BLOBS: usize = 64;

/// Default timeout factor for request/response (Python Link.TRAFFIC_TIMEOUT_FACTOR = 6)
pub const TRAFFIC_TIMEOUT_FACTOR: u64 = 6;

/// Minimum timeout for Link packet receipts (Python Link.TRAFFIC_TIMEOUT_MIN_MS = 5).
pub const TRAFFIC_TIMEOUT_MIN_MS: u64 = 5;

/// Max grace time for response in ms (Python Resource.RESPONSE_MAX_GRACE_TIME = 10s)
pub const RESPONSE_MAX_GRACE_TIME_MS: u64 = 10_000;

/// Maximum number of queued announces per interface (Python Interface.py:260)
pub const MAX_QUEUED_ANNOUNCES_PER_INTERFACE: usize = 512;

/// Default announce bandwidth cap as percentage of link capacity (Python Interface.py:25)
pub const DEFAULT_ANNOUNCE_CAP_PERCENT: u32 = 2;

/// Minimum bitrate (bps) a medium must sustain for a configured `bitrate` to be
/// honoured. A configured value below this is ignored and the interface keeps
/// its default (Python `Reticulum.MINIMUM_BITRATE`, Reticulum.py:134/794-796).
pub const MINIMUM_BITRATE: u32 = 5;

/// Maximum packet hashlist size (matches Python Transport.hashlist_maxsize)
///
/// When the current dedup set exceeds half this size, it rotates into the
/// "previous" set and a fresh empty set takes its place. Dedup checks both.
pub const HASHLIST_MAXSIZE: usize = 1_000_000;

/// Reverse table entry expiry time (milliseconds, 8 minutes per Python reference)
pub const REVERSE_TABLE_EXPIRY_MS: u64 = 480_000;

/// Keepalive packet payload byte sent by initiator
pub const KEEPALIVE_INITIATOR_BYTE: u8 = 0xFF;
/// Keepalive packet payload byte sent by responder (echo)
pub const KEEPALIVE_RESPONDER_BYTE: u8 = 0xFE;
/// Size of keepalive payload in bytes
pub const KEEPALIVE_PAYLOAD_SIZE: usize = 1;

/// Milliseconds per second (for time conversion)
pub const MS_PER_SECOND: u64 = 1000;

// Proof Constants
/// Proof strategy: Never generate proofs (default)
pub const PROVE_NONE: u8 = 0x21;
/// Proof strategy: Ask application via callback to decide
pub const PROVE_APP: u8 = 0x22;
/// Proof strategy: Automatically prove every packet
pub const PROVE_ALL: u8 = 0x23;

/// Proof data size (explicit format): packet_hash (32) + signature (64)
pub const PROOF_DATA_SIZE: usize = 32 + 64; // 96 bytes

/// Implicit proof size: signature only (64 bytes)
/// Python Reticulum uses implicit proofs by default.
/// The proof packet's destination_hash is the truncated_packet_hash,
/// allowing the receiver to look up the full packet_hash from its receipt.
pub const IMPLICIT_PROOF_SIZE: usize = 64;

/// Default receipt timeout in milliseconds
pub const RECEIPT_TIMEOUT_DEFAULT_MS: u64 = 30_000;

/// Resource constants
pub const RESOURCE_WINDOW_INITIAL: usize = 4;
pub const RESOURCE_WINDOW_MIN: usize = 2;
pub const RESOURCE_WINDOW_MAX_SLOW: usize = 10;
pub const RESOURCE_WINDOW_MAX_FAST: usize = 75;
pub const RESOURCE_HASHMAP_LEN: usize = 4;
pub const RESOURCE_AUTO_COMPRESS_MAX: usize = 64 * 1024 * 1024; // 64 MB
/// Fixed overhead in a ResourceAdvertisement msgpack payload (bytes).
/// Size of the advertisement excluding the variable-length hashmap data.
pub const RESOURCE_ADV_OVERHEAD: usize = 134;

// Channel Constants
/// Channel envelope header size: msgtype(2) + sequence(2) + length(2)
pub const CHANNEL_ENVELOPE_HEADER_SIZE: usize = 6;

/// Sequence number modulus for wraparound (2^16)
pub const CHANNEL_SEQ_MODULUS: u32 = 0x10000;

/// Initial window size for channel
pub const CHANNEL_WINDOW_INITIAL: usize = 2;

/// Minimum window size for slow links
pub const CHANNEL_WINDOW_MIN_SLOW: usize = 2;

/// Minimum window size for fast links (Python Channel.py:248 WINDOW_MIN_LIMIT_FAST = 16)
pub const CHANNEL_WINDOW_MIN_FAST: usize = 16;

/// Maximum window size for slow links
pub const CHANNEL_WINDOW_MAX_SLOW: usize = 5;

/// Minimum window size for very slow links (LoRa-class, RTT >= 2s)
pub const CHANNEL_WINDOW_MIN_VERY_SLOW: usize = 1;

/// Maximum window size for very slow links (LoRa-class, RTT >= 2s)
pub const CHANNEL_WINDOW_MAX_VERY_SLOW: usize = 3;

/// Maximum window size for medium-speed links
pub const CHANNEL_WINDOW_MAX_MEDIUM: usize = 12;

/// Maximum window size for fast links
pub const CHANNEL_WINDOW_MAX_FAST: usize = 48;

/// Maximum rx_ring capacity (out-of-order buffer).
/// Python's rx_ring is unbounded; we cap at 512 for embedded safety.
pub const CHANNEL_RX_RING_MAX: usize = 512;

/// Maximum transmission attempts before failure
pub const CHANNEL_MAX_TRIES: u8 = 8;

/// RTT threshold for fast links (180ms)
pub const CHANNEL_RTT_FAST_MS: u64 = 180;

/// RTT threshold for medium-speed links (750ms)
pub const CHANNEL_RTT_MEDIUM_MS: u64 = 750;

/// RTT threshold for very slow links (2000ms, LoRa-class)
pub const CHANNEL_RTT_VERY_SLOW_MS: u64 = 2000;

/// Default RTT when no measurement available (milliseconds)
pub const CHANNEL_DEFAULT_RTT_MS: u64 = 500;

/// Minimum base RTO. 25ms was unreachably low for any
/// real Reticulum link (LoRa: ~365ms/frame, BLE: ~20ms, LAN: ~1ms).
/// 100ms is defense-in-depth, the measured RTT * 2.5 always
/// dominates on any non-loopback link.
pub const CHANNEL_MIN_TIMEOUT_BASE_MS: f64 = 100.0;

/// RTT multiplier for channel timeout calculation
pub const CHANNEL_RTT_TIMEOUT_MULTIPLIER: f64 = 2.5;

/// Exponential backoff base for channel retries
pub const CHANNEL_BACKOFF_BASE: f64 = 1.5;

/// Microseconds per millisecond (for time conversion)
pub const US_PER_MS: u64 = 1000;

/// Reserved message type boundary (>= 0xf000 is reserved)
pub const CHANNEL_MSGTYPE_RESERVED: u16 = 0xf000;

// Stream Data Constants
/// StreamDataMessage type (system-reserved: 0xff00)
pub const STREAM_DATA_MSGTYPE: u16 = 0xff00;

/// Maximum stream ID (14 bits: 0-16383)
pub const STREAM_ID_MAX: u16 = 0x3fff;

/// Stream data header size (2 bytes for stream_id + flags)
pub const STREAM_DATA_HEADER_SIZE: usize = 2;

/// Compressed flag in stream header (bit 14)
pub const STREAM_FLAG_COMPRESSED: u16 = 0x4000;

/// EOF flag in stream header (bit 15)
pub const STREAM_FLAG_EOF: u16 = 0x8000;

/// Interface Access Code minimum size
pub const IFAC_MIN_SIZE: usize = 1;

/// Interface Access Code default size for serial interfaces (8 bytes)
pub const IFAC_DEFAULT_SIZE_SERIAL: usize = 8;

/// Interface Access Code default size for network interfaces (16 bytes)
pub const IFAC_DEFAULT_SIZE_NETWORK: usize = 16;

/// IFAC salt for HKDF key derivation (32 bytes)
/// This is a fixed constant from Python Reticulum
pub const IFAC_SALT: [u8; 32] = [
    0xad, 0xf5, 0x4d, 0x88, 0x2c, 0x9a, 0x9b, 0x80, 0x77, 0x1e, 0xb4, 0x99, 0x5d, 0x70, 0x2d, 0x4a,
    0x3e, 0x73, 0x33, 0x91, 0xb2, 0xa0, 0xf5, 0x3f, 0x41, 0x6d, 0x9f, 0x90, 0x7e, 0x55, 0xcf, 0xf8,
];

// CRC Constants
/// CRC-16-CCITT initial value
pub const CRC_INITIAL: u16 = 0xFFFF;

/// CRC-16 high bit mask for polynomial division
pub const CRC_HIGH_BIT: u16 = 0x8000;

/// Bits in a byte (for CRC bit shifting)
pub const BITS_PER_BYTE: usize = 8;

// Token Encryption Constants
/// Token key size (HMAC key + AES key)
pub const TOKEN_KEY_SIZE: usize = 64;

/// Token HMAC key size (first half of token key)
pub const TOKEN_HMAC_KEY_SIZE: usize = 32;

/// Token AES key size (second half of token key)
pub const TOKEN_AES_KEY_SIZE: usize = 32;

// Link Signaling Constants
/// 21-bit mask for MTU in signaling bytes
pub const SIGNALING_MTU_MASK: u32 = 0x1FFFFF;

/// 3-bit mask for mode in signaling bytes
pub const SIGNALING_MODE_MASK: u32 = 0x07;

/// Bit shift for mode in signaling bytes (21 bits for MTU)
pub const SIGNALING_MODE_SHIFT: u32 = 21;

// Random Hash Constants
//
// The emission-timestamp constants below implement the source-priority
// chain and hardening rules of docs/src/concepts/time-and-clocks.md;
// the concept document is the authoritative rationale.
/// Size of random portion in random hash (bytes)
pub const RANDOM_HASH_RANDOM_SIZE: usize = 5;

/// Size of timestamp portion in random hash (bytes)
pub const RANDOM_HASH_TIMESTAMP_SIZE: usize = 5;

/// Offset into timestamp bytes (skip high bytes)
pub const RANDOM_HASH_TIMESTAMP_OFFSET: usize = 3;

/// Highest emission value the announce random-hash timestamp field can
/// carry: `2^(8*RANDOM_HASH_TIMESTAMP_SIZE) - 1`. Anything larger silently
/// drops its high bits in the wire copy, so the emitted value collapses to
/// near zero and sorts below every stored path entry (Codeberg #160); all
/// emission producers saturate here.
pub const EMISSION_TIMESTAMP_MAX_SECS: u64 = (1 << (8 * RANDOM_HASH_TIMESTAMP_SIZE)) - 1;

/// Upper bound on emission timestamps a clockless node adopts as its
/// timebase, whether learned from an announce or injected by the host
/// (Codeberg #160): 2200-01-01T00:00:00Z. Must sit decades above any wall
/// clock the deployment can ever legitimately see (~174 years from 2026),
/// yet so far below [`EMISSION_TIMESTAMP_MAX_SECS`] that a floor at the
/// ceiling plus any achievable uptime cannot reach the field's truncation
/// point (~34,600 years of margin).
pub const EMISSION_LEARN_CEILING_SECS: u64 = 7_258_118_400;

/// Lower plausibility bound on emission timebases (Codeberg #161):
/// 1_600_000_000 = 2020-09-13T12:26:40Z, kept a round number. It only has
/// to separate two populations that sit orders of magnitude apart: real
/// wall clocks in any deployment running this stack are past 2026 (and a
/// battery-backed RTC set at device manufacture cannot predate the
/// stack's existence), while uptime-derived values cannot reach it — 1.6e9
/// seconds is ~50.7 years of continuous uptime. A learned floor BELOW this
/// bound is treated as if none existed, so one credible announce recovers
/// the node in a single unbounded step instead of the
/// [`EMISSION_LEARN_MAX_ADVANCE_SECS`]-per-announce crawl (#161 §1: ~429
/// days at a 30 min LoRa cadence); `set_wall_time_unix_secs` refuses
/// injections below it for the same reason. Deriving this bound (and a
/// tighter ceiling) from the firmware build timestamp would narrow the
/// remaining first-adoption window considerably and stays future work
/// (#161 §1); a constant needs no build plumbing and covers the routine
/// trigger — adopting a rebooting peer's uptime seconds — today.
pub const EMISSION_PLAUSIBLE_MIN_SECS: u64 = 1_600_000_000;

/// Once a clockless node has an emission timebase, a single announce may
/// advance it at most this far (Codeberg #160). Legitimate forward
/// corrections after the first adoption are bounded by peer clock skew
/// (minutes) plus our monotonic drift (~50 ppm, under half an hour per year
/// of isolation), so a day covers them with orders-of-magnitude headroom —
/// while a peer whose clock is decades wrong needs tens of thousands of
/// rate-limited announces instead of one to drag the timebase there.
/// Adoption is deliberately unbounded while the current timebase sits
/// below [`EMISSION_PLAUSIBLE_MIN_SECS`]: a node booting at uptime
/// seconds (Codeberg #155) — or one that adopted a rebooting peer's
/// uptime seconds as its first floor (Codeberg #161 §1) — must climb to
/// real unix time in one step.
pub const EMISSION_LEARN_MAX_ADVANCE_SECS: u64 = 86_400;

// BLE Interface Constants (Columba Protocol v2.2)
/// Reticulum BLE GATT service UUID
pub const BLE_SERVICE_UUID: &str = "37145b00-442d-4a94-917f-8f42c5da28e3";
/// BLE RX characteristic UUID (write by Central, receives data)
pub const BLE_RX_CHARACTERISTIC_UUID: &str = "37145b00-442d-4a94-917f-8f42c5da28e5";
/// BLE TX characteristic UUID (notify to Central, sends data)
pub const BLE_TX_CHARACTERISTIC_UUID: &str = "37145b00-442d-4a94-917f-8f42c5da28e4";
/// BLE Identity characteristic UUID (read, 16-byte transport identity hash)
pub const BLE_IDENTITY_CHARACTERISTIC_UUID: &str = "37145b00-442d-4a94-917f-8f42c5da28e6";
/// Maximum simultaneous BLE peer connections (nRF52840 hardware limit ~8)
pub const BLE_MAX_CONNECTIONS: usize = 7;
