//! File-based storage with in-memory runtime state
//!
//! Wraps `MemoryStorage` for all runtime collections (paths, reverses, links,
//! announces, etc.) and adds disk persistence for four Python-compatible
//! collections:
//! - `known_destinations`: msgpack map of identity data (flush-on-shutdown)
//! - `packet_hashlist`: msgpack array of 32-byte dedup hashes (flush-on-shutdown)
//! - `ratchets/{hex_hash}`: receiver-side known ratchets (write-through)
//! - `ratchetkeys/{hex_hash}`: sender-side ratchet private keys (write-through)
//!
//! All non-persistent collections live in the inner `MemoryStorage` and are
//! lost on process exit. Flush-on-shutdown data is written by [`Storage::flush()`]
//! (called at shutdown from the driver). Ratchet data uses write-through
//! persistence (written immediately on each update).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::memory_storage::MemoryStorage;
use leviculum_core::traits::Storage as CoreStorage;
use leviculum_core::Identity;

use crate::error::{Error, Result};
use crate::file_known_destinations_store::FileKnownDestinationsStore;
use crate::file_packet_hash_store::FilePacketHashStore;
use crate::file_ratchet_store::FileRatchetStore;
use crate::known_destinations::{KnownDestEntry, PACKET_HASH_LEN};
use leviculum_core::known_destinations::KnownDestinationsStore;
use leviculum_core::packet_hash_store::PacketHashStore;
use leviculum_core::ratchet_store::RatchetStore;

/// Transparent fast hasher for cryptographically-random 32-byte packet hashes.
/// Eliminates SipHash-1-3 overhead on embedded 32-bit MIPS CPUs by taking
/// the first 8 bytes of the SHA-256 digest directly.
#[derive(Default, Clone, Copy, Debug)]
pub struct FastHashBuilder;

impl std::hash::BuildHasher for FastHashBuilder {
    type Hasher = FastHasher;
    #[inline(always)]
    fn build_hasher(&self) -> Self::Hasher {
        FastHasher(0)
    }
}

pub struct FastHasher(u64);

impl std::hash::Hasher for FastHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() >= 8 {
            self.0 = u64::from_ne_bytes(bytes[0..8].try_into().unwrap());
        } else {
            for &b in bytes {
                self.0 = self.0.wrapping_mul(31).wrapping_add(b as u64);
            }
        }
    }
}

pub type PacketHashSet = HashSet<[u8; 32], FastHashBuilder>;

/// Storage manager with in-memory runtime state and file-based persistence.
///
/// All runtime collections (paths, reverse entries, links, announces, receipts,
/// etc.) are delegated to the inner `MemoryStorage`. Two collections are
/// additionally persisted to disk in Python-compatible formats:
/// - known_destinations (identities)
/// - packet_hashlist (dedup hashes)
///
/// The legacy generic API (`load/store/delete/list_keys`) provides raw
/// file-based key-value storage for ratchets.
/// Default packet hash capacity for FileStorage (100k entries).
/// Two-generation rotation: each generation holds up to cap/2 entries.
/// At 32 bytes/entry × 1.5x HashSet overhead ≈ 4.8 MB max.
const FILE_STORAGE_PACKET_HASH_CAP: usize = 25_000;
const MAX_KNOWN_DESTINATIONS: usize = 5_000;
const KNOWN_DEST_EXPIRY_SECS: f64 = 60.0 * 60.0 * 24.0 * 30.0; // 30 days
const MAX_LOADED_RATCHETS: usize = 1_000;

pub struct Storage {
    // Snapshotted by take_flush_snapshot: the off-lock flush writer builds
    // its own store handles from this root (leviculum#44).
    base_path: PathBuf,
    inner: MemoryStorage,
    // Persistent store for write-through ratchets. The flush-on-interval
    // stores are stateless path wrappers, built on demand from base_path.
    ratchet_store: FileRatchetStore,
    // Known dest entries for merge logic
    known_dest_entries: BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>,
    // Packet hash state (two-generation rotation)
    packet_cache: PacketHashSet,
    packet_cache_prev: PacketHashSet,
    packet_hash_cap: usize,
    packet_hashes_dirty: bool,
    identities_dirty: bool,
    // Bumped on every dirtying mutation, not just false→true transitions:
    // finish_flush clears a dirty flag only while the generation still
    // matches its snapshot, so state dirtied mid-write stays dirty for the
    // next interval (leviculum#44).
    packet_hashes_gen: u64,
    identities_gen: u64,
    mono_offset_ms: u64,
    #[cfg(test)]
    flush_io_hook: Option<FlushIoHook>,
}

impl Storage {
    /// Create a new storage manager, loading persistent data from disk.
    ///
    /// Loads known_destinations and packet_hashlist from the given path
    /// and feeds them into the inner MemoryStorage for runtime use.
    ///
    /// `pub` for Codeberg #202: the type is already public as
    /// [`StdStorage`](crate::driver::StdStorage), because the alias
    /// [`StdNodeCore`](crate::driver::StdNodeCore) that a
    /// [`CoreProcessor`](crate::driver::CoreProcessor) is handed names it. A
    /// downstream crate that wants to drive its processor against a real core
    /// — in production, or in a unit test of its own hooks — needs to build
    /// one, and this is the only constructor. Everything else on this type
    /// stays `pub(crate)`; the core reaches it through
    /// [`leviculum_core::traits::Storage`], which is public already.
    pub fn new<P: AsRef<Path>>(base_path: P) -> Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();

        // Create directories if they don't exist
        std::fs::create_dir_all(&base_path)
            .map_err(|e| Error::Storage(format!("Failed to create storage dir: {e}")))?;

        let mut inner = MemoryStorage::with_defaults();

        // Load known_destinations via store
        let mut kd_store = FileKnownDestinationsStore::new(&base_path);
        let mut known_dest_entries = match kd_store.load_all() {
            Ok(entries) => {
                if !entries.is_empty() {
                    tracing::info!("Loaded {} known destinations from storage", entries.len());
                }
                entries
            }
            Err(e) => {
                tracing::warn!("Failed to load known_destinations: {e}");
                BTreeMap::new()
            }
        };

        // Prune expired entries (> 30 days old) and cap to MAX_KNOWN_DESTINATIONS newest
        let now_secs = unix_timestamp_secs();
        known_dest_entries.retain(|_, entry| {
            now_secs - entry.timestamp < KNOWN_DEST_EXPIRY_SECS
        });
        if known_dest_entries.len() > MAX_KNOWN_DESTINATIONS {
            let mut by_time: Vec<([u8; TRUNCATED_HASHBYTES], f64)> = known_dest_entries
                .iter()
                .map(|(k, v)| (*k, v.timestamp))
                .collect();
            by_time.sort_unstable_by(|a, b| (a.1 as u64).cmp(&(b.1 as u64)));
            let excess = by_time.len() - MAX_KNOWN_DESTINATIONS;
            for (k, _) in by_time.into_iter().take(excess) {
                known_dest_entries.remove(&k);
            }
        }

        // Feed identities into runtime storage
        for (hash, entry) in &known_dest_entries {
            if let Ok(identity) = Identity::from_public_key_bytes(&entry.public_key) {
                CoreStorage::set_identity(&mut inner, *hash, identity);
            }
        }

