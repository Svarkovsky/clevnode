//! NomadNet node discovery from received announces.
//!
//! A NomadNet node hosts its pages on a `SINGLE` destination with the app name
//! `nomadnetwork` and the single aspect `node` (`nomadnet/Node.py`: `RNS.Destination(
//! ..., "nomadnetwork", "node")`). Every RNS announce carries an
//! identity-independent `name_hash` = the 10-byte SHA-256 prefix of the full
//! destination name `"nomadnetwork.node"`, so a node announce can be recognised
//! without knowing any identity: filter received announces on
//! `name_hash() == Destination::compute_name_hash("nomadnetwork", &["node"])`.
//!
//! The announce `app_data` a NomadNet node sends is the node's display name as
//! plain UTF-8 (`Node.announce()`: `self.app_data = self.name.encode("utf-8")`;
//! `self.destination.announce(app_data=self.app_data)`), so the name is decoded
//! straight from the bytes, gracefully when it is absent or not valid UTF-8.
//!
//! [`NomadNodeRegistry`] consumes `AnnounceReceived` events, keeps only the node
//! announces, and upserts a [`DiscoveredNode`] keyed by destination hash so a
//! discovery loop (or the browser, opportunistically) can present a live list of
//! reachable nodes to open.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use leviculum_core::constants::NAME_HASHBYTES;
use leviculum_core::{Destination, ReceivedAnnounce};

/// The maximum number of nodes the registry retains. Discovery runs for the
/// whole session and a busy mesh announces far more nodes than a user can act
/// on, so the registry is a bounded FIFO: once it is full, learning a new node
/// evicts the oldest-inserted one. RAM only — never persisted.
pub const NODE_CAPACITY: usize = 500;

/// The app name a NomadNet node destination is registered under.
pub const NOMADNET_APP_NAME: &str = "nomadnetwork";

/// The single aspect of a NomadNet node destination (`nomadnetwork.node`).
pub const NOMADNET_NODE_ASPECTS: [&str; 1] = ["node"];

/// The `name_hash` every `nomadnetwork.node` announce carries: the first
/// [`NAME_HASHBYTES`] bytes of `SHA-256("nomadnetwork.node")`. Announces are
/// filtered against this before any parsing, so a non-node announce (the vast
/// majority) is rejected by a cheap 10-byte compare.
pub fn nomad_node_name_hash() -> [u8; NAME_HASHBYTES] {
    Destination::compute_name_hash(NOMADNET_APP_NAME, &NOMADNET_NODE_ASPECTS)
}

/// Whether a `name_hash` is the `nomadnetwork.node` one. This is the whole
/// filter: it never inspects the announce's keys, so it is identity-independent,
/// and it is unit-testable without constructing a full announce.
pub fn name_hash_is_nomad_node(name_hash: &[u8; NAME_HASHBYTES]) -> bool {
    name_hash == &nomad_node_name_hash()
}

/// Whether `announce` is a NomadNet node announce (its `name_hash` matches
/// [`nomad_node_name_hash`]).
pub fn is_nomad_node_announce(announce: &ReceivedAnnounce) -> bool {
    name_hash_is_nomad_node(announce.name_hash())
}

/// Decode a node display name from an announce `app_data`.
///
/// NomadNet sends the name as plain UTF-8 (`Node.announce`). Returns `None` when
/// the payload is empty (name absent) or not valid UTF-8, so a malformed or
/// nameless announce still registers the node without a name.
pub fn decode_node_name(app_data: &[u8]) -> Option<String> {
    if app_data.is_empty() {
        return None;
    }
    core::str::from_utf8(app_data)
        .ok()
        .map(|name| name.to_string())
}

/// Current wall-clock time in whole seconds since the Unix epoch, or `0` if the
/// clock is before the epoch. Used to stamp `first_seen`/`last_seen`.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A NomadNet node learned from one or more announces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredNode {
    /// The node's destination hash (the URL destination to open pages on).
    pub dest_hash: [u8; 16],
    /// The node's display name, if the announce carried a decodable one.
    pub name: Option<String>,
    /// Unix seconds when this node was first seen.
    pub first_seen: u64,
    /// Unix seconds when the most recent announce for it arrived.
    pub last_seen: u64,
    /// Hop count to the node from the most recent announce, if known.
    pub hops: Option<u32>,
}

impl DiscoveredNode {
    /// The destination hash as lowercase hex, for display and URL building.
    pub fn dest_hex(&self) -> String {
        let mut s = String::with_capacity(self.dest_hash.len() * 2);
        for byte in &self.dest_hash {
            s.push_str(&format!("{byte:02x}"));
        }
        s
    }

