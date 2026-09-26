//! An in-RAM, byte-bounded LRU cache of fetched pictures.
//!
//! Over the mesh a picture is expensive in a way a page is not: a 20 kB
//! thumbnail is on the order of a minute of airtime at SF7 and ten or more at
//! SF12. Stepping back to a page the reader has already seen, or following a
//! link to a page that shares an illustration, must not pay that twice.
//!
//! So fetched payloads are kept, keyed by the fetch [`Target`] they came from,
//! until the cache is over its byte budget; then the least-recently-used
//! entries are dropped, one at a time, until it is back under. Bounding by
//! bytes rather than by count is the point: a cache of "the last fifty
//! pictures" says nothing about how much memory a browser is holding, and
//! pictures differ in size by three orders of magnitude.
//!
//! The order discipline mirrors [`crate::page_cache::PageCache`]: entries live
//! in a `Vec` ordered least-recently-used first, and both a hit and an insert
//! move the touched entry to the back. Memory only, never disk — a browser
//! writing every picture a stranger's node served into a cache directory is a
//! decision for the reader to make, not one to make on their behalf.

use crate::url::Target;

/// The default budget: 10 MB, in the decimal sense the option is spelled in.
pub const DEFAULT_MAX_BYTES: u64 = 10_000_000;

/// A bounded, least-recently-used cache of image payloads keyed by fetch
/// target.
#[derive(Clone, Debug)]
pub struct ImageCache {
    max_bytes: u64,
    /// `(target, bytes)` pairs, LRU-first (index 0) to MRU-last.
    entries: Vec<(Target, Vec<u8>)>,
    /// The sum of the held payload sizes, maintained on every change so the
    /// budget check never walks the list.
    used: u64,
}

impl ImageCache {
    /// A cache holding at most `max_bytes` of payload. Zero disables it: every
    /// insert is dropped and every lookup misses, which is what
    /// `--image-cache 0` asks for.
    pub fn new(max_bytes: u64) -> Self {
        ImageCache {
            max_bytes,
            entries: Vec::new(),
            used: 0,
        }
    }

    /// The configured budget in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The bytes currently held.
    pub fn used_bytes(&self) -> u64 {
        self.used
    }

    /// The number of pictures held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `target` is held, without touching its recency.
    pub fn contains(&self, target: &Target) -> bool {
        self.entries.iter().any(|(t, _)| t == target)
    }

    /// The payload for `target`, marking it most-recently-used.
    pub fn get(&mut self, target: &Target) -> Option<&[u8]> {
        let pos = self.entries.iter().position(|(t, _)| t == target)?;
        let hit = self.entries.remove(pos);
        self.entries.push(hit);
        self.entries.last().map(|(_, bytes)| bytes.as_slice())
    }

    /// Hold `bytes` for `target`, marking it most-recently-used and evicting
    /// the least-recently-used entries until the budget is met again.
    ///
    /// A payload larger than the whole budget is not stored: keeping it would
    /// mean evicting everything else and then, on the next insert, itself.
    /// Returns whether it was stored.
    pub fn insert(&mut self, target: Target, bytes: Vec<u8>) -> bool {
        self.remove(&target);
        let len = bytes.len() as u64;
        if len > self.max_bytes {
            return false;
        }
        self.used += len;
        self.entries.push((target, bytes));
        self.evict_to_budget();
        true
    }

    /// Drop `target`, if it is held.
    pub fn remove(&mut self, target: &Target) {
        if let Some(pos) = self.entries.iter().position(|(t, _)| t == target) {
            let (_, bytes) = self.entries.remove(pos);
            self.used -= bytes.len() as u64;
        }
    }

    /// Drop everything.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.used = 0;
    }

    /// Change the budget, evicting down to it at once.
    pub fn set_max_bytes(&mut self, max_bytes: u64) {
        self.max_bytes = max_bytes;
        self.evict_to_budget();
    }

    /// Drop least-recently-used entries until the held bytes fit the budget.
    fn evict_to_budget(&mut self) {
        while self.used > self.max_bytes && !self.entries.is_empty() {
            let (_, bytes) = self.entries.remove(0);
            self.used -= bytes.len() as u64;
        }
    }
}