        // Load packet_hashlist via store
        let mut ph_store = FilePacketHashStore::new(&base_path);
        let mut packet_cache = PacketHashSet::with_hasher(FastHashBuilder);
        if let Ok(hashes) = ph_store.load_all() {
            if !hashes.is_empty() {
                tracing::info!("Loaded {} packet hashes from storage", hashes.len());
            }
            packet_cache.extend(hashes);
        }

        let mono_offset_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // Load ratchets via store (capped to MAX_LOADED_RATCHETS newest)
        let mut ratchet_store = FileRatchetStore::new(&base_path);
        if let Ok(mut ratchets) = ratchet_store.load_known_ratchets() {
            if ratchets.len() > MAX_LOADED_RATCHETS {
                ratchets.sort_unstable_by(|a, b| (b.1.received_at_secs as u64).cmp(&(a.1.received_at_secs as u64)));
                ratchets.truncate(MAX_LOADED_RATCHETS);
            }
            for (dest_hash, entry) in ratchets {
                let mono_ms = wallclock_secs_to_mono_ms(entry.received_at_secs, mono_offset_ms);
                inner.remember_known_ratchet(dest_hash, entry.ratchet, mono_ms);
            }
        }
        if let Ok(keys) = ratchet_store.load_dest_ratchet_keys() {
            for (dest_hash, data) in keys {
                CoreStorage::store_dest_ratchet_keys(&mut inner, dest_hash, data);
            }
        }

        Ok(Self {
            base_path,
            inner,
            ratchet_store,
            known_dest_entries,
            packet_cache,
            packet_cache_prev: PacketHashSet::with_hasher(FastHashBuilder),
            packet_hash_cap: FILE_STORAGE_PACKET_HASH_CAP,
            packet_hashes_dirty: false,
            identities_dirty: false,
            packet_hashes_gen: 0,
            identities_gen: 0,
            mono_offset_ms,
            #[cfg(test)]
            flush_io_hook: None,
        })
    }

    /// Get path for a specific storage category
    #[cfg(test)]
    pub(crate) fn category_path(&self, category: &str) -> PathBuf {
        self.base_path.join(category)
    }

    /// Ensure a category directory exists
    #[cfg(test)]
    pub(crate) fn ensure_category(&self, category: &str) -> Result<PathBuf> {
        let path = self.category_path(category);
        std::fs::create_dir_all(&path)
            .map_err(|e| Error::Storage(format!("Failed to create category dir: {e}")))?;
        Ok(path)
    }

    /// Read raw bytes from the storage root (no category subdirectory).
    #[cfg(test)]
    pub(crate) fn read_root(&self, name: &str) -> Result<Vec<u8>> {
        let path = self.base_path.join(name);
        std::fs::read(&path)
            .map_err(|e| Error::Storage(format!("Failed to read {}: {e}", path.display())))
    }

    /// Write raw bytes to the storage root (atomic via .tmp + rename)
    #[cfg(test)]
    pub(crate) fn write_root(&self, name: &str, data: &[u8]) -> Result<()> {
        atomic_write(&self.base_path.join(name), data)
    }

    /// Read raw bytes from storage
    #[cfg(test)]
    pub(crate) fn read_raw(&self, category: &str, name: &str) -> Result<Vec<u8>> {
        let path = self.category_path(category).join(name);
        std::fs::read(&path)
            .map_err(|e| Error::Storage(format!("Failed to read {}: {e}", path.display())))
    }

    /// Write raw bytes to storage (atomic via .tmp + rename)
    #[cfg(test)]
    pub(crate) fn write_raw(&self, category: &str, name: &str, data: &[u8]) -> Result<()> {
        let category_path = self.ensure_category(category)?;
        atomic_write(&category_path.join(name), data)
    }

    /// Read msgpack-serialized data
    #[cfg(test)]
    pub(crate) fn read<T: serde::de::DeserializeOwned>(
        &self,
        category: &str,
        name: &str,
    ) -> Result<T> {
        let data = self.read_raw(category, name)?;
        rmp_serde::from_slice(&data)
            .map_err(|e| Error::Serialization(format!("Failed to deserialize: {e}")))
    }

    /// Write msgpack-serialized data
    #[cfg(test)]
    pub(crate) fn write<T: serde::Serialize>(
        &self,
        category: &str,
        name: &str,
        value: &T,
    ) -> Result<()> {
        let data = rmp_serde::to_vec(value)
            .map_err(|e| Error::Serialization(format!("Failed to serialize: {e}")))?;
        self.write_raw(category, name, &data)
    }

    /// Check if a file exists
    #[cfg(test)]
    pub(crate) fn exists(&self, category: &str, name: &str) -> bool {
        self.category_path(category).join(name).exists()
    }

    /// Delete a file
    #[cfg(test)]
    pub(crate) fn delete(&self, category: &str, name: &str) -> Result<()> {
        let path = self.category_path(category).join(name);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| Error::Storage(format!("Failed to delete {}: {e}", path.display())))?;
        }
        Ok(())
    }

    // Ratchet disk persistence
    // load_known_ratchets_from_disk and load_dest_ratchet_keys_from_disk
    // moved to FileRatchetStore. encode/decode_known_ratchet moved to
    // file_ratchet_store.rs.
}

/// Convert monotonic milliseconds to wall-clock seconds (for disk storage).
fn mono_to_wallclock_secs(mono_ms: u64, mono_offset_ms: u64) -> f64 {
    mono_offset_ms.saturating_add(mono_ms) as f64 / 1000.0
}

/// Convert wall-clock seconds to monotonic milliseconds (for loading from disk).
fn wallclock_secs_to_mono_ms(secs: f64, mono_offset_ms: u64) -> u64 {
    let wallclock_ms = (secs * 1000.0) as u64;
    wallclock_ms.saturating_sub(mono_offset_ms)
}

/// Atomic write: write to .tmp then rename into place.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, data)
        .map_err(|e| Error::Storage(format!("Failed to write temp file: {e}")))?;
    std::fs::rename(&temp_path, path)
        .map_err(|e| Error::Storage(format!("Failed to rename temp file: {e}")))?;
    Ok(())
}

/// Current Unix timestamp in seconds (f64), matching Python `time.time()`.
fn unix_timestamp_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Test seam: stalls/observes the off-lock write (compiled out of release).
#[cfg(test)]
pub(crate) type FlushIoHook = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Dirty state captured under the node lock for one flush, written off it
/// (leviculum#44). Carries the storage root, not the live store handles:
/// the file stores are stateless path wrappers, so the off-lock writer
/// builds its own; the in-struct ratchet store stays with the write-through
/// path.
pub(crate) struct FlushSnapshot {
    base_path: PathBuf,
    identities: Option<(u64, BTreeMap<[u8; TRUNCATED_HASHBYTES], KnownDestEntry>)>,
    packet_hashes: Option<(u64, Vec<[u8; 32]>)>,
    #[cfg(test)]
    io_hook: Option<FlushIoHook>,
}

/// Which stores the write landed, tagged with the snapshot's generations;
/// finish_flush clears a dirty flag only while its generation still matches.
pub(crate) struct FlushOutcome {
    identities_written: Option<u64>,
    packet_hashes_written: Option<u64>,
}

