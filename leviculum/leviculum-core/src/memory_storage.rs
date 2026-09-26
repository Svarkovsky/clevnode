//! In-memory Storage implementation backed by BTreeMaps with configurable caps.
//!
//! `MemoryStorage` is the production Storage implementation for embedded targets
//! and the default test storage for core tests. It is NOT `#[cfg(test)]`, it is
//! always available.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::constants::{
    HASHLIST_MAXSIZE, MAX_PATH_REQUEST_TAGS, RATCHET_SIZE, RECEIPT_RETENTION_MS,
    TRUNCATED_HASHBYTES,
};
use crate::identity::Identity;
use crate::storage_types::{
    AnnounceEntry, AnnounceRateEntry, LinkEntry, PacketReceipt, PathEntry, PathState,
    ReceiptStatus, ReverseEntry,
};
use crate::traits::Storage;

/// Default identity capacity for desktop/Linux.
/// 50k identities at ~144 bytes × 3x BTreeMap overhead = ~21 MB max.
const DEFAULT_IDENTITY_CAP: usize = 5_000;

/// Compact packet hash capacity for constrained devices.
const COMPACT_PACKET_HASH_CAP: usize = 10_000;

/// Compact identity capacity for constrained devices.
const COMPACT_IDENTITY_CAP: usize = 1_000;

/// In-memory storage with configurable per-collection capacity limits.
///
/// Uses BTreeMap/BTreeSet for all collections. Not persistent, all data is
/// lost when the process exits. For persistent storage, use `FileStorage`
/// in `leviculum-std`.
pub struct MemoryStorage {
    // Capacity limits
    packet_hash_cap: usize,
    identity_cap: usize,

    // Packet dedup
    /// Current generation of packet hashes
    packet_cache: BTreeSet<[u8; 32]>,
    /// Previous generation (rotated out when current exceeds half cap)
    packet_cache_prev: BTreeSet<[u8; 32]>,

    // Path table
    path_table: BTreeMap<[u8; TRUNCATED_HASHBYTES], PathEntry>,
    path_states: BTreeMap<[u8; TRUNCATED_HASHBYTES], PathState>,

    // Reverse table
    reverse_table: BTreeMap<[u8; TRUNCATED_HASHBYTES], ReverseEntry>,

    // Link table
    link_table: BTreeMap<[u8; TRUNCATED_HASHBYTES], LinkEntry>,

    // Announce table
    announce_table: BTreeMap<[u8; TRUNCATED_HASHBYTES], AnnounceEntry>,
    announce_cache: BTreeMap<[u8; TRUNCATED_HASHBYTES], Vec<u8>>,
    announce_rate_table: BTreeMap<[u8; TRUNCATED_HASHBYTES], AnnounceRateEntry>,

    // Receipts
    receipts: BTreeMap<[u8; TRUNCATED_HASHBYTES], PacketReceipt>,

    // Path requests
    path_requests: BTreeMap<[u8; TRUNCATED_HASHBYTES], u64>,
    path_request_tags: VecDeque<[u8; 32]>,
    path_request_tag_set: BTreeSet<[u8; 32]>,

    // Known identities
    known_identities: BTreeMap<[u8; TRUNCATED_HASHBYTES], Identity>,

    // Known ratchets (sender-side cache)
    known_ratchets: BTreeMap<[u8; TRUNCATED_HASHBYTES], ([u8; RATCHET_SIZE], u64)>,

    // Local client destinations (per-interface tracking)
    local_client_dest_map: BTreeMap<usize, BTreeSet<[u8; TRUNCATED_HASHBYTES]>>,

    // Local client known destinations (persist across disconnects)
    local_client_known_dests: BTreeMap<[u8; TRUNCATED_HASHBYTES], u64>,

    // Discovery path requests
    /// Pending discovery path requests: dest_hash → (requesting_interface, timeout_ms)
    /// Removal: expire_discovery_path_requests() or remove_discovery_path_request()
    discovery_path_requests: BTreeMap<[u8; TRUNCATED_HASHBYTES], (usize, u64)>,

    // Sender-side ratchet keys (destination private keys)
    dest_ratchet_keys: BTreeMap<[u8; TRUNCATED_HASHBYTES], Vec<u8>>,

    // Known-destination cache lifecycle (Codeberg #84).
    // Mirrors Python's known_destinations[dest][4] use-state field: an entry is
    // either recency-touched (`Used`, Python >0) or pinned (`Retained`, Python
    // -1). Absence means "never used" (Python 0). Keyed by destination hash; the
    // authoritative "known" set is announce_cache. Retained entries survive
    // clean_announce_cache even without a path.
    known_dest_use: BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestUse>,
}

/// Cache-lifecycle state for a known destination, mirroring the fifth field of
/// Python's `Identity.known_destinations` entry (Codeberg #84).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KnownDestUse {
    /// Recency touch: last-used monotonic timestamp in ms (Python `time.time()`, >0).
    Used(u64),
    /// Pinned against eviction (Python sentinel -1).
    Retained,
}

impl MemoryStorage {
    /// Create MemoryStorage with generous defaults (suitable for Linux/desktop)
    pub fn with_defaults() -> Self {
        Self {
            packet_hash_cap: HASHLIST_MAXSIZE,
            identity_cap: DEFAULT_IDENTITY_CAP,
            packet_cache: BTreeSet::new(),
            packet_cache_prev: BTreeSet::new(),
            path_table: BTreeMap::new(),
            path_states: BTreeMap::new(),
            reverse_table: BTreeMap::new(),
            link_table: BTreeMap::new(),
            announce_table: BTreeMap::new(),
            announce_cache: BTreeMap::new(),
            announce_rate_table: BTreeMap::new(),
            receipts: BTreeMap::new(),
            path_requests: BTreeMap::new(),
            path_request_tags: VecDeque::new(),
            path_request_tag_set: BTreeSet::new(),
            known_identities: BTreeMap::new(),
            known_ratchets: BTreeMap::new(),
            local_client_dest_map: BTreeMap::new(),
            local_client_known_dests: BTreeMap::new(),
            discovery_path_requests: BTreeMap::new(),
            dest_ratchet_keys: BTreeMap::new(),
            known_dest_use: BTreeMap::new(),
        }
    }

    /// Create MemoryStorage with small caps (suitable for constrained devices)
    pub fn compact() -> Self {
        Self {
            packet_hash_cap: COMPACT_PACKET_HASH_CAP,
            identity_cap: COMPACT_IDENTITY_CAP,
            ..Self::with_defaults()
        }
    }

    // Test convenience methods
    /// Number of packet hashes in both generations
    pub fn packet_hash_count(&self) -> usize {
        self.packet_cache.len() + self.packet_cache_prev.len()
    }

    /// Number of path request tags stored
    pub fn path_request_tag_count(&self) -> usize {
        self.path_request_tags.len()
    }

    /// Number of link table entries
    pub fn link_entry_count(&self) -> usize {
        self.link_table.len()
    }

    /// Iterate over all link table entry values
    pub fn link_entry_values(&self) -> impl Iterator<Item = &LinkEntry> {
        self.link_table.values()
    }