impl Default for ImageCache {
    fn default() -> Self {
        ImageCache::new(DEFAULT_MAX_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(n: u8, name: &str) -> Target {
        Target {
            dest_hash: [n; 16],
            path: format!("/file/{name}"),
            fields: Vec::new(),
            is_file: true,
        }
    }

    fn bytes(n: usize) -> Vec<u8> {
        vec![0u8; n]
    }

    #[test]
    fn a_stored_picture_comes_back() {
        let mut cache = ImageCache::new(1000);
        assert!(cache.insert(target(1, "a.png"), bytes(100)));
        assert_eq!(cache.get(&target(1, "a.png")).map(<[u8]>::len), Some(100));
        assert_eq!(cache.used_bytes(), 100);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_different_node_serving_the_same_name_is_a_different_picture() {
        let mut cache = ImageCache::new(1000);
        cache.insert(target(1, "a.png"), bytes(10));
        assert!(cache.get(&target(2, "a.png")).is_none());
    }

    #[test]
    fn the_oldest_pictures_go_first_when_the_budget_is_exceeded() {
        let mut cache = ImageCache::new(1000);
        cache.insert(target(1, "a.png"), bytes(400));
        cache.insert(target(1, "b.png"), bytes(400));
        // 800 held; this one pushes to 1100, so the oldest has to go.
        cache.insert(target(1, "c.png"), bytes(300));

        assert!(
            !cache.contains(&target(1, "a.png")),
            "the oldest was dropped"
        );
        assert!(cache.contains(&target(1, "b.png")));
        assert!(cache.contains(&target(1, "c.png")));
        assert_eq!(cache.used_bytes(), 700);
        assert!(cache.used_bytes() <= cache.max_bytes());
    }

    #[test]
    fn eviction_keeps_going_until_it_fits() {
        let mut cache = ImageCache::new(1000);
        for i in 0..5u8 {
            cache.insert(target(1, &format!("{i}.png")), bytes(150));
        }
        assert_eq!(cache.used_bytes(), 750);

        // 900 on top of 750 leaves 1650: eviction has to keep going until it
        // fits, which here means all five, not just the first one.
        cache.insert(target(1, "big.png"), bytes(900));
        assert_eq!(cache.len(), 1);
        for i in 0..5u8 {
            assert!(
                !cache.contains(&target(1, &format!("{i}.png"))),
                "{i}.png should have been evicted"
            );
        }
        assert_eq!(cache.used_bytes(), 900);
        assert!(cache.used_bytes() <= cache.max_bytes());
    }

    #[test]
    fn looking_at_a_picture_again_makes_it_young() {
        let mut cache = ImageCache::new(1000);
        cache.insert(target(1, "a.png"), bytes(400));
        cache.insert(target(1, "b.png"), bytes(400));
        // Touching `a` makes `b` the oldest, so `b` is what the next insert
        // displaces.
        assert!(cache.get(&target(1, "a.png")).is_some());
        cache.insert(target(1, "c.png"), bytes(300));

        assert!(cache.contains(&target(1, "a.png")));
        assert!(!cache.contains(&target(1, "b.png")));
    }

    #[test]
    fn re_storing_a_picture_does_not_double_count_it() {
        let mut cache = ImageCache::new(1000);
        cache.insert(target(1, "a.png"), bytes(200));
        cache.insert(target(1, "a.png"), bytes(300));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 300);
    }

    #[test]
    fn a_picture_larger_than_the_whole_budget_is_not_stored() {
        // Storing it would evict everything else and then, on the next
        // insert, itself.
        let mut cache = ImageCache::new(1000);
        cache.insert(target(1, "a.png"), bytes(500));
        assert!(!cache.insert(target(1, "huge.png"), bytes(2000)));
        assert!(cache.contains(&target(1, "a.png")), "the rest must survive");
        assert_eq!(cache.used_bytes(), 500);
    }

    #[test]
    fn a_zero_budget_disables_the_cache() {
        let mut cache = ImageCache::new(0);
        assert!(!cache.insert(target(1, "a.png"), bytes(1)));
        assert!(cache.is_empty());
        assert!(cache.get(&target(1, "a.png")).is_none());
    }

    #[test]
    fn lowering_the_budget_evicts_at_once() {
        let mut cache = ImageCache::new(1000);
        cache.insert(target(1, "a.png"), bytes(400));
        cache.insert(target(1, "b.png"), bytes(400));
        cache.set_max_bytes(500);
        assert_eq!(cache.len(), 1);
        assert!(cache.contains(&target(1, "b.png")), "the newest survives");
        assert_eq!(cache.used_bytes(), 400);
    }

    #[test]
    fn clearing_resets_the_accounting_too() {
        let mut cache = ImageCache::new(1000);
        cache.insert(target(1, "a.png"), bytes(400));
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.used_bytes(), 0);
    }
}