impl FlushSnapshot {
    /// Blocking file IO — blocking pool from the event loop, inline at
    /// shutdown. Merges on-disk entries written by other processes
    /// (Python-compatible) before rewriting. Never touches the node lock.
    pub(crate) fn write(self) -> FlushOutcome {
        #[cfg(test)]
        if let Some(hook) = &self.io_hook {
            hook();
        }

        let mut outcome = FlushOutcome {
            identities_written: None,
            packet_hashes_written: None,
        };

        if let Some((generation, entries)) = self.identities {
            let mut kd_store = FileKnownDestinationsStore::new(&self.base_path);
            match kd_store.save_all(&entries) {
                Ok(()) => {
                    outcome.identities_written = Some(generation);
                    tracing::debug!("Saved {} known destinations to storage", entries.len());
                }
                Err(e) => {
                    tracing::error!("Failed to save known_destinations: {e}");
                }
            }
        }

        if let Some((generation, hashes)) = self.packet_hashes {
            let mut ph_store = FilePacketHashStore::new(&self.base_path);
            match ph_store.save_all(&hashes) {
                Ok(()) => {
                    outcome.packet_hashes_written = Some(generation);
                    tracing::debug!("Saved {} packet hashes to storage", hashes.len());
                }
                Err(e) => {
                    tracing::error!("Failed to save packet_hashlist: {e}");
                }
            }
        }

        outcome
    }
}

impl Storage {
    /// Phase 1, under the node lock, memory ops only: fold runtime
    /// identities into known_dest_entries and clone out everything dirty.
    /// None when nothing is dirty. Dirty flags stay set until finish_flush.
    pub(crate) fn take_flush_snapshot(&mut self) -> Option<FlushSnapshot> {
        if !self.identities_dirty && !self.packet_hashes_dirty {
            tracing::debug!("Flush skipped — nothing dirty");
            return None;
        }

        let identities = if self.identities_dirty {
            // Prune expired entries (> 30 days) before snapshotting for disk flush
            let now = unix_timestamp_secs();
            self.known_dest_entries
                .retain(|_, e| (now - e.timestamp) < KNOWN_DEST_EXPIRY_SECS);

            if self.known_dest_entries.len() > MAX_KNOWN_DESTINATIONS {
                let mut by_time: Vec<([u8; TRUNCATED_HASHBYTES], f64)> = self
                    .known_dest_entries
                    .iter()
                    .map(|(k, v)| (*k, v.timestamp))
                    .collect();
                by_time.sort_unstable_by(|a, b| (a.1 as u64).cmp(&(b.1 as u64)));
                let excess = self.known_dest_entries.len() - MAX_KNOWN_DESTINATIONS;
                for (k, _) in by_time.into_iter().take(excess) {
                    self.known_dest_entries.remove(&k);
                }
            }

            Some((self.identities_gen, self.known_dest_entries.clone()))
        } else {
            None
        };

        let packet_hashes = if self.packet_hashes_dirty {
            let hashes: Vec<[u8; 32]> = self
                .packet_cache
                .iter()
                .chain(self.packet_cache_prev.iter())
                .copied()
                .collect();
            Some((self.packet_hashes_gen, hashes))
        } else {
            None
        };

        Some(FlushSnapshot {
            base_path: self.base_path.clone(),
            identities,
            packet_hashes,
            #[cfg(test)]
            io_hook: self.flush_io_hook.clone(),
        })
    }

