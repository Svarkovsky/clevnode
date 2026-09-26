//! Platform abstraction traits for `leviculum-core`.
//!
//! These traits decouple all protocol logic from platform I/O, enabling the
//! same code to run on Linux/macOS (via `leviculum-std`) and on bare-metal
//! embedded targets (ESP32, nRF52, STM32).
//!
//! # Traits
//!
//! | Trait | Purpose | `std` example | Embedded example |
//! |-------|---------|---------------|------------------|
//! | [`Clock`] | Monotonic time | `SystemClock` | Hardware timer |
//! | [`Storage`] | Key-value persistence | `FileStorage` | Flash storage |
//! | [`Interface`] | Send-only network interface | `InterfaceHandle` (channel) | LoRa / BLE driver |
//!
//! # Sans-I/O Architecture
//!
//! The core protocol engine (`NodeCore`, `Transport`) never performs I/O
//! directly. Instead, it accepts incoming packets and emits [`Action`](crate::transport::Action)
//! values for the driver to execute.
//!
//! The [`Interface`] trait defines the **send side** of the interface contract:
//! the driver calls [`dispatch_actions()`](crate::transport::dispatch_actions)
//! which routes `Action` values to interfaces via [`Interface::try_send()`].
//! The **receive side** is driver-specific (async channels for tokio, interrupts
//! for embedded) and not part of this trait.
//!
//! # Platform Dependencies
//!
//! Functions that need platform services take explicit parameters:
//! - `rng: &mut impl CryptoRngCore` - for randomness
//! - `now_ms: u64` - for timestamps
//! - `storage: &mut impl Storage` - for persistence
//!
//! For devices that do not need persistence, use [`NoStorage`].

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use crate::constants::{RATCHET_SIZE, TRUNCATED_HASHBYTES};
use crate::identity::Identity;
use crate::storage_types::{
    AnnounceEntry, AnnounceRateEntry, LinkEntry, PacketReceipt, PathEntry, PathState, ReverseEntry,
};
use crate::transport::InterfaceId;

/// Error type for interface send operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceError {
    /// Outbound buffer full, packet dropped (non-fatal)
    BufferFull,
    /// Interface disconnected, driver must call handle_interface_down()
    Disconnected,
}

impl core::fmt::Display for InterfaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InterfaceError::BufferFull => write!(f, "buffer full"),
            InterfaceError::Disconnected => write!(f, "disconnected"),
        }
    }
}

/// Reticulum interface propagation mode.
///
/// Governs how announces are (re)broadcast on the interface and how paths
/// learned through it are treated. The discriminants match Python-RNS
/// (`RNS/Interfaces/Interface.py:45-50`) so the value can be reported over the
/// shared-instance IPC (`rnstatus`/`lnstatus`) without translation.
///
/// The mode is a config property carried by core/transport; the interface
/// driver itself stays media-agnostic and never inspects it. Only the
/// announce-propagation and path-selection logic in `transport` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum InterfaceMode {
    /// Full participation: announce, relay, and discover paths normally.
    #[default]
    Full = 0x01,
    /// Point-to-point link. Behaves like `Full` for announce propagation;
    /// carried for reporting parity only.
    PointToPoint = 0x02,
    /// Access point: never (re)broadcasts announces out on this interface,
    /// and paths learned through it expire quickly (`AP_PATH_TIME`).
    AccessPoint = 0x03,
    /// Roaming: withholds transit announces unless the origin is instance-local
    /// or the next hop is a well-connected (non-roaming/boundary) interface;
    /// paths learned through it expire quickly (`ROAMING_PATH_TIME`).
    Roaming = 0x04,
    /// Boundary: like `Roaming` but only withholds when the next hop is a
    /// roaming interface (a boundary next hop is still relayed).
    Boundary = 0x05,
    /// Gateway: behaves like `Full` for announce propagation; participates in
    /// path discovery for unknown destinations.
    Gateway = 0x06,
}

impl InterfaceMode {
    /// Wire/IPC value (matches Python `Interface.MODE_*`).
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether an interface in this mode triggers an *active* recursive path
    /// request when it receives a path request for a destination it does not
    /// know (Codeberg #104). Mirrors Python
    /// `Interface.DISCOVER_PATHS_FOR = [MODE_ACCESS_POINT, MODE_GATEWAY,
    /// MODE_ROAMING]` (Interface.py:54). A `Full` (or point-to-point/boundary)
    /// interface relies on passive announce propagation and does not
    /// re-originate discovery for unknown destinations.
    pub fn discovers_paths(self) -> bool {
        matches!(
            self,
            InterfaceMode::AccessPoint | InterfaceMode::Gateway | InterfaceMode::Roaming
        )
    }

    /// Parse a config `mode` / `interface_mode` value, mirroring the spellings
    /// accepted by Python `Reticulum._synthesize_interface` (Reticulum.py:717-745).
    /// Comparison is case-insensitive; an unrecognised value yields `None` so
    /// the caller can keep the `Full` default (matching Python, which leaves the
    /// mode unchanged on an unknown string).
    pub fn from_config_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "full" => Some(InterfaceMode::Full),
            "access_point" | "accesspoint" | "ap" => Some(InterfaceMode::AccessPoint),
            "pointtopoint" | "ptp" => Some(InterfaceMode::PointToPoint),
            "roaming" => Some(InterfaceMode::Roaming),
            "boundary" => Some(InterfaceMode::Boundary),
            "gateway" | "gw" => Some(InterfaceMode::Gateway),
            _ => None,
        }
    }
}