    /// The display name, or the dest hex when no name is known.
    pub fn display_name(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => self.dest_hex(),
        }
    }
}

/// A registry of discovered NomadNet nodes, keyed by destination hash.
///
/// [`observe`](Self::observe) is the filter-and-upsert: it drops non-node
/// announces and, for node announces, inserts a new [`DiscoveredNode`] or updates
/// an existing one's `last_seen`/`hops` (and fills its `name` from the announce
/// when it carries one). Insertion order is preserved for stable numbering by
/// tracking a per-node sequence.
///
/// The registry is a bounded FIFO of [`NODE_CAPACITY`] nodes: a re-announce of a
/// known node upserts in place and keeps its slot; a new node appends and, when
/// that would exceed the capacity, evicts the oldest-inserted node first.
#[derive(Debug, Default, Clone)]
pub struct NomadNodeRegistry {
    nodes: BTreeMap<[u8; 16], DiscoveredNode>,
    /// First-seen sequence per node, so the list keeps a stable discovery order
    /// independent of the hash-ordered map.
    order: BTreeMap<[u8; 16], u64>,
    next_seq: u64,
}

impl NomadNodeRegistry {
    /// A fresh, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter and upsert an announce. Returns `true` when it was a NomadNet node
    /// announce (and so was recorded), `false` when it was rejected by the
    /// name-hash filter.
    ///
    /// `hops` is the current hop count to the node (from the node's own path
    /// table), if known; `now` is the observation time in Unix seconds.
    pub fn observe(&mut self, announce: &ReceivedAnnounce, hops: Option<u32>, now: u64) -> bool {
        self.observe_parts(
            announce.name_hash(),
            *announce.destination_hash().as_bytes(),
            announce.app_data(),
            hops,
            now,
        )
    }

    /// The filter-and-upsert on the decomposed announce fields, so the registry's
    /// filtering can be exercised without constructing a full [`ReceivedAnnounce`]
    /// (whose parser is crate-private). Returns `true` when the announce was a
    /// node announce and was recorded.
    pub fn observe_parts(
        &mut self,
        name_hash: &[u8; NAME_HASHBYTES],
        dest_hash: [u8; 16],
        app_data: &[u8],
        hops: Option<u32>,
        now: u64,
    ) -> bool {
        if !name_hash_is_nomad_node(name_hash) {
            return false;
        }
        self.upsert(dest_hash, decode_node_name(app_data), hops, now);
        true
    }

    /// Insert a node or update an existing one. A later announce refreshes
    /// `last_seen` and `hops`; a name is filled when present but never cleared by
    /// a later nameless announce.
    pub fn upsert(
        &mut self,
        dest_hash: [u8; 16],
        name: Option<String>,
        hops: Option<u32>,
        now: u64,
    ) {
        match self.nodes.get_mut(&dest_hash) {
            Some(node) => {
                node.last_seen = now;
                node.hops = hops;
                if name.is_some() {
                    node.name = name;
                }
            }
            None => {
                self.insert_new(DiscoveredNode {
                    dest_hash,
                    name,
                    first_seen: now,
                    last_seen: now,
                    hops,
                });
            }
        }
    }

    /// Fold a whole [`DiscoveredNode`] in, preserving its own `first_seen`.
    ///
    /// Unlike [`upsert`](Self::upsert), which stamps `first_seen` at observation
    /// time, this merges a node learned elsewhere (e.g. forwarded from a
    /// [`Session`](crate::fetch::Session)'s own registry over a channel): an
    /// existing entry has its `last_seen`/`hops` refreshed and its name filled
    /// when the incoming one carries a name; a new dest is inserted verbatim,
    /// keeping the carried timestamps.
    pub fn upsert_node(&mut self, node: &DiscoveredNode) {
        match self.nodes.get_mut(&node.dest_hash) {
            Some(existing) => {
                existing.last_seen = node.last_seen;
                existing.hops = node.hops;
                if node.name.is_some() {
                    existing.name = node.name.clone();
                }
            }
            None => {
                self.insert_new(node.clone());
            }
        }
    }

    /// Append a brand-new node, evicting the oldest-inserted one first when the
    /// registry is already at [`NODE_CAPACITY`]. The caller has already confirmed
    /// `node.dest_hash` is unseen.
    fn insert_new(&mut self, node: DiscoveredNode) {
        if self.nodes.len() >= NODE_CAPACITY {
            self.evict_oldest();
        }
        let dest_hash = node.dest_hash;
        self.order.insert(dest_hash, self.next_seq);
        self.next_seq += 1;
        self.nodes.insert(dest_hash, node);
    }

