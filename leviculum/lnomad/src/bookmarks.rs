//! Persisted page bookmarks.
//!
//! A [`Bookmark`] is a saved page: the URL to reopen it and the page title to
//! label it. [`Bookmarks`] is the ordered set of them, keyed by URL (a URL is
//! bookmarked at most once), with a small add/remove/toggle/contains surface and
//! TOML persistence.
//!
//! The store lives at `${XDG_CONFIG_HOME:-~/.config}/lnomad/bookmarks.toml` (see
//! [`default_path`]). Loading tolerates a missing or corrupt file by starting
//! empty, so a first run or a hand-mangled file never crashes the browser;
//! saving creates the parent directories as needed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A saved page: the URL to reopen it and the title it was saved under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    /// The full page URL (`<dest_hex>:<path>`), enough to reopen without a
    /// current destination.
    pub url: String,
    /// The page title captured when it was bookmarked, for a readable label.
    pub title: String,
}

/// An ordered set of [`Bookmark`]s, unique by URL.
///
/// The list preserves insertion order so the places panel numbers them stably.
/// Serialised as a TOML array of tables under the `bookmark` key:
///
/// ```toml
/// [[bookmark]]
/// url = "<dest_hex>:/page/index.mu"
/// title = "Home"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bookmarks {
    /// The bookmarks in insertion order. `#[serde(default)]` so a file with no
    /// `[[bookmark]]` tables (or an empty file) loads as an empty set.
    #[serde(default, rename = "bookmark")]
    items: Vec<Bookmark>,
}

impl Bookmarks {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bookmarks in insertion order.
    pub fn list(&self) -> &[Bookmark] {
        &self.items
    }

    /// The number of bookmarks.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no bookmarks.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether `url` is bookmarked.
    pub fn contains(&self, url: &str) -> bool {
        self.items.iter().any(|b| b.url == url)
    }

    /// Add `bookmark`, unless its URL is already bookmarked (a no-op then, so
    /// the earlier title is kept). Returns `true` when it was newly added.
    pub fn add(&mut self, bookmark: Bookmark) -> bool {
        if self.contains(&bookmark.url) {
            return false;
        }
        self.items.push(bookmark);
        true
    }

    /// Insert `bookmark` at `index` (clamped to the list length), unless its
    /// URL is already bookmarked (a no-op then, so the existing entry is kept).
    /// Returns `true` when it was inserted. Restores an undone deletion to its
    /// original position.
    pub fn insert(&mut self, index: usize, bookmark: Bookmark) -> bool {
        if self.contains(&bookmark.url) {
            return false;
        }
        let at = index.min(self.items.len());
        self.items.insert(at, bookmark);
        true
    }

    /// Remove the bookmark with `url`, if present. Returns `true` when one was
    /// removed.
    pub fn remove(&mut self, url: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|b| b.url != url);
        self.items.len() != before
    }

    /// Toggle `bookmark`: remove it when its URL is already bookmarked, else
    /// add it. Returns `true` when the URL is bookmarked afterwards.
    pub fn toggle(&mut self, bookmark: Bookmark) -> bool {
        if self.contains(&bookmark.url) {
            self.remove(&bookmark.url);
            false
        } else {
            self.items.push(bookmark);
            true
        }
    }

    /// Load the store from `path`, tolerating a missing or corrupt file by
    /// returning an empty set (never an error, so startup cannot fail on it).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Write the store to `path` as TOML, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

/// The default bookmarks-store path: `${XDG_CONFIG_HOME:-~/.config}/lnomad/
/// bookmarks.toml`. `None` when neither `XDG_CONFIG_HOME` nor `HOME` is set, in
/// which case the browser runs without persistence.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lnomad").join("bookmarks.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(url: &str, title: &str) -> Bookmark {
        Bookmark {
            url: url.to_string(),
            title: title.to_string(),
        }
    }

    #[test]
    fn add_remove_toggle_contains() {
        let mut b = Bookmarks::new();
        assert!(!b.contains("u1"));
        assert!(b.add(bm("u1", "One")));
        assert!(b.contains("u1"));
        assert_eq!(b.len(), 1);
        // A second add of the same URL is a no-op, keeping the first entry.
        assert!(!b.add(bm("u1", "Renamed")));
        assert_eq!(b.len(), 1);
        assert_eq!(b.list()[0].title, "One");

        // Toggle removes an existing URL (now unbookmarked) ...
        assert!(!b.toggle(bm("u1", "One")));
        assert!(!b.contains("u1"));
        // ... and adds a missing one (now bookmarked).
        assert!(b.toggle(bm("u2", "Two")));
        assert!(b.contains("u2"));

        assert!(b.remove("u2"));
        assert!(!b.remove("u2"));
        assert!(b.is_empty());
    }

    #[test]
    fn insert_restores_in_place_clamps_and_keeps_urls_unique() {
        let mut b = Bookmarks::new();
        b.add(bm("u1", "One"));
        b.add(bm("u3", "Three"));
        // Insert in the middle lands exactly there.
        assert!(b.insert(1, bm("u2", "Two")));
        let urls: Vec<&str> = b.list().iter().map(|x| x.url.as_str()).collect();
        assert_eq!(urls, ["u1", "u2", "u3"]);
        // An out-of-range index clamps to an append.
        assert!(b.insert(99, bm("u4", "Four")));
        assert_eq!(b.list()[3].url, "u4");
        // An already-bookmarked URL is a no-op keeping the existing entry.
        assert!(!b.insert(0, bm("u2", "Renamed")));
        assert_eq!(b.len(), 4);
        assert_eq!(b.list()[1].title, "Two");
    }

    #[test]
    fn toml_round_trips_through_a_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/bookmarks.toml");

        let mut b = Bookmarks::new();
        b.add(bm("aa11:/page/index.mu", "Home"));
        b.add(bm("bb22:/page/about.mu", "About"));
        b.save(&path).unwrap();

        // Parent directories are created and the file is reloadable verbatim.
        let reloaded = Bookmarks::load(&path);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.list()[0].url, "aa11:/page/index.mu");
        assert_eq!(reloaded.list()[0].title, "Home");
        assert_eq!(reloaded.list()[1].title, "About");
    }

    #[test]
    fn missing_and_corrupt_files_load_empty() {
        let dir = tempfile::tempdir().unwrap();
        // A path that does not exist.
        assert!(Bookmarks::load(&dir.path().join("absent.toml")).is_empty());
        // A file that is not valid TOML for this schema.
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "this is = not [valid").unwrap();
        assert!(Bookmarks::load(&bad).is_empty());
    }
}