    /// Phase 3, under the node lock, map ops only: generation-guarded
    /// dirty-flag clear (an entry dirtied mid-write stays dirty).
    pub(crate) fn finish_flush(&mut self, outcome: FlushOutcome) {
        if outcome.identities_written == Some(self.identities_gen) {
            self.identities_dirty = false;
        }
        if outcome.packet_hashes_written == Some(self.packet_hashes_gen) {
            self.packet_hashes_dirty = false;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_flush_io_hook(&mut self, hook: FlushIoHook) {
        self.flush_io_hook = Some(hook);
    }
}

// Storage Trait Implementation
//
// All runtime collection methods delegate to the inner MemoryStorage.
// The legacy generic API (load/store/delete/list_keys) stays file-based
// for ratchet compatibility.

impl leviculum_core::traits::Storage for Storage {
    // Packet Dedup (own HashSet, not inner MemoryStorage)
    fn has_packet_hash(&self, hash: &[u8; 32]) -> bool {
        self.packet_cache.contains(hash) || self.packet_cache_prev.contains(hash)
    }
    fn add_packet_hash(&mut self, hash: [u8; 32]) {
        self.packet_cache.insert(hash);
        if self.packet_cache.len() > self.packet_hash_cap / 2 {
            std::mem::swap(&mut self.packet_cache, &mut self.packet_cache_prev);
            self.packet_cache.clear();
        }
        self.packet_hashes_dirty = true;
        self.packet_hashes_gen = self.packet_hashes_gen.wrapping_add(1);
    }

    // Path Table
    fn get_path(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&leviculum_core::storage_types::PathEntry> {
        self.inner.get_path(dest_hash)
    }
    fn set_path(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        entry: leviculum_core::storage_types::PathEntry,
    ) {
        self.inner.set_path(dest_hash, entry)
    }
    fn remove_path(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<leviculum_core::storage_types::PathEntry> {
        self.inner.remove_path(dest_hash)
    }
    fn path_count(&self) -> usize {
        self.inner.path_count()
    }
    fn expire_paths(&mut self, now_ms: u64) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.inner.expire_paths(now_ms)
    }
    fn earliest_path_expiry(&self) -> Option<u64> {
        self.inner.earliest_path_expiry()
    }
    fn path_entries(
        &self,
    ) -> Vec<(
        [u8; TRUNCATED_HASHBYTES],
        leviculum_core::storage_types::PathEntry,
    )> {
        self.inner.path_entries()
    }
    fn announce_rate_entries(
        &self,
    ) -> Vec<(
        [u8; TRUNCATED_HASHBYTES],
        leviculum_core::storage_types::AnnounceRateEntry,
    )> {
        self.inner.announce_rate_entries()
    }

    // Path State
    fn get_path_state(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<leviculum_core::storage_types::PathState> {
        self.inner.get_path_state(dest_hash)
    }
    fn set_path_state(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        state: leviculum_core::storage_types::PathState,
    ) {
        self.inner.set_path_state(dest_hash, state)
    }

    // Reverse Table
    fn get_reverse(
        &self,
        hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&leviculum_core::storage_types::ReverseEntry> {
        self.inner.get_reverse(hash)
    }
    fn set_reverse(
        &mut self,
        hash: [u8; TRUNCATED_HASHBYTES],
        entry: leviculum_core::storage_types::ReverseEntry,
    ) {
        self.inner.set_reverse(hash, entry)
    }
    fn remove_reverse(
        &mut self,
        hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<leviculum_core::storage_types::ReverseEntry> {
        self.inner.remove_reverse(hash)
    }
    fn reverse_entries(
        &self,
    ) -> Vec<(
        [u8; TRUNCATED_HASHBYTES],
        leviculum_core::storage_types::ReverseEntry,
    )> {
        self.inner.reverse_entries()
    }

    // Link Table
    fn get_link_entry(
        &self,
        link_id: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&leviculum_core::storage_types::LinkEntry> {
        self.inner.get_link_entry(link_id)
    }
    fn get_link_entry_mut(
        &mut self,
        link_id: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&mut leviculum_core::storage_types::LinkEntry> {
        self.inner.get_link_entry_mut(link_id)
    }
    fn set_link_entry(
        &mut self,
        link_id: [u8; TRUNCATED_HASHBYTES],
        entry: leviculum_core::storage_types::LinkEntry,
    ) {
        self.inner.set_link_entry(link_id, entry)
    }
    fn link_entries(
        &self,
    ) -> Vec<(
        [u8; TRUNCATED_HASHBYTES],
        leviculum_core::storage_types::LinkEntry,
    )> {
        self.inner.link_entries()
    }
    // Announce Table
    fn get_announce(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&leviculum_core::storage_types::AnnounceEntry> {
        self.inner.get_announce(dest_hash)
    }
    fn get_announce_mut(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&mut leviculum_core::storage_types::AnnounceEntry> {
        self.inner.get_announce_mut(dest_hash)
    }
    fn set_announce(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        entry: leviculum_core::storage_types::AnnounceEntry,
    ) {
        self.inner.set_announce(dest_hash, entry)
    }
    fn remove_announce(
        &mut self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<leviculum_core::storage_types::AnnounceEntry> {
        self.inner.remove_announce(dest_hash)
    }
    fn announce_keys(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.inner.announce_keys()
    }

    // Announce Cache
    fn get_announce_cache(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<&Vec<u8>> {
        self.inner.get_announce_cache(dest_hash)
    }
    fn set_announce_cache(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], raw: Vec<u8>) {
        self.inner.set_announce_cache(dest_hash, raw)
    }
    fn announce_cache_keys(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.inner.announce_cache_keys()
    }

    // Known-destination cache lifecycle (Codeberg #84)
    fn retain_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        self.inner.retain_known_dest(dest)
    }
    fn unretain_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES], now_ms: u64) -> bool {
        self.inner.unretain_known_dest(dest, now_ms)
    }
    fn used_known_dest(&mut self, dest: &[u8; TRUNCATED_HASHBYTES], now_ms: u64) -> bool {
        self.inner.used_known_dest(dest, now_ms)
    }
    fn is_known_dest_retained(&self, dest: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        self.inner.is_known_dest_retained(dest)
    }
    fn known_dest_last_used(&self, dest: &[u8; TRUNCATED_HASHBYTES]) -> Option<u64> {
        self.inner.known_dest_last_used(dest)
    }

    // Announce Rate
    fn get_announce_rate(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&leviculum_core::storage_types::AnnounceRateEntry> {
        self.inner.get_announce_rate(dest_hash)
    }
    fn set_announce_rate(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        entry: leviculum_core::storage_types::AnnounceRateEntry,
    ) {
        self.inner.set_announce_rate(dest_hash, entry)
    }

    // Receipts
    fn get_receipt(
        &self,
        hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&leviculum_core::storage_types::PacketReceipt> {
        self.inner.get_receipt(hash)
    }
    fn set_receipt(
        &mut self,
        hash: [u8; TRUNCATED_HASHBYTES],
        receipt: leviculum_core::storage_types::PacketReceipt,
    ) {
        self.inner.set_receipt(hash, receipt)
    }
    // Path Requests
    fn get_path_request_time(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<u64> {
        self.inner.get_path_request_time(dest_hash)
    }
    fn set_path_request_time(&mut self, dest_hash: [u8; TRUNCATED_HASHBYTES], time_ms: u64) {
        self.inner.set_path_request_time(dest_hash, time_ms)
    }
    fn check_path_request_tag(&mut self, tag: &[u8; 32]) -> bool {
        self.inner.check_path_request_tag(tag)
    }

    // Known Identities
    fn get_identity(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<&leviculum_core::Identity> {
        self.inner.get_identity(dest_hash)
    }
    fn set_identity(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        identity: leviculum_core::Identity,
    ) {
        let timestamp = unix_timestamp_secs();
        self.known_dest_entries
            .entry(dest_hash)
            .and_modify(|e| {
                e.timestamp = timestamp;
                e.public_key = identity.public_key_bytes();
            })
            .or_insert_with(|| KnownDestEntry {
                timestamp,
                packet_hash: vec![0u8; PACKET_HASH_LEN],
                public_key: identity.public_key_bytes(),
                app_data: None,
            });

        if self.known_dest_entries.len() > MAX_KNOWN_DESTINATIONS {
            if let Some((&oldest_key, _)) = self
                .known_dest_entries
                .iter()
                .min_by(|a, b| (a.1.timestamp as u64).cmp(&(b.1.timestamp as u64)))
            {
                self.known_dest_entries.remove(&oldest_key);
            }
        }

        self.inner.set_identity(dest_hash, identity);
        self.identities_dirty = true;
        self.identities_gen = self.identities_gen.wrapping_add(1);
    }

    // Cleanup
    fn expire_reverses(&mut self, now_ms: u64, timeout_ms: u64) -> usize {
        self.inner.expire_reverses(now_ms, timeout_ms)
    }
    fn remove_reverse_entries_for_interface(&mut self, iface_index: usize) {
        self.inner.remove_reverse_entries_for_interface(iface_index)
    }
    fn expire_receipts(
        &mut self,
        now_ms: u64,
    ) -> Vec<leviculum_core::storage_types::PacketReceipt> {
        self.inner.expire_receipts(now_ms)
    }
    fn expire_link_entries(
        &mut self,
        now_ms: u64,
        link_timeout_ms: u64,
    ) -> Vec<(
        [u8; TRUNCATED_HASHBYTES],
        leviculum_core::storage_types::LinkEntry,
    )> {
        self.inner.expire_link_entries(now_ms, link_timeout_ms)
    }
    fn clean_stale_path_metadata(&mut self) {
        self.inner.clean_stale_path_metadata()
    }
    fn clean_announce_cache(
        &mut self,
        local_destinations: &std::collections::BTreeSet<[u8; TRUNCATED_HASHBYTES]>,
    ) {
        self.inner.clean_announce_cache(local_destinations)
    }
    fn remove_link_entries_for_interface(
        &mut self,
        iface_index: usize,
    ) -> Vec<(
        [u8; TRUNCATED_HASHBYTES],
        leviculum_core::storage_types::LinkEntry,
    )> {
        self.inner.remove_link_entries_for_interface(iface_index)
    }
    fn remove_paths_for_interface(&mut self, iface_index: usize) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.inner.remove_paths_for_interface(iface_index)
    }

    // Deadlines
    fn earliest_receipt_deadline(&self) -> Option<u64> {
        self.inner.earliest_receipt_deadline()
    }
    fn earliest_link_deadline(&self, link_timeout_ms: u64) -> Option<u64> {
        self.inner.earliest_link_deadline(link_timeout_ms)
    }

    // Flush (persist to disk)
    //
    // The synchronous shutdown entry: all three phases run back-to-back on
    // the caller's thread. The event loop instead runs `FlushSnapshot::write`
    // on the blocking pool between the two lock-holding phases
    // (leviculum#44).
    fn flush(&mut self) {
        if let Some(snapshot) = self.take_flush_snapshot() {
            self.finish_flush(snapshot.write());
        }
    }

    // Diagnostics
    fn diagnostic_dump(&self) -> (String, u64) {
        use std::fmt::Write;
        let mut s = String::new();
        let mut total = 0u64;

        // packet_cache: HashSet<[u8; 32]>, 1.5x overhead
        let n = self.packet_cache.len();
        let raw = (n * 32) as u64;
        let est = raw * 3 / 2;
        total += est;
        let _ = writeln!(
            s,
            "packet_cache: {} entries, raw {} bytes, estimated {} bytes (HashSet 1.5x)",
            n, raw, est
        );

        // packet_cache_prev: HashSet<[u8; 32]>, 1.5x
        let n = self.packet_cache_prev.len();
        let raw = (n * 32) as u64;
        let est = raw * 3 / 2;
        total += est;
        let _ = writeln!(
            s,
            "packet_cache_prev: {} entries, raw {} bytes, estimated {} bytes (HashSet 1.5x)",
            n, raw, est
        );

        // Delegate remaining collections to inner MemoryStorage
        let (s2, total2) = self.inner.diagnostic_dump_non_packet_cache();
        s.push_str(&s2);
        total += total2;

        (s, total)
    }

    // Known Ratchets (write-through to disk)
    fn get_known_ratchet(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<[u8; leviculum_core::constants::RATCHET_SIZE]> {
        self.inner.get_known_ratchet(dest_hash)
    }
    fn remember_known_ratchet(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        ratchet: [u8; leviculum_core::constants::RATCHET_SIZE],
        received_at_ms: u64,
    ) {
        self.inner
            .remember_known_ratchet(dest_hash, ratchet, received_at_ms);

        // Write-through to disk via ratchet store
        let received_secs = mono_to_wallclock_secs(received_at_ms, self.mono_offset_ms);
        let entry = leviculum_core::ratchet_store::KnownRatchetEntry {
            ratchet,
            received_at_secs: received_secs,
        };
        if let Err(e) = self.ratchet_store.save_known_ratchet(&dest_hash, &entry) {
            tracing::warn!("Failed to persist known ratchet: {e}");
        }
    }
    fn expire_known_ratchets(&mut self, now_ms: u64, expiry_ms: u64) -> usize {
        // Collect hashes that will be expired (before memory expiry removes them)
        let expired_hashes: Vec<[u8; TRUNCATED_HASHBYTES]> = self
            .inner
            .known_ratchet_iter()
            .filter(|(_, (_, received_at))| now_ms.saturating_sub(*received_at) >= expiry_ms)
            .map(|(hash, _)| *hash)
            .collect();

        let count = self.inner.expire_known_ratchets(now_ms, expiry_ms);

        // Delete corresponding files from disk
        for hash in &expired_hashes {
            self.ratchet_store.delete_known_ratchet(hash);
        }

        count
    }

    // Local Client Destinations
    fn add_local_client_dest(
        &mut self,
        iface_id: usize,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
    ) -> bool {
        self.inner.add_local_client_dest(iface_id, dest_hash)
    }
    fn remove_local_client_dests(&mut self, iface_id: usize) {
        self.inner.remove_local_client_dests(iface_id)
    }
    // Local Client Known Destinations
    fn set_local_client_known_dest(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        last_seen_ms: u64,
    ) {
        self.inner
            .set_local_client_known_dest(dest_hash, last_seen_ms)
    }
    fn local_client_known_dest_hashes(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.inner.local_client_known_dest_hashes()
    }
    fn expire_local_client_known_dests(&mut self, now_ms: u64, expiry_ms: u64) -> usize {
        self.inner
            .expire_local_client_known_dests(now_ms, expiry_ms)
    }

    // Discovery Path Requests
    fn set_discovery_path_request(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        requesting_interface: usize,
        timeout_ms: u64,
    ) {
        self.inner
            .set_discovery_path_request(dest_hash, requesting_interface, timeout_ms);
    }

    fn get_discovery_path_request(
        &self,
        dest_hash: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<(usize, u64)> {
        self.inner.get_discovery_path_request(dest_hash)
    }

    fn remove_discovery_path_request(&mut self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) {
        self.inner.remove_discovery_path_request(dest_hash);
    }

    fn expire_discovery_path_requests(&mut self, now_ms: u64) -> usize {
        self.inner.expire_discovery_path_requests(now_ms)
    }

    fn discovery_path_request_dest_hashes(&self) -> Vec<[u8; TRUNCATED_HASHBYTES]> {
        self.inner.discovery_path_request_dest_hashes()
    }

    // Sender-Side Ratchet Keys (write-through to disk)
    fn store_dest_ratchet_keys(
        &mut self,
        dest_hash: [u8; TRUNCATED_HASHBYTES],
        serialized: Vec<u8>,
    ) {
        // Write-through to disk via ratchet store
        if let Err(e) = self
            .ratchet_store
            .save_dest_ratchet_keys(&dest_hash, &serialized)
        {
            tracing::warn!("Failed to persist ratchet keys: {e}");
        }
        self.inner.store_dest_ratchet_keys(dest_hash, serialized)
    }
    fn load_dest_ratchet_keys(&self, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Option<Vec<u8>> {
        self.inner.load_dest_ratchet_keys(dest_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    use crate::file_ratchet_store::{RATCHETKEYS_DIR, RATCHETS_DIR};
    use crate::known_destinations::{
        decode_known_destinations, encode_known_destinations, KNOWN_DESTINATIONS_FILE,
    };
    use crate::packet_hashlist::{encode_packet_hashlist, PACKET_HASHLIST_FILE};

    fn temp_storage() -> Storage {
        let path = temp_dir().join(format!("reticulum_test_{}", std::process::id()));
        Storage::new(&path).unwrap()
    }

    #[test]
    fn test_raw_storage() {
        let storage = temp_storage();

        storage.write_raw("test", "data.bin", b"hello").unwrap();
        let data = storage.read_raw("test", "data.bin").unwrap();
        assert_eq!(data, b"hello");

        // Cleanup
        storage.delete("test", "data.bin").unwrap();
    }

    #[test]
    fn test_serialized_storage() {
        let storage = temp_storage();

        let value = vec![1u32, 2, 3, 4, 5];
        storage.write("test", "numbers.mp", &value).unwrap();

        let loaded: Vec<u32> = storage.read("test", "numbers.mp").unwrap();
        assert_eq!(loaded, value);

        // Cleanup
        storage.delete("test", "numbers.mp").unwrap();
    }

    #[test]
    fn test_exists() {
        let storage = temp_storage();

        assert!(!storage.exists("test", "nonexistent"));

        storage.write_raw("test", "exists.bin", b"data").unwrap();
        assert!(storage.exists("test", "exists.bin"));

        // Cleanup
        storage.delete("test", "exists.bin").unwrap();
    }

    #[test]
    fn test_root_storage() {
        let storage = temp_storage();

        storage.write_root("root_data.bin", b"root_hello").unwrap();
        let data = storage.read_root("root_data.bin").unwrap();
        assert_eq!(data, b"root_hello");

        // Verify file is directly in base path, not in a subdirectory
        let expected_path = storage.base_path.join("root_data.bin");
        assert!(expected_path.exists());

        // Cleanup
        std::fs::remove_file(expected_path).unwrap();
    }

    #[test]
    fn test_read_root_missing_file() {
        let storage = temp_storage();
        let result = storage.read_root("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_runtime_storage_delegates_to_inner() {
        use leviculum_core::traits::Storage as CoreStorage;

        let mut storage = temp_storage();

        // Verify packet hash dedup works (was no-op before this commit)
        let hash = [0x42u8; 32];
        assert!(!CoreStorage::has_packet_hash(&storage, &hash));
        CoreStorage::add_packet_hash(&mut storage, hash);
        assert!(CoreStorage::has_packet_hash(&storage, &hash));

        // Verify path table works
        let dest = [0x01u8; TRUNCATED_HASHBYTES];
        assert!(CoreStorage::get_path(&storage, &dest).is_none());
        CoreStorage::set_path(
            &mut storage,
            dest,
            leviculum_core::storage_types::PathEntry {
                hops: 2,
                expires_ms: 5000,
                interface_index: 0,
                random_blobs: Vec::new(),
                next_hop: None,
                via_peer: None,
            },
        );
        assert_eq!(CoreStorage::get_path(&storage, &dest).unwrap().hops, 2);
        assert_eq!(CoreStorage::path_count(&storage), 1);

        // Verify identity storage works
        let id = Identity::generate(&mut rand_core::OsRng);
        let id_hash = [0x02u8; TRUNCATED_HASHBYTES];
        CoreStorage::set_identity(&mut storage, id_hash, id);
        assert!(CoreStorage::get_identity(&storage, &id_hash).is_some());
    }

    #[test]
    fn test_flush_persists_identities() {
        use leviculum_core::traits::Storage as CoreStorage;

        let path = temp_dir().join(format!("reticulum_test_flush_id_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        // Create storage, add identity, flush
        {
            let mut storage = Storage::new(&path).unwrap();
            let id = Identity::generate(&mut rand_core::OsRng);
            CoreStorage::set_identity(&mut storage, [0xAA; TRUNCATED_HASHBYTES], id);
            CoreStorage::flush(&mut storage);
        }

        // Re-open storage, identity should be loaded from disk
        {
            let storage = Storage::new(&path).unwrap();
            assert!(
                CoreStorage::get_identity(&storage, &[0xAA; TRUNCATED_HASHBYTES]).is_some(),
                "identity should persist across restarts"
            );
        }

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_flush_persists_packet_hashes() {
        use leviculum_core::traits::Storage as CoreStorage;

        let path = temp_dir().join(format!("reticulum_test_flush_hash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        let hash = [0xBB; 32];

        // Create storage, add hash, flush
        {
            let mut storage = Storage::new(&path).unwrap();
            CoreStorage::add_packet_hash(&mut storage, hash);
            CoreStorage::flush(&mut storage);
        }

        // Re-open storage, hash should be loaded from disk
        {
            let storage = Storage::new(&path).unwrap();
            assert!(
                CoreStorage::has_packet_hash(&storage, &hash),
                "packet hash should persist across restarts"
            );
        }

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_flush_merges_with_disk() {
        use leviculum_core::traits::Storage as CoreStorage;

        let path = temp_dir().join(format!("reticulum_test_flush_merge_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        // Create storage with identity A, flush
        let id_a = Identity::generate(&mut rand_core::OsRng);
        {
            let mut storage = Storage::new(&path).unwrap();
            CoreStorage::set_identity(&mut storage, [0x01; TRUNCATED_HASHBYTES], id_a);
            CoreStorage::flush(&mut storage);
        }

        // Create new storage, add identity B (not A), flush
        // A should be preserved from disk
        let id_b = Identity::generate(&mut rand_core::OsRng);
        {
            let mut storage = Storage::new(&path).unwrap();
            // A was loaded from disk
            assert!(CoreStorage::get_identity(&storage, &[0x01; TRUNCATED_HASHBYTES]).is_some());
            CoreStorage::set_identity(&mut storage, [0x02; TRUNCATED_HASHBYTES], id_b);
            CoreStorage::flush(&mut storage);
        }

        // Re-open: both A and B should be present
        {
            let storage = Storage::new(&path).unwrap();
            assert!(CoreStorage::get_identity(&storage, &[0x01; TRUNCATED_HASHBYTES]).is_some());
            assert!(CoreStorage::get_identity(&storage, &[0x02; TRUNCATED_HASHBYTES]).is_some());
        }

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_new_storage_loads_persistent_data() {
        let path = temp_dir().join(format!("reticulum_test_autoload_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        // Manually write a known_destinations file and packet_hashlist
        std::fs::create_dir_all(&path).unwrap();

        // Write a known_destinations file with one entry
        let id = Identity::generate(&mut rand_core::OsRng);
        let mut entries = BTreeMap::new();
        entries.insert(
            [0xCC; TRUNCATED_HASHBYTES],
            KnownDestEntry {
                timestamp: unix_timestamp_secs(),
                packet_hash: vec![0; 32],
                public_key: id.public_key_bytes(),
                app_data: None,
            },
        );
        let encoded = encode_known_destinations(&entries).unwrap();
        std::fs::write(path.join(KNOWN_DESTINATIONS_FILE), &encoded).unwrap();

        // Write a packet_hashlist with one hash
        let hash = [0xDD; 32];
        let hashes = [hash];
        let (encoded, _) = encode_packet_hashlist(hashes.iter()).unwrap();
        std::fs::write(path.join(PACKET_HASHLIST_FILE), &encoded).unwrap();

        // Create storage, should load both automatically
        let storage = Storage::new(&path).unwrap();

        use leviculum_core::traits::Storage as CoreStorage;
        assert!(
            CoreStorage::get_identity(&storage, &[0xCC; TRUNCATED_HASHBYTES]).is_some(),
            "should auto-load identity from known_destinations"
        );
        assert!(
            CoreStorage::has_packet_hash(&storage, &hash),
            "should auto-load packet hash from packet_hashlist"
        );

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_flush_updates_timestamp_on_existing_entry() {
        use leviculum_core::traits::Storage as CoreStorage;

        let path = temp_dir().join(format!("reticulum_test_flush_ts_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        let dest_hash = [0xEE; TRUNCATED_HASHBYTES];
        let id = Identity::generate(&mut rand_core::OsRng);
        let app_data = Some(b"test_app_data".to_vec());

        // Create storage, seed a known_dest_entry with old timestamp and app_data,
        // add the identity to runtime, flush
        {
            let mut storage = Storage::new(&path).unwrap();

            // Manually insert an old entry with app_data (simulates a previous flush
            // or disk load)
            storage.known_dest_entries.insert(
                dest_hash,
                KnownDestEntry {
                    timestamp: 1000.0,
                    packet_hash: vec![0xAB; PACKET_HASH_LEN],
                    public_key: id.public_key_bytes(),
                    app_data: app_data.clone(),
                },
            );

            // Add the same identity to runtime storage (simulates a fresh announce)
            CoreStorage::set_identity(&mut storage, dest_hash, id);

            // Flush, should update timestamp but preserve app_data and packet_hash
            CoreStorage::flush(&mut storage);
        }

        // Re-open and verify the timestamp was updated
        {
            let storage = Storage::new(&path).unwrap();
            let entry = storage.known_dest_entries.get(&dest_hash);
            assert!(entry.is_some(), "entry should exist after flush");
            let entry = entry.unwrap();

            // Timestamp should be much larger than the old value of 1000.0
            assert!(
                entry.timestamp > 1_000_000.0,
                "timestamp should be updated to current time, got {}",
                entry.timestamp
            );

            // app_data should be preserved (not overwritten with None)
            assert_eq!(
                entry.app_data, app_data,
                "app_data should be preserved across flush"
            );

            // packet_hash should be preserved (not overwritten with zeros)
            assert_eq!(
                entry.packet_hash,
                vec![0xAB; PACKET_HASH_LEN],
                "packet_hash should be preserved across flush"
            );
        }

        let _ = std::fs::remove_dir_all(&path);
    }

    // Three-phase flush (leviculum#44)

    #[test]
    fn test_take_flush_snapshot_defers_dirty_clear() {
        let path = temp_dir().join(format!("reticulum_test_snap_defer_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        let mut storage = Storage::new(&path).unwrap();
        let id = Identity::generate(&mut rand_core::OsRng);
        CoreStorage::set_identity(&mut storage, [0xA1; TRUNCATED_HASHBYTES], id);

        let snapshot = storage
            .take_flush_snapshot()
            .expect("dirty storage must snapshot");
        assert!(
            storage.identities_dirty,
            "the snapshot must not clear the dirty flag; only finish_flush may"
        );

        storage.finish_flush(snapshot.write());
        assert!(
            !storage.identities_dirty,
            "an undisturbed write must clear the dirty flag"
        );

        let bytes = std::fs::read(path.join(KNOWN_DESTINATIONS_FILE)).unwrap();
        let entries = decode_known_destinations(&bytes).unwrap();
        assert!(
            entries.contains_key(&[0xA1; TRUNCATED_HASHBYTES]),
            "the snapshotted identity must land on disk"
        );

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_dirtied_mid_write_stays_dirty() {
        let path = temp_dir().join(format!("reticulum_test_snap_gen_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        let mut storage = Storage::new(&path).unwrap();
        let id_a = Identity::generate(&mut rand_core::OsRng);
        CoreStorage::set_identity(&mut storage, [0x01; TRUNCATED_HASHBYTES], id_a);
        CoreStorage::add_packet_hash(&mut storage, [0x0A; 32]);

        let snapshot = storage.take_flush_snapshot().unwrap();

        // Dirtied between snapshot and finish: both generations bump, so the
        // finished write must not clear either flag.
        let id_b = Identity::generate(&mut rand_core::OsRng);
        CoreStorage::set_identity(&mut storage, [0x02; TRUNCATED_HASHBYTES], id_b);
        CoreStorage::add_packet_hash(&mut storage, [0x0B; 32]);

        storage.finish_flush(snapshot.write());
        assert!(
            storage.identities_dirty,
            "identity added mid-write must stay dirty for the next interval"
        );
        assert!(
            storage.packet_hashes_dirty,
            "packet hash added mid-write must stay dirty for the next interval"
        );

        // The next snapshot carries both the flushed and the mid-write data.
        let second = storage.take_flush_snapshot().unwrap();
        let (_, entries) = second.identities.as_ref().unwrap();
        assert!(entries.contains_key(&[0x01; TRUNCATED_HASHBYTES]));
        assert!(entries.contains_key(&[0x02; TRUNCATED_HASHBYTES]));
        let (_, hashes) = second.packet_hashes.as_ref().unwrap();
        assert!(hashes.contains(&[0x0A; 32]));
        assert!(hashes.contains(&[0x0B; 32]));

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_failed_write_leaves_dirty_for_retry() {
        let path = temp_dir().join(format!("reticulum_test_snap_retry_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        let mut storage = Storage::new(&path).unwrap();
        let id = Identity::generate(&mut rand_core::OsRng);
        CoreStorage::set_identity(&mut storage, [0x03; TRUNCATED_HASHBYTES], id);
        CoreStorage::add_packet_hash(&mut storage, [0x0C; 32]);

        let snapshot = storage.take_flush_snapshot().unwrap();
        // Yank the storage root: both saves fail, nothing is written.
        std::fs::remove_dir_all(&path).unwrap();
        storage.finish_flush(snapshot.write());
        assert!(
            storage.identities_dirty,
            "a failed identities write must leave the flag set"
        );
        assert!(
            storage.packet_hashes_dirty,
            "a failed packet-hash write must leave the flag set"
        );

        // Next interval retries and succeeds.
        std::fs::create_dir_all(&path).unwrap();
        let snapshot = storage.take_flush_snapshot().unwrap();
        storage.finish_flush(snapshot.write());
        assert!(!storage.identities_dirty);
        assert!(!storage.packet_hashes_dirty);
        assert!(path.join(KNOWN_DESTINATIONS_FILE).exists());
        assert!(path.join(PACKET_HASHLIST_FILE).exists());

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_take_flush_snapshot_none_when_clean() {
        let path = temp_dir().join(format!("reticulum_test_snap_clean_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        let mut storage = Storage::new(&path).unwrap();
        assert!(storage.take_flush_snapshot().is_none());

        // The trait-level shutdown flush stays a no-op on clean storage.
        CoreStorage::flush(&mut storage);
        assert!(!path.join(KNOWN_DESTINATIONS_FILE).exists());
        assert!(!path.join(PACKET_HASHLIST_FILE).exists());

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_fast_hasher_packet_cache_correctness() {
        use leviculum_core::traits::Storage as CoreStorage;
        let mut storage = temp_storage();

        // Verify storage packet_cache uses FastHashBuilder type
        let is_fast = std::any::type_name::<PacketHashSet>().contains("FastHashBuilder");
        assert!(is_fast, "packet_cache must use FastHashBuilder");

        // Insert 2,000 distinct pseudo-random hashes
        for i in 0..2000u32 {
            let mut h = [0u8; 32];
            h[0..4].copy_from_slice(&i.to_be_bytes());
            h[4..8].copy_from_slice(&(i ^ 0xDEADBEEF).to_be_bytes());
            assert!(!CoreStorage::has_packet_hash(&storage, &h));
            CoreStorage::add_packet_hash(&mut storage, h);
            assert!(CoreStorage::has_packet_hash(&storage, &h));
        }

        // Verify absent hash
        let mut absent = [0xFFu8; 32];
        absent[0..4].copy_from_slice(&99999u32.to_be_bytes());
        assert!(!CoreStorage::has_packet_hash(&storage, &absent));
    }

    #[test]
    fn test_hashset_packet_cache_has_add() {
        use leviculum_core::traits::Storage as CoreStorage;

        let mut storage = temp_storage();
        let hash = [0x42u8; 32];

        assert!(!CoreStorage::has_packet_hash(&storage, &hash));
        CoreStorage::add_packet_hash(&mut storage, hash);
        assert!(CoreStorage::has_packet_hash(&storage, &hash));

        // Inner MemoryStorage's packet_cache should stay empty
        assert_eq!(storage.inner.packet_hash_count(), 0);
    }

    #[test]
    fn test_hashset_packet_cache_rotation() {
        use leviculum_core::traits::Storage as CoreStorage;

        let path = temp_dir().join(format!("reticulum_test_rotation_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);

        let mut storage = Storage::new(&path).unwrap();
        // Override cap to a small value for testing
        storage.packet_hash_cap = 10;

        // Add 6 hashes (exceeds half of 10 = 5), triggers rotation
        for i in 0..6u8 {
            let mut hash = [0u8; 32];
            hash[0] = i;
            CoreStorage::add_packet_hash(&mut storage, hash);
        }

        // All 6 should still be findable (5 in prev, 1 in current after rotation)
        for i in 0..6u8 {
            let mut hash = [0u8; 32];
            hash[0] = i;
            assert!(
                CoreStorage::has_packet_hash(&storage, &hash),
                "hash {i} should be found after rotation"
            );
        }

        // Current should have 1 entry (the 6th hash, after rotation cleared current)
        // Wait, 6th hash triggers rotation, then gets inserted? No:
        // add_packet_hash inserts first, then checks. So after inserting 6th:
        // current has 6, exceeds 5, rotation happens: current(6) -> prev, current cleared.
        // So current=0, prev=6. The 6th hash is in prev.
        assert_eq!(storage.packet_cache.len(), 0);
        assert_eq!(storage.packet_cache_prev.len(), 6);

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_hashset_packet_cache_persistence() {
        use leviculum_core::traits::Storage as CoreStorage;

        let path = temp_dir().join(format!(
            "reticulum_test_hash_persist_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);

        let hash_a = [0xAA; 32];
        let hash_b = [0xBB; 32];

        // Create, add hashes to both generations, flush
        {
            let mut storage = Storage::new(&path).unwrap();
            storage.packet_hash_cap = 4; // cap/2 = 2

            CoreStorage::add_packet_hash(&mut storage, hash_a);
            CoreStorage::add_packet_hash(&mut storage, hash_b);
            // After 2 inserts: current has 2, but cap/2=2, so 2 > 2 is false, no rotation yet
            // Actually: len() > cap/2 means 2 > 2 which is false, no rotation
            // Add a third to trigger rotation
            let hash_c = [0xCC; 32];
            CoreStorage::add_packet_hash(&mut storage, hash_c);
            // Now current had 3, 3 > 2, rotation: prev=[A,B,C], current=[]

            CoreStorage::flush(&mut storage);
        }

        // Re-open, all hashes should be loaded
        {
            let storage = Storage::new(&path).unwrap();
            assert!(
                CoreStorage::has_packet_hash(&storage, &hash_a),
                "hash_a should persist"
            );
            assert!(
                CoreStorage::has_packet_hash(&storage, &hash_b),
                "hash_b should persist"
            );
        }

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_hashset_diagnostic_dump() {
        use leviculum_core::traits::Storage as CoreStorage;

        let mut storage = temp_storage();
        CoreStorage::add_packet_hash(&mut storage, [0x01; 32]);
        CoreStorage::add_packet_hash(&mut storage, [0x02; 32]);

        let (dump, total) = CoreStorage::diagnostic_dump(&storage);
        assert!(
            dump.contains("HashSet 1.5x"),
            "should report HashSet overhead"
        );
        assert!(
            dump.contains("packet_cache: 2 entries"),
            "should show 2 entries in packet_cache"
        );
        assert!(total > 0);
    }

    // Ratchet Persistence Tests
    /// Create a temp storage with a unique per-test directory.
    fn unique_temp_storage(suffix: &str) -> (Storage, PathBuf) {
        let path = temp_dir().join(format!("reticulum_ratchet_{}_{suffix}", std::process::id()));
        // Clean up any previous run
        let _ = std::fs::remove_dir_all(&path);
        let storage = Storage::new(&path).unwrap();
        (storage, path)
    }

    #[test]
    fn test_known_ratchet_persists_to_disk() {
        let (mut storage, path) = unique_temp_storage("kr_persist");

        let hash = [0x11; TRUNCATED_HASHBYTES];
        let ratchet = [0xaa; 32];
        CoreStorage::remember_known_ratchet(&mut storage, hash, ratchet, 5000);

        // Verify file exists
        let hex = hex_encode(&hash);
        let file_path = path.join(RATCHETS_DIR).join(&hex);
        assert!(file_path.exists(), "Ratchet file should exist on disk");

        // Drop and recreate from same directory
        drop(storage);
        let storage2 = Storage::new(&path).unwrap();
        let loaded = CoreStorage::get_known_ratchet(&storage2, &hash);
        assert_eq!(loaded, Some(ratchet), "Ratchet should survive restart");

        // Cleanup
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_dest_ratchet_keys_persist_to_disk() {
        let (mut storage, path) = unique_temp_storage("drk_persist");

        let hash = [0x22; TRUNCATED_HASHBYTES];
        let data = vec![0x82, 0xa9]; // fake signed msgpack header
        CoreStorage::store_dest_ratchet_keys(&mut storage, hash, data.clone());

        // Verify file exists
        let hex = hex_encode(&hash);
        let file_path = path.join(RATCHETKEYS_DIR).join(&hex);
        assert!(file_path.exists(), "Ratchet keys file should exist on disk");

        // Drop and recreate from same directory
        drop(storage);
        let storage2 = Storage::new(&path).unwrap();
        let loaded = CoreStorage::load_dest_ratchet_keys(&storage2, &hash);
        assert_eq!(loaded, Some(data), "Ratchet keys should survive restart");

        // Cleanup
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_known_ratchet_expire_deletes_file() {
        let (mut storage, path) = unique_temp_storage("kr_expire");

        let h1 = [0x01; TRUNCATED_HASHBYTES];
        let h2 = [0x02; TRUNCATED_HASHBYTES];
        let ratchet = [0xbb; 32];

        CoreStorage::remember_known_ratchet(&mut storage, h1, ratchet, 1000);
        CoreStorage::remember_known_ratchet(&mut storage, h2, ratchet, 5000);

        // Both files should exist
        let hex1 = hex_encode(&h1);
        let hex2 = hex_encode(&h2);
        assert!(path.join(RATCHETS_DIR).join(&hex1).exists());
        assert!(path.join(RATCHETS_DIR).join(&hex2).exists());

        // Expire: h1 is 4000ms old at now=5000, threshold=3000
        let removed = CoreStorage::expire_known_ratchets(&mut storage, 5000, 3000);
        assert_eq!(removed, 1);

        // h1 file should be deleted, h2 should remain
        assert!(
            !path.join(RATCHETS_DIR).join(&hex1).exists(),
            "Expired ratchet file should be deleted"
        );
        assert!(
            path.join(RATCHETS_DIR).join(&hex2).exists(),
            "Non-expired ratchet file should remain"
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_corrupted_ratchet_file_skipped() {
        let (_, path) = unique_temp_storage("kr_corrupt");

        // Write a corrupted file manually
        let ratchets_dir = path.join(RATCHETS_DIR);
        std::fs::create_dir_all(&ratchets_dir).unwrap();
        let hex = hex_encode(&[0x33; TRUNCATED_HASHBYTES]);
        std::fs::write(ratchets_dir.join(&hex), b"garbage data").unwrap();

        // Recreate storage, should load without panic, skip corrupted file
        let storage = Storage::new(&path).unwrap();
        let loaded = CoreStorage::get_known_ratchet(&storage, &[0x33; TRUNCATED_HASHBYTES]);
        assert!(loaded.is_none(), "Corrupted ratchet should not be loaded");

        // Corrupted file should be deleted
        assert!(
            !ratchets_dir.join(&hex).exists(),
            "Corrupted file should be deleted"
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn test_wallclock_conversion_roundtrip() {
        let offset = 1_700_000_000_000u64; // ~2023 in ms
        let mono = 5_000u64; // 5 seconds into process

        let secs = mono_to_wallclock_secs(mono, offset);
        let back = wallclock_secs_to_mono_ms(secs, offset);

        // Should round-trip (within f64 precision)
        assert_eq!(back, mono);
    }
}
