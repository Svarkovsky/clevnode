//! An in-RAM LRU cache of recently viewed pages.
//!
//! Revisiting a page (following a link back to it, or stepping through the
//! browsing history) is common and, over the mesh, slow: a fetch crosses the
//! network and re-parses the micron source. The [`PageCache`] keeps the parsed
//! [`MicronDocument`] of the last [`PageCache::DEFAULT_CAPACITY`] distinct pages
//! keyed by their fetch [`Target`], so a revisit renders instantly from memory
//! with the reader's last scroll position restored.
//!
//! The cache stores the PARSED document, not the laid-out page: layout depends on
//! the terminal width and the active theme, both of which can change between
//! visits, so the caller re-lays the cached document out at the current width on
//! show. A reload always bypasses the cache and overwrites the entry; form
//! submits (non-idempotent requests) are never cached by the caller.

use crate::url::Target;
use leviculum_micron::MicronDocument;

/// A single cached page: everything needed to redisplay it without a fetch.
#[derive(Clone, Debug)]
pub struct CacheEntry {
    /// The parsed document, re-laid-out at the current width/theme on show.
    pub doc: MicronDocument,
    /// The page title shown in the top-bar.
    pub title: String,
    /// The scroll offset the reader was last at, restored on revisit.
    pub scroll: usize,
}

/// A bounded, least-recently-used cache of parsed pages keyed by fetch target.
///
/// Entries are held in a `Vec` ordered least-recently-used first (index 0) to
/// most-recently-used last. [`get`](PageCache::get) and [`insert`](PageCache::insert)
/// move the touched entry to the back; an insert over capacity evicts the front
/// (the least-recently-used) entry. The capacity is small (50), so the linear
/// scan is cheaper than a hash map plus an intrusive order list.
#[derive(Clone, Debug)]
pub struct PageCache {
    capacity: usize,
    /// `(target, entry)` pairs, LRU-first (index 0) to MRU-last.
    entries: Vec<(Target, CacheEntry)>,
}

impl PageCache {
    /// The default page-cache capacity: the last 50 distinct pages.
    pub const DEFAULT_CAPACITY: usize = 50;