impl core::fmt::Display for InterfaceMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Matches the labels `rnstatus` prints (Utilities/rnstatus.py:421-427).
        let s = match self {
            InterfaceMode::Full => "Full",
            InterfaceMode::PointToPoint => "Point-to-Point",
            InterfaceMode::AccessPoint => "Access Point",
            InterfaceMode::Roaming => "Roaming",
            InterfaceMode::Boundary => "Boundary",
            InterfaceMode::Gateway => "Gateway",
        };
        write!(f, "{}", s)
    }
}

/// The concrete transport medium an interface runs over.
///
/// Unlike [`InterfaceMode`] (a Reticulum propagation property), this is the
/// carrier the driver actually built: TCP, UDP, I2P, LoRa, etc. It lets a status
/// consumer group interfaces by transport rather than by name — the name is only
/// a peer label, so an autoconnected TCP client to a peer discovered over I2P is
/// still `Tcp`.
///
/// # This is a label, not a switch
///
/// **Nothing outside the interface layer may branch on it.** The kind is
/// reported; it is never acted on. Only the interface knows the quirks of its
/// carrier medium — the core, the transport and the daemon are media-agnostic
/// (`docs/src/concepts/interface-isolation.md`). Timing, framing, duty cycle,
/// MTU and retry behaviour belong behind the [`Interface`] trait, which is why
/// [`Interface::kind`] exists to *describe* an interface and nothing more.
///
/// So a `match` on this enum outside `traits.rs` should only ever produce a
/// string, a number or a status field. If you are reaching for one to decide
/// *what the stack does* — "LoRa needs a longer timeout", "skip this on
/// serial" — the fix is at the wrong layer: widen the trait so the interface
/// answers the question itself. As of 2026-07-30 the only two consumers are
/// `transport.rs`'s sparse-map bookkeeping (`Unknown` means "no entry") and
/// `rpc/handlers.rs::interface_type`, which renders the Python-RNS class name
/// for `rnstatus`. Both are reporting; neither changes behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterfaceKind {
    Tcp,
    Udp,
    I2p,
    Serial,
    Rnode,
    Kiss,
    Local,
    Pipe,
    Channel,
    Auto,
    Ble,
    #[default]
    Unknown,
}

impl InterfaceKind {
    /// Stable lowercase token for status/IPC reporting.
    pub fn as_str(self) -> &'static str {
        match self {
            InterfaceKind::Tcp => "tcp",
            InterfaceKind::Udp => "udp",
            InterfaceKind::I2p => "i2p",
            InterfaceKind::Serial => "serial",
            InterfaceKind::Rnode => "rnode",
            InterfaceKind::Kiss => "kiss",
            InterfaceKind::Local => "local",
            InterfaceKind::Pipe => "pipe",
            InterfaceKind::Channel => "channel",
            InterfaceKind::Auto => "auto",
            InterfaceKind::Ble => "ble",
            InterfaceKind::Unknown => "unknown",
        }
    }
}

impl core::fmt::Display for InterfaceKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A network interface that can send packets (sync, non-blocking)
///
/// This trait defines the **send side** of the interface contract. The driver
/// implements it on whatever holds the outbound channel (e.g., a tokio mpsc
/// sender, an Embassy SPI handle, a LoRa radio driver).
///
/// The **receive side** is intentionally absent, receiving is async and
/// driver-specific (tokio channels, hardware interrupts, DMA). The driver
/// feeds received packets into core via `handle_packet()`.
///
/// Core's [`dispatch_actions()`](crate::transport::dispatch_actions) calls
/// `try_send()` on interfaces to route `Action` values to the network.
/// This keeps broadcast-exclusion and interface-selection logic in core,
/// so every driver (tokio, Embassy, bare-metal) gets it for free.
///
/// # Error handling
///
/// - `BufferFull`: non-fatal, packet dropped. Reticulum is best-effort
/// - `Disconnected`: driver must call `handle_interface_down()` for cleanup
pub trait Interface {
    /// Opaque identifier used by core for routing tables
    fn id(&self) -> InterfaceId;

    /// Human-readable name for logging
    fn name(&self) -> &str;

    /// Maximum transmission unit (max packet payload size)
    fn mtu(&self) -> usize;

    /// Default propagation mode for this interface.
    ///
    /// Almost always `Full`; the effective mode is set from config and held by
    /// `transport` (see `Transport::set_interface_mode`), so drivers stay
    /// media-agnostic and need not override this.
    fn mode(&self) -> InterfaceMode {
        InterfaceMode::default()
    }

    /// The transport medium this interface runs over (TCP, UDP, I2P, …).
    ///
    /// Reported in interface status so consumers group by transport rather than
    /// by name. Defaults to `Unknown`; the std driver overrides it from the
    /// interface it built.
    fn kind(&self) -> InterfaceKind {
        InterfaceKind::default()
    }

    /// Check if interface is online/connected
    fn is_online(&self) -> bool;

    /// Try to send a packet (non-blocking, fire-and-forget)
    ///
    /// Returns `Ok(())` if the packet was accepted for delivery.
    /// Returns `Err(BufferFull)` if the outbound buffer is full, packet is dropped.
    /// Returns `Err(Disconnected)` if the interface is dead.
    ///
    /// The implementation handles framing internally (e.g., HDLC for TCP).
    fn try_send(&mut self, data: &[u8]) -> Result<(), InterfaceError>;