    /// Clear all packet hashes (test convenience)
    pub fn clear_packet_hashes(&mut self) {
        self.packet_cache.clear();
        self.packet_cache_prev.clear();
    }

    /// Clear all state (test convenience)
    pub fn clear_all(&mut self) {
        self.packet_cache.clear();
        self.packet_cache_prev.clear();
        self.path_table.clear();
        self.path_states.clear();
        self.reverse_table.clear();
        self.link_table.clear();
        self.announce_table.clear();
        self.announce_cache.clear();
        self.announce_rate_table.clear();
        self.receipts.clear();
        self.path_requests.clear();
        self.path_request_tags.clear();
        self.path_request_tag_set.clear();
        self.known_identities.clear();
        self.known_ratchets.clear();
        self.local_client_dest_map.clear();
        self.local_client_known_dests.clear();
        self.discovery_path_requests.clear();
        self.dest_ratchet_keys.clear();
        self.known_dest_use.clear();
    }

    /// Number of entries in the announce rate table (test/stats convenience)
    pub fn announce_rate_count(&self) -> usize {
        self.announce_rate_table.len()
    }

    /// Iterate all packet hashes across both generations (for persistence on flush)
    pub fn packet_hash_iter(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.packet_cache
            .iter()
            .chain(self.packet_cache_prev.iter())
    }

    /// Iterate all known ratchets (for FileStorage disk persistence and expiry)
    pub fn known_ratchet_iter(
        &self,
    ) -> impl Iterator<Item = (&[u8; TRUNCATED_HASHBYTES], &([u8; RATCHET_SIZE], u64))> {
        self.known_ratchets.iter()
    }

    /// Iterate all known identities (for persistence on flush)
    pub fn known_identity_iter(
        &self,
    ) -> impl Iterator<Item = (&[u8; TRUNCATED_HASHBYTES], &Identity)> {
        self.known_identities.iter()
    }

    /// Rotate packet cache: current becomes prev, fresh empty set takes its place
    fn rotate_packet_cache(&mut self) {
        core::mem::swap(&mut self.packet_cache, &mut self.packet_cache_prev);
        self.packet_cache.clear();
    }

    /// Diagnostic dump for packet_cache and packet_cache_prev only.
    ///
    /// Returns (formatted_text, estimated_bytes). FileStorage overrides packet_cache
    /// with its own HashSet and calls only `diagnostic_dump_non_packet_cache()`.
    pub fn diagnostic_dump_packet_cache(&self) -> (String, u64) {
        use core::fmt::Write;
        let mut s = String::new();
        let mut total = 0u64;

        // packet_cache: BTreeSet<[u8; 32]>, 3x overhead
        let n = self.packet_cache.len();
        let raw = (n * 32) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "packet_cache: {} entries, raw {} bytes, estimated {} bytes (BTreeSet 3x)",
            n, raw, est
        );

