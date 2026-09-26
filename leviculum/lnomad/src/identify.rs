//! Per-node identify decisions.
//!
//! [`IdentifyStore`] is the set of destination hashes lnomad reveals its own
//! identity to when fetching (the NomadNet `identify_on_connect` equivalent).
//! Anonymous is the default: a destination not in the set is fetched with no
//! identify at all.
//!
//! The store lives at `${XDG_CONFIG_HOME:-~/.config}/lnomad/identify.toml` (see
//! [`default_path`]), serialised as a plain array of lowercase-hex destination
//! hashes:
//!
//! ```toml
//! dest = ["aa11...", "bb22..."]
//! ```
//!
//! Like the bookmarks store, loading tolerates a missing or corrupt file by
//! starting empty, so a first run or a hand-mangled file never crashes the
//! browser; saving creates the parent directories as needed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The set of destinations lnomad identifies to, keyed by lowercase-hex
/// destination hash. Plain membership only: no trust levels, no display names.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentifyStore {
    /// The destination hashes, sorted for a stable file. `#[serde(default)]` so
    /// an empty file loads as an empty set.
    #[serde(default, rename = "dest")]
    dests: BTreeSet<String>,
    /// Where [`save`](Self::save) writes. `None` for an in-memory store (no
    /// persistence), e.g. when no config dir can be resolved.
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl IdentifyStore {
    /// An empty in-memory store; [`save`](Self::save) is a no-op on it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the store from `path`, tolerating a missing or corrupt file by
    /// starting empty (never an error, so startup cannot fail on it). The path
    /// is remembered for [`save`](Self::save).
    pub fn load(path: &Path) -> Self {
        let mut store: Self = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        store.path = Some(path.to_path_buf());
        store
    }

    /// Write the store to its load path as TOML, creating parent directories as
    /// needed. A store without a path (in-memory) saves nowhere, successfully.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    /// Whether lnomad identifies to `dest`.
    pub fn contains(&self, dest: &[u8; 16]) -> bool {
        self.dests.contains(&crate::identity::fingerprint_hex(dest))
    }

    /// Add (`on = true`) or remove (`on = false`) `dest`. Returns `true` when
    /// membership actually changed. Does not persist; call
    /// [`save`](Self::save) for that.
    pub fn set(&mut self, dest: &[u8; 16], on: bool) -> bool {
        let key = crate::identity::fingerprint_hex(dest);
        if on {
            self.dests.insert(key)
        } else {
            self.dests.remove(&key)
        }
    }

    /// The number of destinations in the set.
    pub fn len(&self) -> usize {
        self.dests.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.dests.is_empty()
    }
}

/// The default store path: `${XDG_CONFIG_HOME:-~/.config}/lnomad/
/// identify.toml`. `None` when no config dir can be resolved, in which case
/// identify decisions do not persist across runs.
pub fn default_path() -> Option<PathBuf> {
    Some(crate::identity::app_config_dir()?.join("identify.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_contains_unset() {
        let mut store = IdentifyStore::new();
        let a = [0x11u8; 16];
        let b = [0x22u8; 16];
        assert!(!store.contains(&a));

        assert!(store.set(&a, true));
        assert!(store.contains(&a));
        assert!(!store.contains(&b));
        // Setting an existing member again is a no-op.
        assert!(!store.set(&a, true));
        assert_eq!(store.len(), 1);

        assert!(store.set(&a, false));
        assert!(!store.contains(&a));
        assert!(!store.set(&a, false));
        assert!(store.is_empty());
    }

    #[test]
    fn membership_round_trips_through_a_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/identify.toml");

        let mut store = IdentifyStore::load(&path);
        assert!(store.is_empty(), "missing file loads empty");
        let a = [0xAAu8; 16];
        let b = [0xBBu8; 16];
        store.set(&a, true);
        store.set(&b, true);
        store.set(&b, false);
        store.save().unwrap();

        let reloaded = IdentifyStore::load(&path);
        assert!(reloaded.contains(&a));
        assert!(!reloaded.contains(&b));
        assert_eq!(reloaded.len(), 1);
    }

    #[test]
    fn corrupt_file_loads_empty_and_is_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identify.toml");
        std::fs::write(&path, "this is = not [valid").unwrap();

        let mut store = IdentifyStore::load(&path);
        assert!(store.is_empty());

        // The store stays usable: a save replaces the corrupt file.
        store.set(&[0x33u8; 16], true);
        store.save().unwrap();
        assert!(IdentifyStore::load(&path).contains(&[0x33u8; 16]));
    }

    #[test]
    fn in_memory_store_saves_nowhere() {
        let mut store = IdentifyStore::new();
        store.set(&[0x44u8; 16], true);
        store.save().unwrap();
    }
}