    /// Try to send a packet with priority hint (non-blocking)
    ///
    /// `high_priority` signals that this packet should be sent before lower-priority
    /// queued packets (e.g., link requests/proofs vs announce rebroadcasts).
    /// Interfaces that don't support priority queuing ignore the hint via the
    /// default implementation.
    fn try_send_prioritized(
        &mut self,
        data: &[u8],
        high_priority: bool,
    ) -> Result<(), InterfaceError> {
        let _ = high_priority;
        self.try_send(data)
    }

    /// Try to send a packet the core has already addressed to one peer
    /// behind this interface (Codeberg #376).
    ///
    /// `peer` is the 16-byte identity a multi-peer interface reports on
    /// peer-up/peer-lost — the SAME value it stamps on an inbound packet
    /// through `Transport::process_incoming_from_peer`, and therefore the
    /// value a path entry carries as `via_peer`. `None` means the core has
    /// no addressee for these bytes: an announce, a path request, anything
    /// broadcast, and every packet whose next hop the core did not learn on
    /// a named peer link.
    ///
    /// The hint is a DELIVERY hint, not a routing decision: it names the
    /// peer, never a link, a connection handle or an address, so the core
    /// stays media-agnostic and the interface maps peer to link(s) itself.
    /// A single-peer interface (TCP, serial, LoRa's shared air) has nothing
    /// to map, which is why the default implementation drops the hint and
    /// behaves exactly like [`Self::try_send_prioritized`]. Only the two BLE
    /// interfaces override it, because only there does one interface hold
    /// several point-to-point links and a fan-out cost real airtime on the
    /// links the packet was never meant for.
    fn try_send_to_peer(
        &mut self,
        data: &[u8],
        peer: Option<&[u8; TRUNCATED_HASHBYTES]>,
        high_priority: bool,
    ) -> Result<(), InterfaceError> {
        let _ = peer;
        self.try_send_prioritized(data, high_priority)
    }

    /// Wall-clock time (ms) at which this interface will next accept a
    /// packet of the given size.
    ///
    /// The default implementation returns `now_ms`, the interface is
    /// always ready. LoRa/constrained interfaces override to return the
    /// earliest-fit time computed from their airtime budget.
    ///
    /// Transport uses this answer to schedule retries against interface
    /// capacity without itself knowing any radio physics (SF, BW, CR,
    /// CSMA backoff stay entirely below this boundary). For a Gigabit LAN
    /// interface the default is correct and zero-cost.
    fn next_slot_ms(&self, _size: usize, now_ms: u64) -> u64 {
        now_ms
    }
}

/// Clock for timestamps and timeouts
///
/// Implementations:
/// - `std`: `SystemClock` using `std::time::Instant`
/// - `embedded`: Hardware timer or `embassy::time::Instant`
///
/// The clock must be monotonic (never go backwards).
pub trait Clock {
    /// Milliseconds since some arbitrary epoch (monotonic)
    fn now_ms(&self) -> u64;

    /// Wall-clock unix time in seconds, if the platform has one.
    ///
    /// This is NOT a timer source. All timeout and deadline arithmetic must
    /// keep using the monotonic [`now_ms`](Self::now_ms); this method exists
    /// solely for wire fields that remote peers compare across our process
    /// lifetimes — today the 5-byte announce emission timestamp, which
    /// Python-RNS fills with `int(time.time())` (Destination.py:282) and
    /// orders same-destination paths by (Transport.py:1772/1809). A value
    /// derived from process start restarts from zero on every reboot and
    /// permanently loses that ordering (Codeberg #155).
    ///
    /// Platforms without a real-time clock return `None`; the transport
    /// then falls back to its learned emission timebase
    /// ([`Transport::emission_secs`](crate::transport::Transport::emission_secs)),
    /// seeded from validated received announces or an explicit host
    /// injection. Full source-priority chain and rules:
    /// `docs/src/concepts/time-and-clocks.md`.
    fn wall_unix_secs(&self) -> Option<u64> {
        None
    }

    /// Wall-clock unix time in microseconds, if the platform has one.
    ///
    /// Same contract as [`wall_unix_secs`](Self::wall_unix_secs) — a wire-field
    /// source, never a timer — with the sub-second precision that LXMF message
    /// timestamps need. `Message::create` hashes the timestamp into the message
    /// ID, so at whole-second granularity two identical messages sent inside one
    /// second are one message ID and the second is refused as a duplicate
    /// (Codeberg #217); the reference writes `time.time()` and sends both.
    ///
    /// Microseconds rather than milliseconds because the collision this closes
    /// is between two calls in the same code path, not two user actions: two
    /// consecutive `LxmfRouter::create_message` calls measure ~115 µs apart
    /// (each signs an Ed25519 message), so a millisecond clock collides on
    /// every such pair. `time.time()` on the reference side is an f64 of unix
    /// seconds, whose resolution at present-day timestamps is ~0.24 µs, so this
    /// is also the unit that matches what a Python peer produces.
    ///
    /// The default derives from `wall_unix_secs`, so a platform that already
    /// implements only whole seconds keeps working unchanged and simply gains no
    /// precision. Implementations with a finer clock should override this and
    /// leave `wall_unix_secs` as the truncation of the same reading.
    fn wall_unix_micros(&self) -> Option<u64> {
        self.wall_unix_secs()
            .map(|secs| secs.saturating_mul(1_000_000))
    }