    /// Evict the oldest-inserted node (the smallest first-seen sequence), keeping
    /// the registry a FIFO bounded by [`NODE_CAPACITY`]. A no-op on an empty
    /// registry.
    fn evict_oldest(&mut self) {
        let oldest = self
            .order
            .iter()
            .min_by_key(|(_, &seq)| seq)
            .map(|(dest, _)| *dest);
        if let Some(dest) = oldest {
            self.nodes.remove(&dest);
            self.order.remove(&dest);
        }
    }

    /// The discovered nodes in discovery order (oldest first).
    pub fn nodes(&self) -> Vec<&DiscoveredNode> {
        let mut nodes: Vec<&DiscoveredNode> = self.nodes.values().collect();
        nodes.sort_by_key(|n| self.order.get(&n.dest_hash).copied().unwrap_or(u64::MAX));
        nodes
    }

    /// The node with the given destination hash, if discovered.
    pub fn get_by_hash(&self, dest_hash: &[u8; 16]) -> Option<&DiscoveredNode> {
        self.nodes.get(dest_hash)
    }

    /// The node at 1-based discovery index `n`, matching the numbering shown to
    /// the user (`[1]`, `[2]`, ...).
    pub fn get(&self, n: usize) -> Option<&DiscoveredNode> {
        if n == 0 {
            return None;
        }
        self.nodes().into_iter().nth(n - 1)
    }

    /// The number of discovered nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether no nodes have been discovered yet.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `name_hash` of a `<app_name>.<aspect...>` destination, the value a real
    /// announce for it carries.
    fn name_hash(app_name: &str, aspects: &[&str]) -> [u8; NAME_HASHBYTES] {
        Destination::compute_name_hash(app_name, aspects)
    }

    #[test]
    fn name_hash_constant_matches_compute_name_hash() {
        assert_eq!(
            nomad_node_name_hash(),
            Destination::compute_name_hash("nomadnetwork", &["node"]),
        );
    }

    #[test]
    fn filter_accepts_node_name_hash_rejects_other_aspect() {
        // The name_hash a NomadNet node announce carries is accepted.
        assert!(name_hash_is_nomad_node(&name_hash(
            "nomadnetwork",
            &["node"]
        )));

        // A different aspect on the same app name is a different name_hash and is
        // rejected.
        assert!(!name_hash_is_nomad_node(&name_hash(
            "nomadnetwork",
            &["propagation"]
        )));

        // A wholly different app name is rejected.
        assert!(!name_hash_is_nomad_node(&name_hash("lxmf", &["delivery"])));
    }

    #[test]
    fn decode_name_present_absent_and_non_utf8() {
        assert_eq!(decode_node_name(b"NodeName"), Some("NodeName".to_string()));
        assert_eq!(decode_node_name(b""), None);
        // 0xff is not valid UTF-8.
        assert_eq!(decode_node_name(&[0xff, 0xfe]), None);
    }

    #[test]
    fn observe_rejects_non_node_and_accepts_node() {
        let mut reg = NomadNodeRegistry::new();
        // A non-node announce (different aspect) is filtered out.
        assert!(!reg.observe_parts(
            &name_hash("nomadnetwork", &["propagation"]),
            [1u8; 16],
            b"x",
            None,
            100
        ));
        assert_eq!(reg.len(), 0);

        // A node announce is accepted and recorded with its decoded name.
        assert!(reg.observe_parts(
            &name_hash("nomadnetwork", &["node"]),
            [2u8; 16],
            b"Alpha",
            Some(2),
            100
        ));
        assert_eq!(reg.len(), 1);
        let discovered = reg.get(1).expect("node 1");
        assert_eq!(discovered.name.as_deref(), Some("Alpha"));
        assert_eq!(discovered.dest_hash, [2u8; 16]);
        assert_eq!(discovered.hops, Some(2));
        assert_eq!(discovered.first_seen, 100);
        assert_eq!(discovered.last_seen, 100);
    }

    #[test]
    fn registry_dedups_by_dest_and_updates_last_seen() {
        let mut reg = NomadNodeRegistry::new();
        let hash = [7u8; 16];
        reg.upsert(hash, Some("First".to_string()), Some(1), 100);
        // A second announce for the same dest refreshes last_seen and hops but
        // keeps the single entry and its first_seen.
        reg.upsert(hash, Some("First".to_string()), Some(3), 250);
        assert_eq!(reg.len(), 1);
        let node = reg.get(1).expect("node 1");
        assert_eq!(node.first_seen, 100);
        assert_eq!(node.last_seen, 250);
        assert_eq!(node.hops, Some(3));
    }