    /// A cache holding up to `capacity` pages (at least one).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Vec::new(),
        }
    }

    /// The number of cached pages.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds no pages.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `target` is cached, without touching its recency.
    pub fn contains(&self, target: &Target) -> bool {
        self.entries.iter().any(|(t, _)| t == target)
    }

    /// Fetch the entry for `target`, marking it most-recently-used. Returns `None`
    /// on a miss.
    pub fn get(&mut self, target: &Target) -> Option<&CacheEntry> {
        let pos = self.entries.iter().position(|(t, _)| t == target)?;
        let hit = self.entries.remove(pos);
        self.entries.push(hit);
        self.entries.last().map(|(_, e)| e)
    }

    /// Insert (or overwrite) the entry for `target`, marking it most-recently-used.
    /// Evicts the least-recently-used entry when this pushes past the capacity.
    pub fn insert(&mut self, target: Target, entry: CacheEntry) {
        if let Some(pos) = self.entries.iter().position(|(t, _)| *t == target) {
            self.entries.remove(pos);
        }
        self.entries.push((target, entry));
        while self.entries.len() > self.capacity {
            self.entries.remove(0);
        }
    }

    /// Update the stored scroll offset of `target` without changing its recency.
    /// A no-op when `target` is not cached. Called when navigating away from a
    /// page so a later revisit restores where the reader was.
    pub fn set_scroll(&mut self, target: &Target, scroll: usize) {
        if let Some((_, entry)) = self.entries.iter_mut().find(|(t, _)| t == target) {
            entry.scroll = scroll;
        }
    }

    /// The cached targets in LRU-first to MRU-last order. For tests.
    #[cfg(test)]
    pub fn lru_order(&self) -> Vec<Target> {
        self.entries.iter().map(|(t, _)| t.clone()).collect()
    }
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_micron::parse;

    fn tgt(n: u8) -> Target {
        Target {
            dest_hash: [n; 16],
            path: format!("/page/{n}.mu"),
            fields: Vec::new(),
            is_file: false,
        }
    }

    fn entry(body: &str, scroll: usize) -> CacheEntry {
        CacheEntry {
            doc: parse(body),
            title: body.to_string(),
            scroll,
        }
    }

    #[test]
    fn insert_then_get_returns_the_entry() {
        let mut c = PageCache::new(50);
        assert!(c.is_empty());
        c.insert(tgt(1), entry("one", 3));
        assert_eq!(c.len(), 1);
        assert!(c.contains(&tgt(1)));
        let hit = c.get(&tgt(1)).expect("a hit");
        assert_eq!(hit.title, "one");
        assert_eq!(hit.scroll, 3);
        assert!(c.get(&tgt(2)).is_none(), "a miss returns None");
    }

    #[test]
    fn default_capacity_is_fifty() {
        assert_eq!(PageCache::default().capacity, PageCache::DEFAULT_CAPACITY);
        assert_eq!(PageCache::DEFAULT_CAPACITY, 50);
    }

    #[test]
    fn get_moves_the_entry_to_most_recently_used() {
        let mut c = PageCache::new(50);
        c.insert(tgt(1), entry("one", 0));
        c.insert(tgt(2), entry("two", 0));
        c.insert(tgt(3), entry("three", 0));
        // Order is now [1, 2, 3] (LRU-first). Touching 1 moves it to the back.
        assert_eq!(c.lru_order(), vec![tgt(1), tgt(2), tgt(3)]);
        c.get(&tgt(1));
        assert_eq!(c.lru_order(), vec![tgt(2), tgt(3), tgt(1)]);
    }

    #[test]
    fn insert_overwrites_and_marks_most_recently_used() {
        let mut c = PageCache::new(50);
        c.insert(tgt(1), entry("one", 0));
        c.insert(tgt(2), entry("two", 0));
        // Overwrite 1 with a fresh body: length unchanged, moved to the back.
        c.insert(tgt(1), entry("one-fresh", 7));
        assert_eq!(c.len(), 2);
        assert_eq!(c.lru_order(), vec![tgt(2), tgt(1)]);
        let hit = c.get(&tgt(1)).expect("a hit");
        assert_eq!(hit.title, "one-fresh");
        assert_eq!(hit.scroll, 7);
    }

    #[test]
    fn eviction_drops_the_least_recently_used_at_capacity() {
        let mut c = PageCache::new(50);
        for i in 0..50u8 {
            c.insert(tgt(i), entry("body", 0));
        }
        assert_eq!(c.len(), 50);
        // Touch the oldest (0) so it is no longer the LRU victim.
        c.get(&tgt(0));
        // The 51st insert evicts the current LRU (now target 1), keeping 0.
        c.insert(tgt(200), entry("body", 0));
        assert_eq!(c.len(), 50);
        assert!(!c.contains(&tgt(1)), "the LRU entry was evicted");
        assert!(c.contains(&tgt(0)), "the recently-touched entry survived");
        assert!(c.contains(&tgt(200)), "the new entry is present");
    }

    #[test]
    fn set_scroll_updates_without_changing_recency() {
        let mut c = PageCache::new(50);
        c.insert(tgt(1), entry("one", 0));
        c.insert(tgt(2), entry("two", 0));
        // Stashing 1's scroll must not move it ahead of 2 in recency.
        c.set_scroll(&tgt(1), 42);
        assert_eq!(c.lru_order(), vec![tgt(1), tgt(2)]);
        assert_eq!(c.get(&tgt(1)).expect("a hit").scroll, 42);
        // A no-op for an absent target.
        c.set_scroll(&tgt(9), 5);
        assert!(!c.contains(&tgt(9)));
    }
}