    /// Seconds since epoch (convenience method)
    fn now_secs(&self) -> u64 {
        self.now_ms() / 1000
    }

    /// Check if a deadline (in ms) has passed
    fn has_elapsed(&self, deadline_ms: u64) -> bool {
        self.now_ms() >= deadline_ms
    }

    /// Calculate deadline from now + duration_ms
    fn deadline(&self, duration_ms: u64) -> u64 {
        self.now_ms().saturating_add(duration_ms)
    }
}

/// Type-safe storage for all Transport and NodeCore state
///
/// Storage is the source of truth for all long-lived collections that were
/// previously held as BTreeMap/BTreeSet fields on Transport and NodeCore.
/// Core asks questions ("have you seen this hash?"), tells Storage to
/// remember things, and Storage decides capacity, eviction, and persistence.
///
/// Implementations:
/// - `NoStorage`: zero-sized no-op (stubs, FFI, smoke tests)
/// - `MemoryStorage` (in `memory_storage` module): BTreeMap-backed with
///   configurable caps. Production implementation for embedded AND test
///   storage for core tests.
/// - `FileStorage` (in `leviculum-std`): wraps MemoryStorage + disk
///   persistence with Python-compatible file formats.
pub trait Storage {
    // Packet Dedup
    /// Check if a packet hash has been seen before
    fn has_packet_hash(&self, hash: &[u8; 32]) -> bool;

    /// Record a packet hash as seen. Implementations handle capacity/eviction.
    fn add_packet_hash(&mut self, hash: [u8; 32]);

    // Path Table
    /// Look up a path by destination hash
    fn get_path(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&PathEntry>;

    /// Insert or update a path
    fn set_path(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], entry: PathEntry);