        // packet_cache_prev: BTreeSet<[u8; 32]>, 3x
        let n = self.packet_cache_prev.len();
        let raw = (n * 32) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "packet_cache_prev: {} entries, raw {} bytes, estimated {} bytes (BTreeSet 3x)",
            n, raw, est
        );

        (s, total)
    }

    /// Diagnostic dump for all collections except packet_cache/packet_cache_prev.
    ///
    /// Used by FileStorage which manages its own packet_cache HashSets.
    pub fn diagnostic_dump_non_packet_cache(&self) -> (String, u64) {
        use core::fmt::Write;
        let mut s = String::new();
        let mut total = 0u64;

        // path_table: BTreeMap<[u8; 16], PathEntry>, 3x
        let n = self.path_table.len();
        let mut raw = 0u64;
        for entry in self.path_table.values() {
            raw += (TRUNCATED_HASHBYTES + 33 + entry.random_blobs.len() * 10) as u64;
        }
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "path_table: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // path_states: BTreeMap<[u8; 16], PathState>, 3x
        let n = self.path_states.len();
        let raw = (n * (TRUNCATED_HASHBYTES + 1)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "path_states: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // reverse_table: BTreeMap<[u8; 16], ReverseEntry>, 3x
        let n = self.reverse_table.len();
        let raw = (n * (TRUNCATED_HASHBYTES + 24)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "reverse_table: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // link_table: BTreeMap<[u8; 16], LinkEntry>, 3x
        let n = self.link_table.len();
        let mut raw = 0u64;
        for entry in self.link_table.values() {
            raw += (TRUNCATED_HASHBYTES + 51) as u64;
            if entry.peer_signing_key.is_some() {
                raw += 32;
            }
        }
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "link_table: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // announce_table: BTreeMap<[u8; 16], AnnounceEntry>, 3x
        let n = self.announce_table.len();
        let mut raw = 0u64;
        for entry in self.announce_table.values() {
            raw += (TRUNCATED_HASHBYTES + 28 + entry.raw_packet.len()) as u64;
        }
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "announce_table: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // announce_cache: BTreeMap<[u8; 16], Vec<u8>>, 3x
        let n = self.announce_cache.len();
        let mut raw = 0u64;
        for v in self.announce_cache.values() {
            raw += (TRUNCATED_HASHBYTES + v.len()) as u64;
        }
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "announce_cache: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // announce_rate_table: BTreeMap<[u8; 16], AnnounceRateEntry>, 3x
        let n = self.announce_rate_table.len();
        let raw = (n * (TRUNCATED_HASHBYTES + 17)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "announce_rate_table: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // receipts: BTreeMap<[u8; 16], PacketReceipt>, 3x
        let n = self.receipts.len();
        let raw = (n * (TRUNCATED_HASHBYTES + 81)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "receipts: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // path_requests: BTreeMap<[u8; 16], u64>, 3x
        let n = self.path_requests.len();
        let raw = (n * (TRUNCATED_HASHBYTES + 8)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "path_requests: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // path_request_tags: VecDeque<[u8; 32]>, 1x
        let n = self.path_request_tags.len();
        let raw = (n * 32) as u64;
        let est = raw;
        total += est;
        let _ = writeln!(
            s,
            "path_request_tags: {} entries, raw {} bytes, estimated {} bytes (VecDeque 1x)",
            n, raw, est
        );

        // path_request_tag_set: BTreeSet<[u8; 32]>, 3x
        let n = self.path_request_tag_set.len();
        let raw = (n * 32) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "path_request_tag_set: {} entries, raw {} bytes, estimated {} bytes (BTreeSet 3x)",
            n, raw, est
        );

        // known_identities: BTreeMap<[u8; 16], Identity>, 3x
        let n = self.known_identities.len();
        let raw = (n * (TRUNCATED_HASHBYTES + 128)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "known_identities: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // known_ratchets: BTreeMap<[u8; 16], ([u8; 32], u64)>, 3x
        let n = self.known_ratchets.len();
        let raw = (n * (TRUNCATED_HASHBYTES + RATCHET_SIZE + 8)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "known_ratchets: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // local_client_dest_map: BTreeMap<usize, BTreeSet<[u8; 16]>>, 3x
        let n: usize = self.local_client_dest_map.values().map(|s| s.len()).sum();
        let raw = (n * TRUNCATED_HASHBYTES + self.local_client_dest_map.len() * 8) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "local_client_dest_map: {} ifaces, {} dest entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            self.local_client_dest_map.len(), n, raw, est
        );

        // local_client_known_dests: BTreeMap<[u8; 16], u64>, 3x
        let n = self.local_client_known_dests.len();
        let raw = (n * (TRUNCATED_HASHBYTES + 8)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "local_client_known_dests: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // discovery_path_requests: BTreeMap<[u8; 16], (usize, u64)>, 3x
        let n = self.discovery_path_requests.len();
        let raw = (n * (TRUNCATED_HASHBYTES + 8 + 8)) as u64;
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "discovery_path_requests: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        // dest_ratchet_keys: BTreeMap<[u8; 16], Vec<u8>>, 3x
        let n = self.dest_ratchet_keys.len();
        let mut raw = 0u64;
        for v in self.dest_ratchet_keys.values() {
            raw += (TRUNCATED_HASHBYTES + v.len()) as u64;
        }
        let est = raw * 3;
        total += est;
        let _ = writeln!(
            s,
            "dest_ratchet_keys: {} entries, raw {} bytes, estimated {} bytes (BTreeMap 3x)",
            n, raw, est
        );

        (s, total)
    }
}

#[cfg(test)]
impl MemoryStorage {
    /// Test-only: count of known ratchets stored
    pub fn known_ratchet_count(&self) -> usize {
        self.known_ratchets.len()
    }

    /// Test-only: check if a destination hash is tracked for a local client interface
    pub fn has_local_client_dest(
        &self,
        iface_id: usize,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> bool {
        self.local_client_dest_map
            .get(&iface_id)
            .is_some_and(|set| set.contains(dest_hash))
    }

    /// Test-only: check if a destination hash is in the known-dest set
    pub fn has_local_client_known_dest(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        self.local_client_known_dests.contains_key(dest_hash)
    }
}

impl Storage for MemoryStorage {
    // Packet Dedup
    fn has_packet_hash(&self, hash: &[u8; 32]) -> bool {
        self.packet_cache.contains(hash) || self.packet_cache_prev.contains(hash)
    }

    fn add_packet_hash(&mut self, hash: [u8; 32]) {
        self.packet_cache.insert(hash);
        // Two-generation rotation: when current exceeds half cap, rotate
        if self.packet_cache.len() > self.packet_hash_cap / 2 {
            self.rotate_packet_cache();
        }
    }

    // Path Table
    fn get_path(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&PathEntry> {
        self.path_table.get(dest_hash)
    }

    fn set_path(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], entry: PathEntry) {
        self.path_table.insert(dest_hash, entry);
    }

    fn remove_path(&mut self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<PathEntry> {
        self.path_table.remove(dest_hash)
    }

    fn path_count(&self) -> usize {
        self.path_table.len()
    }

    fn expire_paths(&mut self, now_ms: u64) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        let mut expired = Vec::new();
        self.path_table.retain(|hash, entry| {
            if entry.expires_ms < now_ms {
                expired.push(*hash);
                false
            } else {
                true
            }
        });
        expired
    }

    fn earliest_path_expiry(&self) -> Option<u64> {
        self.path_table.values().map(|e| e.expires_ms).min()
    }

    fn path_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], PathEntry)> {
        self.path_table
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    fn announce_rate_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], AnnounceRateEntry)> {
        self.announce_rate_table
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect()
    }

    // Path State
    fn get_path_state(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<PathState> {
        self.path_states.get(dest_hash).copied()
    }

    fn set_path_state(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], state: PathState) {
        self.path_states.insert(dest_hash, state);
    }

    // Reverse Table
    fn get_reverse(&self, hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&ReverseEntry> {
        self.reverse_table.get(hash)
    }

    fn set_reverse(&mut self, hash: [u8; TRUNCATED_HASHBYTES], entry: ReverseEntry) {
        self.reverse_table.insert(hash, entry);
    }

    fn remove_reverse(&mut self, hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<ReverseEntry> {
        self.reverse_table.remove(hash)
    }

    fn reverse_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], ReverseEntry)> {
        self.reverse_table.iter().map(|(k, v)| (*k, *v)).collect()
    }

    // Link Table
    fn get_link_entry(&self, link_id: &[u8; TRUNCATED_HASHBYTES]) -> Option<&LinkEntry> {
        self.link_table.get(link_id)
    }

    fn get_link_entry_mut(
        &mut self,
        link_id: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&mut LinkEntry> {
        self.link_table.get_mut(link_id)
    }

    fn set_link_entry(&mut self, link_id: [u8; TRUNCATED_HASHBYTES], entry: LinkEntry) {
        self.link_table.insert(link_id, entry);
    }

    fn link_entries(&self) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)> {
        self.link_table
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    // Announce Table
    fn get_announce(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&AnnounceEntry> {
        self.announce_table.get(dest_hash)
    }

    fn get_announce_mut(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&mut AnnounceEntry> {
        self.announce_table.get_mut(dest_hash)
    }

    fn set_announce(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], entry: AnnounceEntry) {
        self.announce_table.insert(dest_hash, entry);
    }

    fn remove_announce(&mut self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<AnnounceEntry> {
        self.announce_table.remove(dest_hash)
    }

    fn announce_keys(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.announce_table.keys().copied().collect()
    }

    // Announce Cache
    fn get_announce_cache(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&Vec<u8>> {
        self.announce_cache.get(dest_hash)
    }

    fn set_announce_cache(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], raw: Vec<u8>) {
        self.announce_cache.insert(dest_hash, raw);
    }

    // Announce Rate
    fn get_announce_rate(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&AnnounceRateEntry> {
        self.announce_rate_table.get(dest_hash)
    }

    fn set_announce_rate(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        entry: AnnounceRateEntry,
    ) {
        self.announce_rate_table.insert(dest_hash, entry);
    }

    // Receipts
    fn get_receipt(&self, hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&PacketReceipt> {
        self.receipts.get(hash)
    }

    fn set_receipt(&mut self, hash: [u8; TRUNCATED_HASHBYTES], receipt: PacketReceipt) {
        self.receipts.insert(hash, receipt);
    }

    // Path Requests
    fn get_path_request_time(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<u64> {
        self.path_requests.get(dest_hash).copied()
    }

    fn set_path_request_time(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], time_ms: u64) {
        self.path_requests.insert(dest_hash, time_ms);
    }

    fn check_path_request_tag(&mut self, tag: &[u8; 32]) -> bool {
        if self.path_request_tag_set.contains(tag) {
            return true;
        }
        self.path_request_tags.push_back(*tag);
        self.path_request_tag_set.insert(*tag);
        while self.path_request_tags.len() > MAX_PATH_REQUEST_TAGS {
            if let Some(evicted) = self.path_request_tags.pop_front() {
                self.path_request_tag_set.remove(&evicted);
            }
        }
        false
    }

    // Known Identities
    fn get_identity(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&Identity> {
        self.known_identities.get(dest_hash)
    }

    fn set_identity(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], identity: Identity) {
        // Evict oldest when at cap (BTreeMap has no insertion order,
        // so we just remove the first key, deterministic but arbitrary)
        if self.known_identities.len() >= self.identity_cap
            && !self.known_identities.contains_key(&dest_hash)
        {
            if let Some(&first_key) = self.known_identities.keys().next() {
                self.known_identities.remove(&first_key);
            }
        }
        self.known_identities.insert(dest_hash, identity);
    }

    // Cleanup
    fn expire_reverses(&mut self, now_ms: u64, timeout_ms: u64) -> usize {
        let before = self.reverse_table.len();
        self.reverse_table
            .retain(|_, entry| now_ms.saturating_sub(entry.timestamp_ms) <= timeout_ms);
        before - self.reverse_table.len()
    }

    fn remove_reverse_entries_for_interface(&mut self, iface_index: usize) {
        self.reverse_table.retain(|_, e| {
            e.receiving_interface_index != iface_index && e.outbound_interface_index != iface_index
        });
    }

    fn expire_receipts(&mut self, now_ms: u64) -> Vec<PacketReceipt> {
        let mut expired = Vec::new();
        self.receipts.retain(|_, receipt| {
            if receipt.status == ReceiptStatus::Sent && receipt.is_expired(now_ms) {
                // A still-pending packet whose proof never came: timed out.
                // Returned so the caller can emit a ReceiptTimeout event.
                expired.push(receipt.clone());
                false
            } else if receipt.status != ReceiptStatus::Sent
                && now_ms.saturating_sub(receipt.sent_at_ms)
                    > receipt.timeout_ms.saturating_add(RECEIPT_RETENTION_MS)
            {
                // A terminal (Delivered/Failed) receipt kept only for the
                // retention grace window is now reaped silently: the outcome
                // already reached the application, so it is not returned as a
                // timeout (Codeberg #275).
                false
            } else {
                true
            }
        });
        expired
    }

    fn expire_link_entries(
        &mut self,
        now_ms: u64,
        link_timeout_ms: u64,
    ) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)> {
        let mut expired = Vec::new();
        self.link_table.retain(|hash, entry| {
            let is_expired = if entry.validated {
                now_ms.saturating_sub(entry.timestamp_ms) > link_timeout_ms
            } else {
                now_ms > entry.proof_timeout_ms
            };
            if is_expired {
                expired.push((*hash, entry.clone()));
                false
            } else {
                true
            }
        });
        expired
    }

    fn clean_stale_path_metadata(&mut self) {
        self.path_states
            .retain(|hash, _| self.path_table.contains_key(hash));
        self.announce_rate_table
            .retain(|hash, _| self.path_table.contains_key(hash));
    }

    fn clean_announce_cache(&mut self, local_destinations: &BTreeSet<[u8; TRUNCATED_HASHBYTES]>) {
        let known_dest_use = &self.known_dest_use;
        self.announce_cache.retain(|hash, _| {
            self.path_table.contains_key(hash)
                || local_destinations.contains(hash)
                || matches!(known_dest_use.get(hash), Some(KnownDestUse::Retained))
        });
        // Drop lifecycle state for destinations whose cached announce was
        // evicted (retained ones survive above, so this only reaps stale
        // Used timestamps), mirroring Python popping the whole entry.
        let announce_cache = &self.announce_cache;
        self.known_dest_use
            .retain(|hash, _| announce_cache.contains_key(hash));
    }

    fn announce_cache_keys(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.announce_cache.keys().copied().collect()
    }

    fn retain_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        // Python Identity._retain_destination_data: pin iff the destination is
        // known (has a cached announce), setting use-state to the -1 sentinel.
        if self.announce_cache.contains_key(dest) {
            self.known_dest_use.insert(*dest, KnownDestUse::Retained);
            true
        } else {
            false
        }
    }

    fn unretain_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES], now_ms: u64) -> bool {
        // Python Identity._unretain_destination_data: reset use-state to a
        // recency timestamp (lifting the pin) iff the destination is known.
        if self.announce_cache.contains_key(dest) {
            self.known_dest_use
                .insert(*dest, KnownDestUse::Used(now_ms));
            true
        } else {
            false
        }
    }

    fn used_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES], now_ms: u64) -> bool {
        // Python Identity._used_destination_data: touch recency only when the
        // destination is known AND not retained (use-state not < 0); a retained
        // entry is left pinned and the call reports False.
        if !self.announce_cache.contains_key(dest) {
            return false;
        }
        if matches!(self.known_dest_use.get(dest), Some(KnownDestUse::Retained)) {
            return false;
        }
        self.known_dest_use
            .insert(*dest, KnownDestUse::Used(now_ms));
        true
    }

    fn is_known_dest_retained(&self, dest: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        matches!(self.known_dest_use.get(dest), Some(KnownDestUse::Retained))
    }

    fn known_dest_last_used(&self, dest: &[u8; TRUNCATED_HASHBYTES]) -> Option<u64> {
        match self.known_dest_use.get(dest) {
            Some(KnownDestUse::Used(ts)) => Some(*ts),
            _ => None,
        }
    }

    fn remove_link_entries_for_interface(
        &mut self,
        iface_index: usize,
    ) -> Vec<([u8; TRUNCATED_HASHBYTES], LinkEntry)> {
        let mut removed = Vec::new();
        self.link_table.retain(|hash, entry| {
            if entry.received_interface_index == iface_index
                || entry.next_hop_interface_index == iface_index
            {
                removed.push((*hash, entry.clone()));
                false
            } else {
                true
            }
        });
        removed
    }

    fn remove_paths_for_interface(&mut self, iface_index: usize) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        let mut removed = Vec::new();
        self.path_table.retain(|hash, entry| {
            if entry.interface_index == iface_index {
                removed.push(*hash);
                false
            } else {
                true
            }
        });
        removed
    }

    // Deadlines
    fn earliest_receipt_deadline(&self) -> Option<u64> {
        self.receipts
            .values()
            .filter(|r| r.status == ReceiptStatus::Sent)
            .map(|r| r.sent_at_ms.saturating_add(r.timeout_ms))
            .min()
    }

    fn earliest_link_deadline(&self, link_timeout_ms: u64) -> Option<u64> {
        self.link_table
            .values()
            .map(|entry| {
                if entry.validated {
                    entry.timestamp_ms.saturating_add(link_timeout_ms)
                } else {
                    entry.proof_timeout_ms
                }
            })
            .min()
    }

    // Diagnostics
    fn diagnostic_dump(&self) -> (String, u64) {
        let (mut s, mut total) = self.diagnostic_dump_packet_cache();
        let (s2, total2) = self.diagnostic_dump_non_packet_cache();
        s.push_str(&s2);
        total += total2;
        (s, total)
    }

    // Known Ratchets
    fn get_known_ratchet(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<[u8; RATCHET_SIZE]> {
        self.known_ratchets.get(dest_hash).map(|(r, _)| *r)
    }

    fn remember_known_ratchet(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        ratchet: [u8; RATCHET_SIZE],
        received_at_ms: u64,
    ) {
        self.known_ratchets
            .insert(dest_hash, (ratchet, received_at_ms));
    }

    fn expire_known_ratchets(&mut self, now_ms: u64, expiry_ms: u64) -> usize {
        let before = self.known_ratchets.len();
        self.known_ratchets
            .retain(|_, (_, received_at)| now_ms.saturating_sub(*received_at) < expiry_ms);
        before - self.known_ratchets.len()
    }

    // Local Client Destinations
    fn add_local_client_dest(
        &mut self,
        iface_id: usize,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
    ) -> bool {
        self.local_client_dest_map
            .entry(iface_id)
            .or_default()
            .insert(dest_hash)
    }

    fn remove_local_client_dests(&mut self, iface_id: usize) {
        self.local_client_dest_map.remove(&iface_id);
    }

    // Local Client Known Destinations
    fn set_local_client_known_dest(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        last_seen_ms: u64,
    ) {
        self.local_client_known_dests
            .insert(dest_hash, last_seen_ms);
    }

    fn local_client_known_dest_hashes(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.local_client_known_dests.keys().copied().collect()
    }

    fn expire_local_client_known_dests(&mut self, now_ms: u64, expiry_ms: u64) -> usize {
        let before = self.local_client_known_dests.len();
        self.local_client_known_dests
            .retain(|_, last_seen| now_ms.saturating_sub(*last_seen) < expiry_ms);
        before - self.local_client_known_dests.len()
    }

    // Discovery Path Requests
    fn set_discovery_path_request(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        requesting_interface: usize,
        timeout_ms: u64,
    ) {
        // Only store first request (Python behavior at Transport.py:2793-2794)
        self.discovery_path_requests
            .entry(dest_hash)
            .or_insert((requesting_interface, timeout_ms));
    }

    fn get_discovery_path_request(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<(usize, u64)> {
        self.discovery_path_requests.get(dest_hash).copied()
    }

    fn remove_discovery_path_request(&mut self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) {
        self.discovery_path_requests.remove(dest_hash);
    }

    fn expire_discovery_path_requests(&mut self, now_ms: u64) -> usize {
        let before = self.discovery_path_requests.len();
        self.discovery_path_requests
            .retain(|_, (_, timeout)| *timeout > now_ms);
        before - self.discovery_path_requests.len()
    }

    fn discovery_path_request_dest_hashes(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.discovery_path_requests.keys().copied().collect()
    }

    // Sender-Side Ratchet Keys
    fn store_dest_ratchet_keys(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        serialized: Vec<u8>,
    ) {
        self.dest_ratchet_keys.insert(dest_hash, serialized);
    }

    fn load_dest_ratchet_keys(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<Vec<u8>> {
        self.dest_ratchet_keys.get(dest_hash).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::DestinationHash;
    use alloc::vec;

    #[test]
    fn test_packet_hash_dedup() {
        let mut s = MemoryStorage::with_defaults();
        let hash = [0x42u8; 32];
        assert!(!s.has_packet_hash(&hash));
        s.add_packet_hash(hash);
        assert!(s.has_packet_hash(&hash));
    }

    #[test]
    fn test_packet_hash_rotation() {
        let mut s = MemoryStorage {
            packet_hash_cap: 10,
            ..MemoryStorage::with_defaults()
        };
        // Add 6 hashes (exceeds half of 10 = 5), triggers rotation
        for i in 0..6u8 {
            let mut hash = [0u8; 32];
            hash[0] = i;
            s.add_packet_hash(hash);
        }
        // First 5 hashes should be in prev, hash[5] in current
        let mut hash0 = [0u8; 32];
        hash0[0] = 0;
        assert!(s.has_packet_hash(&hash0)); // in prev

        let mut hash5 = [0u8; 32];
        hash5[0] = 5;
        assert!(s.has_packet_hash(&hash5)); // in current
    }

    #[test]
    fn test_path_operations() {
        let mut s = MemoryStorage::with_defaults();
        let hash = [0x01u8; TRUNCATED_HASHBYTES];
        assert!(s.get_path(&hash).is_none());
        assert_eq!(s.path_count(), 0);

        s.set_path(
            hash,
            PathEntry {
                hops: 2,
                expires_ms: 5000,
                interface_index: 0,
                random_blobs: Vec::new(),
                next_hop: None,
                via_peer: None,
            },
        );
        assert_eq!(s.path_count(), 1);
        assert_eq!(s.get_path(&hash).unwrap().hops, 2);
        assert!(s.has_path(&hash));

        let removed = s.remove_path(&hash);
        assert!(removed.is_some());
        assert_eq!(s.path_count(), 0);
    }

    #[test]
    fn test_path_expiry() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];
        let h2 = [0x02u8; TRUNCATED_HASHBYTES];

        s.set_path(
            h1,
            PathEntry {
                hops: 0,
                expires_ms: 1000,
                interface_index: 0,
                random_blobs: Vec::new(),
                next_hop: None,
                via_peer: None,
            },
        );
        s.set_path(
            h2,
            PathEntry {
                hops: 0,
                expires_ms: 5000,
                interface_index: 0,
                random_blobs: Vec::new(),
                next_hop: None,
                via_peer: None,
            },
        );
        assert_eq!(s.earliest_path_expiry(), Some(1000));

        let expired = s.expire_paths(2000);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0], h1);
        assert_eq!(s.path_count(), 1);
    }

    #[test]
    fn test_reverse_table() {
        let mut s = MemoryStorage::with_defaults();
        let hash = [0x01u8; TRUNCATED_HASHBYTES];

        s.set_reverse(
            hash,
            ReverseEntry {
                timestamp_ms: 1000,
                receiving_interface_index: 0,
                outbound_interface_index: 1,
            },
        );
        assert!(s.get_reverse(&hash).is_some());
        assert_eq!(s.get_reverse(&hash).unwrap().receiving_interface_index, 0);

        let count = s.expire_reverses(2000, 500);
        assert_eq!(count, 1);
        assert!(s.get_reverse(&hash).is_none());
    }

    #[test]
    fn test_receipt_operations() {
        let mut s = MemoryStorage::with_defaults();
        let hash = [0x01u8; TRUNCATED_HASHBYTES];
        let receipt = PacketReceipt::new([0x42u8; 32], DestinationHash::new(hash), 1000);

        s.set_receipt(hash, receipt);
        assert!(s.get_receipt(&hash).is_some());
        assert_eq!(s.earliest_receipt_deadline(), Some(1000 + 30_000));

        let expired = s.expire_receipts(50_000);
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn delivered_receipts_are_reaped_after_the_retention_window() {
        // Codeberg #275: a proved send used to leak its receipt forever. Store
        // several delivered receipts, advance the clock past the retention
        // window, and the table must be empty with no spurious timeout events.
        let mut s = MemoryStorage::with_defaults();
        let sent_at = 1_000u64;
        let mut hashes = Vec::new();
        for i in 0..8u8 {
            let hash = [i; TRUNCATED_HASHBYTES];
            let mut receipt = PacketReceipt::new([i; 32], DestinationHash::new(hash), sent_at);
            receipt.set_delivered();
            s.set_receipt(hash, receipt);
            hashes.push(hash);
        }

        // Still inside the window: nothing reaped yet.
        let inside = sent_at + crate::constants::DATA_RECEIPT_TIMEOUT_MS;
        assert!(s.expire_receipts(inside).is_empty());
        assert!(hashes.iter().all(|h| s.get_receipt(h).is_some()));

        // Past timeout + retention: all gone, and no timeout events emitted
        // (they were delivered, not timed out).
        let after = sent_at
            + crate::constants::DATA_RECEIPT_TIMEOUT_MS
            + crate::constants::RECEIPT_RETENTION_MS
            + 1;
        let timed_out = s.expire_receipts(after);
        assert!(
            timed_out.is_empty(),
            "delivered receipts must not surface as timeouts"
        );
        assert!(
            hashes.iter().all(|h| s.get_receipt(h).is_none()),
            "every delivered receipt must be reaped"
        );
    }

    #[test]
    fn test_path_request_tag_dedup() {
        let mut s = MemoryStorage::with_defaults();
        let tag = [0x42u8; 32];
        assert!(!s.check_path_request_tag(&tag)); // first time: not duplicate
        assert!(s.check_path_request_tag(&tag)); // second time: duplicate
    }

    #[test]
    fn test_known_identities_cap() {
        let mut s = MemoryStorage {
            identity_cap: 2,
            ..MemoryStorage::with_defaults()
        };
        let id1 = Identity::generate(&mut rand_core::OsRng);
        let id2 = Identity::generate(&mut rand_core::OsRng);
        let id3 = Identity::generate(&mut rand_core::OsRng);

        s.set_identity([0x01; TRUNCATED_HASHBYTES], id1);
        s.set_identity([0x02; TRUNCATED_HASHBYTES], id2);
        assert_eq!(s.known_identities.len(), 2);

        // Third should evict first
        s.set_identity([0x03; TRUNCATED_HASHBYTES], id3);
        assert_eq!(s.known_identities.len(), 2);
        assert!(s.get_identity(&[0x01; TRUNCATED_HASHBYTES]).is_none());
    }

    #[test]
    fn test_known_ratchet_store_get() {
        let mut s = MemoryStorage::with_defaults();
        let hash = [0xaa; TRUNCATED_HASHBYTES];
        let ratchet = [0xbb; RATCHET_SIZE];

        assert!(s.get_known_ratchet(&hash).is_none());
        assert_eq!(s.known_ratchet_count(), 0);

        s.remember_known_ratchet(hash, ratchet, 1000);
        assert_eq!(s.get_known_ratchet(&hash), Some(ratchet));
        assert_eq!(s.known_ratchet_count(), 1);

        // Update replaces
        let ratchet2 = [0xcc; RATCHET_SIZE];
        s.remember_known_ratchet(hash, ratchet2, 2000);
        assert_eq!(s.get_known_ratchet(&hash), Some(ratchet2));
        assert_eq!(s.known_ratchet_count(), 1);
    }

    #[test]
    fn test_known_ratchet_expire() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01; TRUNCATED_HASHBYTES];
        let h2 = [0x02; TRUNCATED_HASHBYTES];
        let ratchet = [0xaa; RATCHET_SIZE];

        s.remember_known_ratchet(h1, ratchet, 1000);
        s.remember_known_ratchet(h2, ratchet, 5000);

        // Expire with threshold: h1 is 4000ms old, h2 is 0ms old
        let removed = s.expire_known_ratchets(5000, 3000);
        assert_eq!(removed, 1);
        assert!(s.get_known_ratchet(&h1).is_none());
        assert!(s.get_known_ratchet(&h2).is_some());
    }

    #[test]
    fn test_known_ratchet_iter() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01; TRUNCATED_HASHBYTES];
        let h2 = [0x02; TRUNCATED_HASHBYTES];
        let r1 = [0xaa; RATCHET_SIZE];
        let r2 = [0xbb; RATCHET_SIZE];

        s.remember_known_ratchet(h1, r1, 1000);
        s.remember_known_ratchet(h2, r2, 2000);

        let entries: alloc::vec::Vec<_> = s.known_ratchet_iter().collect();
        assert_eq!(entries.len(), 2);
        // BTreeMap is sorted by key
        assert_eq!(entries[0].0, &h1);
        assert_eq!(entries[0].1, &(r1, 1000));
        assert_eq!(entries[1].0, &h2);
        assert_eq!(entries[1].1, &(r2, 2000));
    }

    #[test]
    fn test_local_client_dest_add_remove() {
        let mut s = MemoryStorage::with_defaults();
        let hash = [0xaa; TRUNCATED_HASHBYTES];

        assert!(!s.has_local_client_dest(0, &hash));
        assert!(s.add_local_client_dest(0, hash)); // first insert returns true
        assert!(!s.add_local_client_dest(0, hash)); // duplicate returns false
        assert!(s.has_local_client_dest(0, &hash));
        assert!(!s.has_local_client_dest(1, &hash)); // different iface

        s.remove_local_client_dests(0);
        assert!(!s.has_local_client_dest(0, &hash));
    }

    #[test]
    fn test_local_client_known_dest_expire() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01; TRUNCATED_HASHBYTES];
        let h2 = [0x02; TRUNCATED_HASHBYTES];

        s.set_local_client_known_dest(h1, 1000);
        s.set_local_client_known_dest(h2, 5000);
        assert!(s.has_local_client_known_dest(&h1));
        assert_eq!(s.local_client_known_dest_hashes().len(), 2);

        let removed = s.expire_local_client_known_dests(5000, 3000);
        assert_eq!(removed, 1);
        assert!(!s.has_local_client_known_dest(&h1));
        assert!(s.has_local_client_known_dest(&h2));
    }

    #[test]
    fn test_dest_ratchet_keys_store_load() {
        let mut s = MemoryStorage::with_defaults();
        let hash = [0xaa; TRUNCATED_HASHBYTES];

        assert!(s.load_dest_ratchet_keys(&hash).is_none());

        let data = vec![1, 2, 3, 4, 5];
        s.store_dest_ratchet_keys(hash, data.clone());
        assert_eq!(s.load_dest_ratchet_keys(&hash), Some(data));
    }

    #[test]
    fn test_link_entry_expire() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];

        s.set_link_entry(
            h1,
            LinkEntry {
                timestamp_ms: 1000,
                next_hop_interface_index: 0,
                remaining_hops: 1,
                received_interface_index: 1,
                hops: 1,
                validated: true,
                proof_timeout_ms: 0,
                destination_hash: [0u8; TRUNCATED_HASHBYTES],
                peer_signing_key: None,
            },
        );

        // Not expired yet
        let expired = s.expire_link_entries(2000, 5000);
        assert!(expired.is_empty());

        // Expired
        let expired = s.expire_link_entries(10_000, 5000);
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn test_clean_stale_path_metadata() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];
        let h2 = [0x02u8; TRUNCATED_HASHBYTES];

        // h1 has a path, h2 does not
        s.set_path(
            h1,
            PathEntry {
                hops: 0,
                expires_ms: u64::MAX,
                interface_index: 0,
                random_blobs: Vec::new(),
                next_hop: None,
                via_peer: None,
            },
        );

        s.set_path_state(h1, PathState::Responsive);
        s.set_path_state(h2, PathState::Unresponsive); // stale
        s.set_announce_rate(
            h2,
            AnnounceRateEntry {
                last_ms: 0,
                rate_violations: 0,
                blocked_until_ms: 0,
            },
        ); // stale

        s.set_announce_cache(h1, vec![0xAA; 20]);
        s.set_announce_cache(h2, vec![0xBB; 20]);

        s.clean_stale_path_metadata();
        assert!(s.get_path_state(&h1).is_some());
        assert!(s.get_path_state(&h2).is_none());
        assert!(s.get_announce_rate(&h2).is_none());
        // clean_stale_path_metadata does NOT touch announce_cache
        assert!(s.get_announce_cache(&h1).is_some());
        assert!(s.get_announce_cache(&h2).is_some());

        // clean_announce_cache removes entries with no path AND not local
        let h3 = [0x03u8; TRUNCATED_HASHBYTES];
        s.set_announce_cache(h3, vec![0xCC; 20]);
        let mut local = BTreeSet::new();
        local.insert(h2); // h2 is "local" — no path but should survive
        s.clean_announce_cache(&local);
        assert!(s.get_announce_cache(&h1).is_some(), "has path → kept");
        assert!(s.get_announce_cache(&h2).is_some(), "local dest → kept");
        assert!(
            s.get_announce_cache(&h3).is_none(),
            "no path, not local → removed"
        );
    }

    #[test]
    fn test_default_identity_cap() {
        assert_eq!(DEFAULT_IDENTITY_CAP, 5_000);
    }

    // Known-destination cache lifecycle (Codeberg #84).
    #[test]
    fn test_retain_survives_clean_announce_cache() {
        let mut s = MemoryStorage::with_defaults();
        let pinned = [0x01u8; TRUNCATED_HASHBYTES];
        let plain = [0x02u8; TRUNCATED_HASHBYTES];
        s.set_announce_cache(pinned, vec![0xAA; 20]);
        s.set_announce_cache(plain, vec![0xBB; 20]);

        // Retain only takes effect for a known destination.
        assert!(s.retain_known_dest(&pinned), "known dest → retained");
        assert!(
            !s.retain_known_dest(&[0x09u8; TRUNCATED_HASHBYTES]),
            "unknown dest → not retained"
        );
        assert!(s.is_known_dest_retained(&pinned));

        // Neither has a path nor is local: only the pinned one survives.
        let empty = BTreeSet::new();
        s.clean_announce_cache(&empty);
        assert!(
            s.get_announce_cache(&pinned).is_some(),
            "retained → survives cache pressure"
        );
        assert!(
            s.get_announce_cache(&plain).is_none(),
            "non-retained, no path, not local → evicted (negative guard)"
        );
        // Use-state for the evicted entry is reaped; the pin persists.
        assert!(s.is_known_dest_retained(&pinned));

        // Unretain lifts the pin; the entry can then be evicted.
        assert!(s.unretain_known_dest(&pinned, 5_000));
        assert!(!s.is_known_dest_retained(&pinned));
        assert_eq!(s.known_dest_last_used(&pinned), Some(5_000));
        s.clean_announce_cache(&empty);
        assert!(
            s.get_announce_cache(&pinned).is_none(),
            "after unretain → evicted normally"
        );
    }

    #[test]
    fn test_used_touches_recency_and_skips_retained() {
        let mut s = MemoryStorage::with_defaults();
        let dest = [0x03u8; TRUNCATED_HASHBYTES];

        // Unknown destination: used reports false, no recency recorded.
        assert!(!s.used_known_dest(&dest, 1_000));
        assert_eq!(s.known_dest_last_used(&dest), None);

        // Known destination: used is a recency touch (Python >0).
        s.set_announce_cache(dest, vec![0xCC; 20]);
        assert!(s.used_known_dest(&dest, 1_000));
        assert_eq!(s.known_dest_last_used(&dest), Some(1_000));
        assert!(s.used_known_dest(&dest, 2_000));
        assert_eq!(s.known_dest_last_used(&dest), Some(2_000));

        // Once retained, used leaves the pin intact and reports false
        // (Python skips use-state < 0).
        assert!(s.retain_known_dest(&dest));
        assert!(!s.used_known_dest(&dest, 3_000));
        assert!(s.is_known_dest_retained(&dest));
        assert_eq!(
            s.known_dest_last_used(&dest),
            None,
            "retained entry exposes no recency timestamp"
        );
    }

    #[test]
    fn test_retained_survives_even_without_path_or_local() {
        // Explicit negative-vs-positive contrast in a single clean pass.
        let mut s = MemoryStorage::with_defaults();
        let a = [0x0Au8; TRUNCATED_HASHBYTES];
        let b = [0x0Bu8; TRUNCATED_HASHBYTES];
        let c = [0x0Cu8; TRUNCATED_HASHBYTES];
        s.set_announce_cache(a, vec![1; 8]);
        s.set_announce_cache(b, vec![2; 8]);
        s.set_announce_cache(c, vec![3; 8]);

        s.retain_known_dest(&b);
        s.used_known_dest(&c, 100); // touched but not pinned

        s.clean_announce_cache(&BTreeSet::new());
        assert!(s.get_announce_cache(&a).is_none(), "never used → evicted");
        assert!(s.get_announce_cache(&b).is_some(), "retained → kept");
        assert!(
            s.get_announce_cache(&c).is_none(),
            "recency-touched but not pinned → still evicted"
        );
    }

    // Announce table operations
    #[test]
    fn test_announce_table_operations() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];
        let h2 = [0x02u8; TRUNCATED_HASHBYTES];

        // Initially empty
        assert!(s.get_announce(&h1).is_none());
        assert!(s.announce_keys().is_empty());

        // Set and get
        s.set_announce(
            h1,
            AnnounceEntry {
                timestamp_ms: 1000,
                hops: 2,
                retries: 0,
                retransmit_at_ms: Some(2000),
                raw_packet: [0xAA; 10].to_vec(),
                receiving_interface_index: 0,
                target_interface: None,
                local_rebroadcasts: 0,
                block_rebroadcasts: false,
            },
        );
        assert_eq!(s.get_announce(&h1).unwrap().hops, 2);

        // Mutable access
        s.get_announce_mut(&h1).unwrap().retries = 3;
        assert_eq!(s.get_announce(&h1).unwrap().retries, 3);

        // Second entry and keys
        s.set_announce(
            h2,
            AnnounceEntry {
                timestamp_ms: 2000,
                hops: 1,
                retries: 0,
                retransmit_at_ms: None,
                raw_packet: [0xBB; 5].to_vec(),
                receiving_interface_index: 1,
                target_interface: None,
                local_rebroadcasts: 0,
                block_rebroadcasts: true,
            },
        );
        let keys = s.announce_keys();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&h1));
        assert!(keys.contains(&h2));

        // Remove
        let removed = s.remove_announce(&h1);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().hops, 2);
        assert!(s.get_announce(&h1).is_none());
        assert_eq!(s.announce_keys().len(), 1);

        // Announce cache
        assert!(s.get_announce_cache(&h2).is_none());
        s.set_announce_cache(h2, [0xCC; 20].to_vec());
        assert_eq!(s.get_announce_cache(&h2).unwrap().len(), 20);

        // Announce rate
        assert!(s.get_announce_rate(&h2).is_none());
        s.set_announce_rate(
            h2,
            AnnounceRateEntry {
                last_ms: 500,
                rate_violations: 2,
                blocked_until_ms: 3000,
            },
        );
        let rate = s.get_announce_rate(&h2).unwrap();
        assert_eq!(rate.rate_violations, 2);
        assert_eq!(rate.blocked_until_ms, 3000);
    }

    // earliest_link_deadline
    #[test]
    fn test_earliest_link_deadline() {
        let mut s = MemoryStorage::with_defaults();

        // Empty → None
        assert_eq!(s.earliest_link_deadline(5000), None);

        // Validated entry: deadline = timestamp_ms + link_timeout_ms
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];
        s.set_link_entry(
            h1,
            LinkEntry {
                timestamp_ms: 1000,
                next_hop_interface_index: 0,
                remaining_hops: 1,
                received_interface_index: 1,
                hops: 1,
                validated: true,
                proof_timeout_ms: 0,
                destination_hash: [0u8; TRUNCATED_HASHBYTES],
                peer_signing_key: None,
            },
        );
        assert_eq!(s.earliest_link_deadline(5000), Some(6000));

        // Unvalidated entry with earlier deadline: uses proof_timeout_ms directly
        let h2 = [0x02u8; TRUNCATED_HASHBYTES];
        s.set_link_entry(
            h2,
            LinkEntry {
                timestamp_ms: 2000,
                next_hop_interface_index: 0,
                remaining_hops: 1,
                received_interface_index: 1,
                hops: 1,
                validated: false,
                proof_timeout_ms: 3000,
                destination_hash: [0u8; TRUNCATED_HASHBYTES],
                peer_signing_key: None,
            },
        );
        // min(6000, 3000) = 3000
        assert_eq!(s.earliest_link_deadline(5000), Some(3000));
    }

    // Unvalidated link entry expiry
    #[test]
    fn test_link_entry_expire_unvalidated() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];

        s.set_link_entry(
            h1,
            LinkEntry {
                timestamp_ms: 1000,
                next_hop_interface_index: 0,
                remaining_hops: 1,
                received_interface_index: 1,
                hops: 1,
                validated: false,
                proof_timeout_ms: 5000,
                destination_hash: [0u8; TRUNCATED_HASHBYTES],
                peer_signing_key: None,
            },
        );

        // now_ms=4000 < proof_timeout_ms=5000 → not expired
        let expired = s.expire_link_entries(4000, 999_999);
        assert!(expired.is_empty());

        // now_ms=6000 > proof_timeout_ms=5000 → expired
        let expired = s.expire_link_entries(6000, 999_999);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, h1);
    }

    // remove_link_entries_for_interface
    #[test]
    fn test_remove_link_entries_for_interface() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];
        let h2 = [0x02u8; TRUNCATED_HASHBYTES];
        let h3 = [0x03u8; TRUNCATED_HASHBYTES];

        let make_entry = |recv: usize, next: usize| LinkEntry {
            timestamp_ms: 1000,
            next_hop_interface_index: next,
            remaining_hops: 1,
            received_interface_index: recv,
            hops: 1,
            validated: true,
            proof_timeout_ms: 0,
            destination_hash: [0u8; TRUNCATED_HASHBYTES],
            peer_signing_key: None,
        };

        // h1: received on iface 0, forwarded on iface 1
        s.set_link_entry(h1, make_entry(0, 1));
        // h2: received on iface 1, forwarded on iface 2
        s.set_link_entry(h2, make_entry(1, 2));
        // h3: received on iface 2, forwarded on iface 3
        s.set_link_entry(h3, make_entry(2, 3));

        // Remove all entries involving iface 1 (h1 forwards on 1, h2 received on 1)
        let removed = s.remove_link_entries_for_interface(1);
        assert_eq!(removed.len(), 2);
        assert!(s.get_link_entry(&h1).is_none());
        assert!(s.get_link_entry(&h2).is_none());
        assert!(s.get_link_entry(&h3).is_some());
    }

    // remove_paths_for_interface
    #[test]
    fn test_remove_paths_for_interface() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];
        let h2 = [0x02u8; TRUNCATED_HASHBYTES];
        let h3 = [0x03u8; TRUNCATED_HASHBYTES];

        let make_path = |iface: usize| PathEntry {
            hops: 1,
            expires_ms: u64::MAX,
            interface_index: iface,
            random_blobs: Vec::new(),
            next_hop: None,
            via_peer: None,
        };

        s.set_path(h1, make_path(0));
        s.set_path(h2, make_path(1));
        s.set_path(h3, make_path(1));

        let removed = s.remove_paths_for_interface(1);
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&h2));
        assert!(removed.contains(&h3));
        assert!(s.get_path(&h1).is_some());
        assert!(s.get_path(&h2).is_none());
        assert!(s.get_path(&h3).is_none());
    }

    // path_request_time get/set
    #[test]
    fn test_path_request_time() {
        let mut s = MemoryStorage::with_defaults();
        let h1 = [0x01u8; TRUNCATED_HASHBYTES];

        // Initially none
        assert_eq!(s.get_path_request_time(&h1), None);

        // Set and get
        s.set_path_request_time(h1, 42000);
        assert_eq!(s.get_path_request_time(&h1), Some(42000));

        // Overwrite
        s.set_path_request_time(h1, 99000);
        assert_eq!(s.get_path_request_time(&h1), Some(99000));
    }

    #[test]
    fn test_diagnostic_dump_empty() {
        let s = MemoryStorage::with_defaults();
        let (dump, total) = s.diagnostic_dump();
        assert!(dump.contains("packet_cache: 0 entries"));
        assert!(dump.contains("known_identities: 0 entries"));
        assert!(dump.contains("known_ratchets: 0 entries"));
        assert_eq!(total, 0);
    }

    #[test]
    fn test_diagnostic_dump_with_data() {
        let mut s = MemoryStorage::with_defaults();
        // Add some packet hashes
        s.add_packet_hash([0x01; 32]);
        s.add_packet_hash([0x02; 32]);
        // Add a path
        s.set_path(
            [0xAA; TRUNCATED_HASHBYTES],
            PathEntry {
                hops: 1,
                expires_ms: u64::MAX,
                interface_index: 0,
                random_blobs: Vec::new(),
                next_hop: None,
                via_peer: None,
            },
        );
        let (dump, total) = s.diagnostic_dump();
        assert!(dump.contains("packet_cache: 2 entries"));
        assert!(dump.contains("path_table: 1 entries"));
        assert!(total > 0);
    }
}