    #[test]
    fn nameless_announce_does_not_clear_a_known_name() {
        let mut reg = NomadNodeRegistry::new();
        let hash = [9u8; 16];
        reg.upsert(hash, Some("Named".to_string()), Some(1), 10);
        reg.upsert(hash, None, Some(1), 20);
        assert_eq!(reg.get(1).unwrap().name.as_deref(), Some("Named"));
    }

    #[test]
    fn discovery_order_is_stable_by_first_seen() {
        let mut reg = NomadNodeRegistry::new();
        // Two hashes whose byte order is the reverse of their discovery order,
        // so a naive map iteration would swap them.
        let first = [0xf0u8; 16];
        let second = [0x01u8; 16];
        reg.upsert(first, Some("First".to_string()), None, 10);
        reg.upsert(second, Some("Second".to_string()), None, 20);
        let nodes = reg.nodes();
        assert_eq!(nodes[0].name.as_deref(), Some("First"));
        assert_eq!(nodes[1].name.as_deref(), Some("Second"));
        assert_eq!(reg.get(1).unwrap().name.as_deref(), Some("First"));
        assert_eq!(reg.get(2).unwrap().name.as_deref(), Some("Second"));
    }

    /// Build a distinct 16-byte dest hash from an index, so a FIFO test can mint
    /// more than 256 unique nodes (the low two bytes carry the whole range).
    fn dest(i: u32) -> [u8; 16] {
        let mut h = [0u8; 16];
        h[0] = (i >> 8) as u8;
        h[1] = (i & 0xff) as u8;
        h
    }

    #[test]
    fn registry_is_a_bounded_fifo_of_capacity() {
        let mut reg = NomadNodeRegistry::new();
        // Fill to exactly capacity with distinct nodes.
        for i in 0..NODE_CAPACITY as u32 {
            reg.upsert(dest(i), Some(format!("n{i}")), None, i as u64);
        }
        assert_eq!(reg.len(), NODE_CAPACITY);
        assert!(
            reg.get_by_hash(&dest(0)).is_some(),
            "first present when full"
        );

        // Re-announcing an existing node upserts in place: no growth, no evict,
        // and it keeps its slot in discovery order (still first).
        reg.upsert(dest(0), Some("renamed".to_string()), Some(4), 999);
        assert_eq!(reg.len(), NODE_CAPACITY, "upsert must not grow the buffer");
        assert!(reg.get_by_hash(&dest(0)).is_some(), "upsert kept oldest");
        assert_eq!(
            reg.nodes()[0].dest_hash,
            dest(0),
            "order preserved by upsert"
        );
        assert_eq!(reg.nodes()[0].name.as_deref(), Some("renamed"));

        // One more DISTINCT node exceeds capacity: the oldest (dest(0)) is
        // evicted, the newcomer is present, and the count stays at capacity.
        let newcomer = dest(NODE_CAPACITY as u32);
        reg.upsert(newcomer, Some("new".to_string()), None, 1000);
        assert_eq!(reg.len(), NODE_CAPACITY, "FIFO stays bounded");
        assert!(reg.get_by_hash(&dest(0)).is_none(), "oldest evicted first");
        assert!(reg.get_by_hash(&newcomer).is_some(), "newcomer retained");
        // dest(1) is now the oldest surviving node, so it heads discovery order.
        assert_eq!(
            reg.nodes()[0].dest_hash,
            dest(1),
            "order preserved after evict"
        );
        assert_eq!(reg.nodes().last().unwrap().dest_hash, newcomer);
    }

    #[test]
    fn upsert_node_also_evicts_oldest_when_full() {
        let mut reg = NomadNodeRegistry::new();
        for i in 0..NODE_CAPACITY as u32 {
            reg.upsert_node(&DiscoveredNode {
                dest_hash: dest(i),
                name: Some(format!("n{i}")),
                first_seen: i as u64,
                last_seen: i as u64,
                hops: None,
            });
        }
        assert_eq!(reg.len(), NODE_CAPACITY);
        reg.upsert_node(&DiscoveredNode {
            dest_hash: dest(NODE_CAPACITY as u32),
            name: Some("new".to_string()),
            first_seen: 1000,
            last_seen: 1000,
            hops: None,
        });
        assert_eq!(reg.len(), NODE_CAPACITY);
        assert!(reg.get_by_hash(&dest(0)).is_none(), "oldest evicted");
        assert!(reg.get_by_hash(&dest(NODE_CAPACITY as u32)).is_some());
    }

    #[test]
    fn get_zero_is_none() {
        let mut reg = NomadNodeRegistry::new();
        reg.upsert([1u8; 16], None, None, 0);
        assert!(reg.get(0).is_none());
        assert!(reg.get(2).is_none());
    }
}