    /// Remove a path entry
    fn remove_path(&mut self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<PathEntry>;

    /// Number of entries in the path table
    fn path_count(&self) -> usize;

    /// Remove all paths that have expired. Returns destination hashes of removed paths.
    fn expire_paths(&mut self, now_ms: u64) -> Vec<[u8; TRUNCATED_HASHBYTES]>;

    /// Earliest path expiry timestamp, or None if table is empty
    fn earliest_path_expiry(&self) -> Option<u64>;

    /// Check if a path exists
    fn has_path(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        self.get_path(dest_hash).is_some()
    }

    /// Return owned copies of all path table entries (for RPC export).
    fn path_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], PathEntry)>;

    /// Return owned copies of all announce rate entries (for RPC export).
    fn announce_rate_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], AnnounceRateEntry)>;

    // Path State
    /// Get path quality state for a destination
    fn get_path_state(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<PathState>;

    /// Set path quality state
    fn set_path_state(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], state: PathState);

    // Reverse Table
    /// Look up a reverse entry by packet hash
    fn get_reverse(&self, hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&ReverseEntry>;

    /// Insert a reverse entry
    fn set_reverse(&mut self, hash: [u8; TRUNCATED_HASHBYTES], entry: ReverseEntry);

    /// Remove a reverse entry
    fn remove_reverse(&mut self, hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<ReverseEntry>;

    /// Return owned copies of all reverse table entries (for RPC export).
    fn reverse_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], ReverseEntry)>;

    // Link Table
    /// Look up a link table entry
    fn get_link_entry(&self, link_id: &[u8; TRUNCATED_HASHBYTES]) -> Option<&LinkEntry>;

    /// Look up a mutable link table entry
    fn get_link_entry_mut(&mut self, link_id: &[u8; TRUNCATED_HASHBYTES])
        -> Option<&mut LinkEntry>;

    /// Insert or update a link table entry
    fn set_link_entry(&mut self, link_id: [u8; TRUNCATED_HASHBYTES], entry: LinkEntry);

    /// Check if a link table entry exists
    fn has_link_entry(&self, link_id: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        self.get_link_entry(link_id).is_some()
    }

    /// Return owned copies of all link table entries (for RPC export).
    fn link_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)>;

    // Announce Table
    /// Look up an announce entry
    fn get_announce(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&AnnounceEntry>;

    /// Look up a mutable announce entry
    fn get_announce_mut(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&mut AnnounceEntry>;

    /// Insert or update an announce entry
    fn set_announce(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], entry: AnnounceEntry);

    /// Remove an announce entry
    fn remove_announce(&mut self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<AnnounceEntry>;

    /// Return all destination hashes in the announce table
    fn announce_keys(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]>;

    // Announce Cache
    /// Get cached raw announce bytes for a destination
    fn get_announce_cache(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&Vec<u8>>;

    /// Cache raw announce bytes for a destination
    fn set_announce_cache(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], raw: Vec<u8>);

    /// Return every destination hash that has a cached announce (Codeberg #84).
    fn announce_cache_keys(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]>;

    // Known-destination cache lifecycle (Codeberg #84).
    // These mirror Python's Identity retain/used/unretain over the fifth
    // known_destinations field, driven over the shared-instance RPC.

    /// Pin a known destination against announce-cache cleaning. Returns true iff
    /// the destination is known (has a cached announce). Mirrors Python
    /// `Identity._retain_destination_data` (use-state -> -1).
    fn retain_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES]) -> bool;

    /// Lift a destination's retain pin, resetting recency to `now_ms`. Returns
    /// true iff known. Mirrors Python `Identity._unretain_destination_data`
    /// (use-state -> time.time()).
    fn unretain_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES], now_ms: u64) -> bool;

    /// Touch recency for a known, non-retained destination. Returns true iff the
    /// touch applied (known and not retained). Mirrors Python
    /// `Identity._used_destination_data` (skips use-state < 0).
    fn used_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES], now_ms: u64) -> bool;

    /// Whether a destination is currently retained (pinned).
    fn is_known_dest_retained(&self, dest: &[u8; TRUNCATED_HASHBYTES]) -> bool;

    /// Last-used recency timestamp (ms) for a destination, or `None` when it was
    /// never touched or is retained. Diagnostic/test accessor.
    fn known_dest_last_used(&self, dest: &[u8; TRUNCATED_HASHBYTES]) -> Option<u64>;

    // Announce Rate
    /// Get announce rate tracking for a destination
    fn get_announce_rate(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&AnnounceRateEntry>;

    /// Set announce rate tracking for a destination
    fn set_announce_rate(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], entry: AnnounceRateEntry);

    // Receipts
    /// Look up a receipt by truncated hash
    fn get_receipt(&self, hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&PacketReceipt>;

    /// Insert or update a receipt
    fn set_receipt(&mut self, hash: [u8; TRUNCATED_HASHBYTES], receipt: PacketReceipt);

    // Path Requests
    /// Get the last path request timestamp for a destination
    fn get_path_request_time(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<u64>;

    /// Set the last path request timestamp for a destination
    fn set_path_request_time(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], time_ms: u64);

    /// Check if a path request tag is a duplicate. If new, records it and returns false.
    /// If already seen, returns true.
    fn check_path_request_tag(&mut self, tag: &[u8; 32]) -> bool;

    // Known Identities
    /// Look up a known remote identity by destination hash
    fn get_identity(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&Identity>;

    /// Store a known remote identity
    fn set_identity(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], identity: Identity);

    // Known Ratchets (sender-side cache)
    /// Get the known ratchet public key for a destination (owned, not ref, disk-backed
    /// Storage can't return references to deserialized data).
    fn get_known_ratchet(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<[u8; RATCHET_SIZE]>;

    /// Remember a ratchet public key received from an announce.
    fn remember_known_ratchet(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        ratchet: [u8; RATCHET_SIZE],
        received_at_ms: u64,
    );

    /// Remove known ratchets older than `expiry_ms`. Returns count removed.
    fn expire_known_ratchets(&mut self, now_ms: u64, expiry_ms: u64) -> usize;

    // Local Client Destinations (per-interface tracking)
    /// Track a destination hash as belonging to a local client interface.
    /// Returns true if the hash was newly inserted.
    fn add_local_client_dest(
        &mut self,
        iface_id: usize,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
    ) -> bool;

    /// Remove all destination hashes for a local client interface.
    fn remove_local_client_dests(&mut self, iface_id: usize);

    // Local Client Known Destinations (persist across disconnects)
    /// Record a destination hash with its last-seen timestamp.
    fn set_local_client_known_dest(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        last_seen_ms: u64,
    );

    /// Return all known destination hashes (callers build BTreeSet locally if needed).
    fn local_client_known_dest_hashes(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]>;

    /// Remove entries older than `expiry_ms`. Returns count removed.
    fn expire_local_client_known_dests(&mut self, now_ms: u64, expiry_ms: u64) -> usize;

    // Discovery Path Requests
    /// Record a pending discovery path request.
    ///
    /// When a transport node forwards a path request for an unknown
    /// destination, it records the requesting interface. When a matching
    /// announce arrives, a targeted PATH_RESPONSE is sent to that interface.
    ///
    /// Only the first request is stored (Python behavior). Subsequent
    /// requests for the same destination within the timeout are ignored.
    ///
    /// Removal: `expire_discovery_path_requests()` in `clean_path_states()`,
    /// or `remove_discovery_path_request()` after successful delivery.
    fn set_discovery_path_request(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        requesting_interface: usize,
        timeout_ms: u64,
    );

    /// Get the requesting interface for a pending discovery path request.
    /// Returns `(requesting_interface, timeout_ms)` if present.
    fn get_discovery_path_request(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<(usize, u64)>;

    /// Remove a discovery path request (after delivery or invalidation).
    fn remove_discovery_path_request(&mut self, dest_hash: &[u8; TRUNCATED_HASHBYTES]);

    /// Remove expired discovery path requests. Returns count removed.
    fn expire_discovery_path_requests(&mut self, now_ms: u64) -> usize;

    /// Return destination hashes for all pending discovery path requests.
    /// Used by the retry mechanism to iterate entries without exposing
    /// the internal collection.
    fn discovery_path_request_dest_hashes(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]>;

    // Sender-Side Ratchet Keys (Destination private keys)
    /// Persist serialized ratchet private keys for a destination.
    fn store_dest_ratchet_keys(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        serialized: Vec<u8>,
    );

    /// Load serialized ratchet private keys for a destination.
    fn load_dest_ratchet_keys(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<Vec<u8>>;

    // Cleanup
    /// Remove expired reverse table entries. Returns count removed.
    fn expire_reverses(&mut self, now_ms: u64, timeout_ms: u64) -> usize;

    /// Remove reverse table entries referencing a specific interface (for interface-down cleanup)
    fn remove_reverse_entries_for_interface(&mut self, iface_index: usize);

    /// Remove expired receipts and return only the ones that timed out.
    ///
    /// Two kinds of receipt are dropped, and both implementations must agree
    /// on this so the receipt map is bounded on either target (Codeberg #275):
    ///  - `Sent` and past its timeout: the proof never came. These are
    ///    returned so the caller can emit a `ReceiptTimeout` event.
    ///  - Terminal (`Delivered`/`Failed`) and past
    ///    [`RECEIPT_RETENTION_MS`](crate::constants::RECEIPT_RETENTION_MS)
    ///    beyond its timeout window: the outcome already reached the
    ///    application, so it is dropped silently and never returned here.
    fn expire_receipts(&mut self, now_ms: u64) -> Vec<PacketReceipt>;

    /// Remove expired link table entries (validated past timeout, unvalidated past proof_timeout).
    /// Returns the removed entries for protocol logic (path rediscovery etc.).
    fn expire_link_entries(
        &mut self,
        now_ms: u64,
        link_timeout_ms: u64,
    ) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)>;

    /// Remove path_states and announce_rate entries for destinations no longer in path_table
    fn clean_stale_path_metadata(&mut self);

    /// Remove announce_cache entries for destinations that have no path and are not local.
    /// Called from Transport which knows the local destination set.
    fn clean_announce_cache(&mut self, local_destinations: &BTreeSet<[u8; TRUNCATED_HASHBYTES]>);

    /// Remove link table entries that reference a specific interface (for interface-down cleanup)
    fn remove_link_entries_for_interface(
        &mut self,
        iface_index: usize,
    ) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)>;

    /// Remove path entries that reference a specific interface (for interface-down cleanup).
    /// Returns destination hashes of removed paths.
    fn remove_paths_for_interface(&mut self, iface_index: usize) -> Vec<[u8; TRUNCATED_HASHBYTES]>;

    // Deadlines
    /// Earliest receipt deadline (sent_at + timeout), or None if no pending receipts
    fn earliest_receipt_deadline(&self) -> Option<u64>;

    /// Earliest link entry deadline, or None if table is empty
    fn earliest_link_deadline(&self, link_timeout_ms: u64) -> Option<u64>;

    // Flush
    /// Persist all dirty state to underlying storage (no-op for in-memory implementations)
    fn flush(&mut self) {}

    // Diagnostics
    /// Return a diagnostic dump of storage collection sizes and estimated byte usage.
    ///
    /// Returns (formatted_text, total_estimated_bytes). Default returns empty.
    fn diagnostic_dump(&self) -> (String, u64) {
        (String::new(), 0)
    }
}

/// Storage error type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    /// Storage is full
    Full,
    /// Key not found (for delete)
    NotFound,
    /// I/O error
    IoError,
    /// Data corruption
    Corrupted,
}

impl core::fmt::Display for StorageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StorageError::Full => write!(f, "storage full"),
            StorageError::NotFound => write!(f, "key not found"),
            StorageError::IoError => write!(f, "I/O error"),
            StorageError::Corrupted => write!(f, "data corrupted"),
        }
    }
}

/// No-op storage for devices without persistence
///
/// All lookups return None/false/0, all writes are no-ops.
/// Use this for stateless embedded devices or smoke tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoStorage;

impl Storage for NoStorage {
    fn has_packet_hash(&self, _hash: &[u8; 32]) -> bool {
        false
    }
    fn add_packet_hash(&mut self, _hash: [u8; 32]) {}

    fn get_path(&self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&PathEntry> {
        None
    }
    fn set_path(&mut self, _dest_hash: [u8; TRUNCATED_HASHBYTES], _entry: PathEntry) {}
    fn remove_path(&mut self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<PathEntry> {
        None
    }
    fn path_count(&self) -> usize {
        0
    }
    fn expire_paths(&mut self, _now_ms: u64) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        Vec::new()
    }
    fn earliest_path_expiry(&self) -> Option<u64> {
        None
    }

    fn path_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], PathEntry)> {
        Vec::new()
    }

    fn announce_rate_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], AnnounceRateEntry)> {
        Vec::new()
    }

    fn get_path_state(&self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<PathState> {
        None
    }
    fn set_path_state(&mut self, _dest_hash: [u8; TRUNCATED_HASHBYTES], _state: PathState) {}

    fn get_reverse(&self, _hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&ReverseEntry> {
        None
    }
    fn set_reverse(&mut self, _hash: [u8; TRUNCATED_HASHBYTES], _entry: ReverseEntry) {}
    fn remove_reverse(&mut self, _hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<ReverseEntry> {
        None
    }
    fn reverse_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], ReverseEntry)> {
        Vec::new()
    }

    fn get_link_entry(&self, _link_id: &[u8; TRUNCATED_HASHBYTES]) -> Option<&LinkEntry> {
        None
    }
    fn get_link_entry_mut(
        &mut self,
        _link_id: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&mut LinkEntry> {
        None
    }
    fn set_link_entry(&mut self, _link_id: [u8; TRUNCATED_HASHBYTES], _entry: LinkEntry) {}
    fn link_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)> {
        Vec::new()
    }

    fn get_announce(&self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&AnnounceEntry> {
        None
    }
    fn get_announce_mut(
        &mut self,
        _dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&mut AnnounceEntry> {
        None
    }
    fn set_announce(&mut self, _dest_hash: [u8; TRUNCATED_HASHBYTES], _entry: AnnounceEntry) {}
    fn remove_announce(&mut self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<AnnounceEntry> {
        None
    }
    fn announce_keys(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        Vec::new()
    }

    fn get_announce_cache(&self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&Vec<u8>> {
        None
    }
    fn set_announce_cache(&mut self, _dest_hash: [u8; TRUNCATED_HASHBYTES], _raw: Vec<u8>) {}
    fn announce_cache_keys(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        Vec::new()
    }
    fn retain_known_dest(&mut self, _dest: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        false
    }
    fn unretain_known_dest(&mut self, _dest: &[u8; TRUNCATED_HASHBYTES], _now_ms: u64) -> bool {
        false
    }
    fn used_known_dest(&mut self, _dest: &[u8; TRUNCATED_HASHBYTES], _now_ms: u64) -> bool {
        false
    }
    fn is_known_dest_retained(&self, _dest: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        false
    }
    fn known_dest_last_used(&self, _dest: &[u8; TRUNCATED_HASHBYTES]) -> Option<u64> {
        None
    }

    fn get_announce_rate(
        &self,
        _dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&AnnounceRateEntry> {
        None
    }
    fn set_announce_rate(
        &mut self,
        _dest_hash: [u8; TRUNCATED_HASHBYTES],
        _entry: AnnounceRateEntry,
    ) {
    }

    fn get_receipt(&self, _hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&PacketReceipt> {
        None
    }
    fn set_receipt(&mut self, _hash: [u8; TRUNCATED_HASHBYTES], _receipt: PacketReceipt) {}

    fn get_path_request_time(&self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<u64> {
        None
    }
    fn set_path_request_time(&mut self, _dest_hash: [u8; TRUNCATED_HASHBYTES], _time_ms: u64) {}
    fn check_path_request_tag(&mut self, _tag: &[u8; 32]) -> bool {
        false
    }

    fn get_identity(&self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&Identity> {
        None
    }
    fn set_identity(&mut self, _dest_hash: [u8; TRUNCATED_HASHBYTES], _identity: Identity) {}

    // Known Ratchets
    fn get_known_ratchet(
        &self,
        _dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<[u8; RATCHET_SIZE]> {
        None
    }
    fn remember_known_ratchet(
        &mut self,
        _dest_hash: [u8; TRUNCATED_HASHBYTES],
        _ratchet: [u8; RATCHET_SIZE],
        _received_at_ms: u64,
    ) {
    }
    fn expire_known_ratchets(&mut self, _now_ms: u64, _expiry_ms: u64) -> usize {
        0
    }

    // Local Client Destinations
    fn add_local_client_dest(
        &mut self,
        _iface_id: usize,
        _dest_hash: [u8; TRUNCATED_HASHBYTES],
    ) -> bool {
        false
    }
    fn remove_local_client_dests(&mut self, _iface_id: usize) {}

    // Local Client Known Destinations
    fn set_local_client_known_dest(
        &mut self,
        _dest_hash: [u8; TRUNCATED_HASHBYTES],
        _last_seen_ms: u64,
    ) {
    }
    fn local_client_known_dest_hashes(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        Vec::new()
    }
    fn expire_local_client_known_dests(&mut self, _now_ms: u64, _expiry_ms: u64) -> usize {
        0
    }

    // Discovery Path Requests
    fn set_discovery_path_request(
        &mut self,
        _dest_hash: [u8; TRUNCATED_HASHBYTES],
        _requesting_interface: usize,
        _timeout_ms: u64,
    ) {
    }
    fn get_discovery_path_request(
        &self,
        _dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<(usize, u64)> {
        None
    }
    fn remove_discovery_path_request(&mut self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) {}
    fn expire_discovery_path_requests(&mut self, _now_ms: u64) -> usize {
        0
    }
    fn discovery_path_request_dest_hashes(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        Vec::new()
    }

    // Sender-Side Ratchet Keys
    fn store_dest_ratchet_keys(
        &mut self,
        _dest_hash: [u8; TRUNCATED_HASHBYTES],
        _serialized: Vec<u8>,
    ) {
    }
    fn load_dest_ratchet_keys(&self, _dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<Vec<u8>> {
        None
    }

    fn expire_reverses(&mut self, _now_ms: u64, _timeout_ms: u64) -> usize {
        0
    }
    fn remove_reverse_entries_for_interface(&mut self, _iface_index: usize) {}
    fn expire_receipts(&mut self, _now_ms: u64) -> Vec<PacketReceipt> {
        Vec::new()
    }
    fn expire_link_entries(
        &mut self,
        _now_ms: u64,
        _link_timeout_ms: u64,
    ) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)> {
        Vec::new()
    }
    fn clean_stale_path_metadata(&mut self) {}
    fn clean_announce_cache(&mut self, _local: &BTreeSet<[u8; TRUNCATED_HASHBYTES]>) {}
    fn remove_link_entries_for_interface(
        &mut self,
        _iface_index: usize,
    ) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)> {
        Vec::new()
    }
    fn remove_paths_for_interface(
        &mut self,
        _iface_index: usize,
    ) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        Vec::new()
    }

    fn earliest_receipt_deadline(&self) -> Option<u64> {
        None
    }
    fn earliest_link_deadline(&self, _link_timeout_ms: u64) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockClock;
    use alloc::vec;

    #[test]
    fn test_no_storage_packet_hash() {
        let mut storage = NoStorage;
        assert!(!storage.has_packet_hash(&[0u8; 32]));
        storage.add_packet_hash([0u8; 32]);
        assert!(!storage.has_packet_hash(&[0u8; 32]));
    }

    #[test]
    fn test_no_storage_path() {
        let mut storage = NoStorage;
        let hash = [0u8; TRUNCATED_HASHBYTES];
        assert!(storage.get_path(&hash).is_none());
        assert!(!storage.has_path(&hash));
        assert_eq!(storage.path_count(), 0);
        storage.set_path(
            hash,
            PathEntry {
                hops: 0,
                expires_ms: 0,
                interface_index: 0,
                random_blobs: Vec::new(),
                next_hop: None,
                via_peer: None,
            },
        );
        assert!(storage.get_path(&hash).is_none());
    }

    #[test]
    fn test_no_storage_known_ratchets() {
        let mut storage = NoStorage;
        let hash = [0x42u8; TRUNCATED_HASHBYTES];
        assert!(storage.get_known_ratchet(&hash).is_none());
        storage.remember_known_ratchet(hash, [0xaa; 32], 1000);
        assert!(storage.get_known_ratchet(&hash).is_none()); // NoStorage is a no-op
        assert_eq!(storage.expire_known_ratchets(5000, 1000), 0);
    }

    #[test]
    fn test_no_storage_local_client() {
        let mut storage = NoStorage;
        let hash = [0x42u8; TRUNCATED_HASHBYTES];
        assert!(!storage.add_local_client_dest(0, hash));
        storage.remove_local_client_dests(0);
        storage.set_local_client_known_dest(hash, 1000);
        assert!(storage.local_client_known_dest_hashes().is_empty());
        assert_eq!(storage.expire_local_client_known_dests(5000, 1000), 0);
    }

    #[test]
    fn test_no_storage_dest_ratchet_keys() {
        let mut storage = NoStorage;
        let hash = [0x42u8; TRUNCATED_HASHBYTES];
        storage.store_dest_ratchet_keys(hash, vec![1, 2, 3]);
        assert!(storage.load_dest_ratchet_keys(&hash).is_none());
    }

    #[test]
    fn test_interface_mode_default() {
        // Default matches Python MODE_FULL (0x01).
        assert_eq!(InterfaceMode::default(), InterfaceMode::Full);
        assert_eq!(InterfaceMode::Full.as_u8(), 0x01);
        assert_eq!(InterfaceMode::PointToPoint.as_u8(), 0x02);
        assert_eq!(InterfaceMode::AccessPoint.as_u8(), 0x03);
        assert_eq!(InterfaceMode::Roaming.as_u8(), 0x04);
        assert_eq!(InterfaceMode::Boundary.as_u8(), 0x05);
        assert_eq!(InterfaceMode::Gateway.as_u8(), 0x06);
    }

    #[test]
    fn test_interface_mode_from_config_str() {
        use InterfaceMode::*;
        // Every spelling accepted by Python Reticulum.py:717-745.
        for (s, m) in [
            ("full", Full),
            ("access_point", AccessPoint),
            ("accesspoint", AccessPoint),
            ("ap", AccessPoint),
            ("AP", AccessPoint),
            ("pointtopoint", PointToPoint),
            ("ptp", PointToPoint),
            ("roaming", Roaming),
            ("Boundary", Boundary),
            ("gateway", Gateway),
            ("gw", Gateway),
        ] {
            assert_eq!(InterfaceMode::from_config_str(s), Some(m), "spelling {s}");
        }
        assert_eq!(InterfaceMode::from_config_str("nonsense"), None);
    }

    #[test]
    fn test_clock_deadline() {
        let clock = MockClock::new(1000);

        assert_eq!(clock.now_secs(), 1);
        assert_eq!(clock.deadline(500), 1500);
        assert!(!clock.has_elapsed(1500));
        assert!(clock.has_elapsed(1000));
        assert!(clock.has_elapsed(500));
    }

    /// Interface trait default `next_slot_ms` returns `now_ms` verbatim,
    /// modelling "always ready" semantics for TCP/UDP/Local.
    #[test]
    fn next_slot_ms_default_is_now() {
        struct AlwaysReady;
        impl Interface for AlwaysReady {
            fn id(&self) -> InterfaceId {
                InterfaceId(0)
            }
            fn name(&self) -> &str {
                "always-ready"
            }
            fn mtu(&self) -> usize {
                500
            }
            fn is_online(&self) -> bool {
                true
            }
            fn try_send(&mut self, _data: &[u8]) -> Result<(), InterfaceError> {
                Ok(())
            }
        }

        let iface = AlwaysReady;
        assert_eq!(iface.next_slot_ms(100, 12_345), 12_345);
        assert_eq!(iface.next_slot_ms(0, 0), 0);
    }

    /// An override is honoured, the default is overridable per impl.
    #[test]
    fn next_slot_ms_override_returns_custom_value() {
        struct DelayedByHundred;
        impl Interface for DelayedByHundred {
            fn id(&self) -> InterfaceId {
                InterfaceId(1)
            }
            fn name(&self) -> &str {
                "delayed"
            }
            fn mtu(&self) -> usize {
                500
            }
            fn is_online(&self) -> bool {
                true
            }
            fn try_send(&mut self, _data: &[u8]) -> Result<(), InterfaceError> {
                Ok(())
            }
            fn next_slot_ms(&self, _size: usize, now_ms: u64) -> u64 {
                now_ms + 100
            }
        }

        let iface = DelayedByHundred;
        assert_eq!(iface.next_slot_ms(100, 1_000), 1_100);
    }
}
