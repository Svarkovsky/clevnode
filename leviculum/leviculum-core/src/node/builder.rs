//! NodeCore builder pattern implementation
//!
//! The [`NodeCoreBuilder`] provides a fluent API for configuring and creating
//! a [`NodeCore`] instance.

use alloc::vec::Vec;

use crate::destination::ProofStrategy;
use crate::identity::Identity;
use crate::resource::{WindowPolicy, RESOURCE_MAX_INCOMING_SIZE};
use crate::transport::TransportConfig;
use rand_core::CryptoRngCore;

use super::NodeCore;

/// Builder for creating [`NodeCore`] instances
///
/// # Example
///
/// ```no_run
/// use leviculum_core::node::NodeCoreBuilder;
/// use leviculum_core::identity::Identity;
/// use leviculum_core::ProofStrategy;
/// use leviculum_core::traits::{Clock, NoStorage};
/// # use core::cell::Cell;
/// # struct MyClock(Cell<u64>);
/// # impl MyClock { fn new(ms: u64) -> Self { Self(Cell::new(ms)) } }
/// # impl Clock for MyClock { fn now_ms(&self) -> u64 { self.0.get() } }
///
/// # fn example() {
/// let my_identity = Identity::generate(&mut rand_core::OsRng);
///
/// let node = NodeCoreBuilder::new()
///     .identity(my_identity)
///     .proof_strategy(ProofStrategy::All)
///     .enable_transport(true)
///     .build(rand_core::OsRng, MyClock::new(0), NoStorage);
/// # }
/// ```
pub struct NodeCoreBuilder {
    identity: Option<Identity>,
    proof_strategy: ProofStrategy,
    transport_config: TransportConfig,
    respond_to_probes: bool,
    remote_management: bool,
    remote_management_allowed: Vec<[u8; crate::constants::TRUNCATED_HASHBYTES]>,
    max_incoming_resource_size: usize,
    resource_window_policy: WindowPolicy,
    peer_up_pull_names: Option<Vec<alloc::string::String>>,
}

impl Default for NodeCoreBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeCoreBuilder {
    /// Create a new builder with default settings
    pub fn new() -> Self {
        Self {
            identity: None,
            proof_strategy: ProofStrategy::None,
            transport_config: TransportConfig::default(),
            respond_to_probes: false,
            remote_management: false,
            remote_management_allowed: Vec::new(),
            max_incoming_resource_size: RESOURCE_MAX_INCOMING_SIZE,
            resource_window_policy: WindowPolicy::default(),
            peer_up_pull_names: None,
        }
    }

    /// Replace the full destination names the peer-up pull derives per-peer
    /// destinations from (Codeberg #365). Default: EMPTY — the handshake
    /// identity the pull would derive from is the peer's TRANSPORT
    /// identity, unrelated to its LXMF identity, so the derived name does
    /// not exist on reference peers; see the `peer_up_pull_names` field
    /// doc on `NodeCore` for the citations and
    /// `NodeCore::handle_interface_peer_up` for the mechanism. Setting
    /// e.g. `["lxmf.delivery"]` re-enables the pull for fleets whose
    /// nodes are known to hand their LXMF identity to the handshake (our
    /// firmware and lnsd do).
    pub fn peer_up_pull_names(mut self, names: Vec<alloc::string::String>) -> Self {
        self.peer_up_pull_names = Some(names);
        self
    }

    /// Set the node's identity
    ///
    /// If not set, a new identity will be generated during `build()`.
    pub fn identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Set the default proof strategy for destinations
    pub fn proof_strategy(mut self, strategy: ProofStrategy) -> Self {
        self.proof_strategy = strategy;
        self
    }

    /// Enable or disable transport mode (routing for other nodes)
    pub fn enable_transport(mut self, enable: bool) -> Self {
        self.transport_config.enable_transport = enable;
        self
    }

    /// Set the maximum number of hops for path finding
    pub fn max_hops(mut self, hops: u8) -> Self {
        self.transport_config.max_hops = hops;
        self
    }

    /// Set the path expiry time in seconds
    pub fn path_expiry_secs(mut self, secs: u64) -> Self {
        self.transport_config.path_expiry_secs = secs;
        self
    }

    /// Set the announce rate limit interval in milliseconds
    pub fn announce_rate_limit_ms(mut self, ms: u64) -> Self {
        self.transport_config.announce_rate_limit_ms = ms;
        self
    }

    /// Set the maximum number of queued announces per capped interface.
    /// Default: 16384. On embedded with limited heap, set to e.g. 32.
    pub fn max_queued_announces(mut self, n: usize) -> Self {
        self.transport_config.max_queued_announces = n;
        self
    }

    /// Set the maximum number of random blobs retained per path entry.
    /// Default: 64. On embedded with limited heap, set to e.g. 8.
    pub fn max_random_blobs(mut self, n: usize) -> Self {
        self.transport_config.max_random_blobs = n;
        self
    }

    /// Override the link keepalive interval (seconds) for every link this node
    /// creates. `None` (default) keeps the RTT-derived interval. Useful for
    /// slow links and for shrinking the stale-link timeout in tests.
    pub fn link_keepalive(mut self, secs: Option<u64>) -> Self {
        self.transport_config.link_keepalive_secs = secs;
        self
    }

    /// Set LRPROOF hop-asymmetry handling (#38). `true` (default) rewrites a
    /// mismatching forwarded proof to the frozen count; `false` selects the
    /// reference-exact strict check that DROPS on mismatch, arming the healing
    /// loop. See [`TransportConfig::lrproof_rewrite_on_asymmetry`].
    pub fn lrproof_rewrite_on_asymmetry(mut self, rewrite: bool) -> Self {
        self.transport_config.lrproof_rewrite_on_asymmetry = rewrite;
        self
    }

    /// Enable probe responder (rnstransport.probe destination with PROVE_ALL).
    ///
    /// When enabled, the node creates a probe destination from its transport
    /// identity at build time and announces it periodically. Other nodes can
    /// then use `rnprobe` to measure RTT via delivery proofs.
    pub fn respond_to_probes(mut self, enable: bool) -> Self {
        self.respond_to_probes = enable;
        self
    }

    /// Enable remote management (rnstransport.remote.management `/status`).
    ///
    /// When enabled, the node creates a `rnstransport.remote.management`
    /// destination from its transport identity at build time, registers a
    /// `/status` request handler gated by `allowed` (identity hashes), and
    /// announces the destination periodically so `rnstatus -R` clients can
    /// resolve it. Mirrors Python `Transport.py:253-259`.
    pub fn remote_management(
        mut self,
        enable: bool,
        allowed: Vec<[u8; crate::constants::TRUNCATED_HASHBYTES]>,
    ) -> Self {
        self.remote_management = enable;
        self.remote_management_allowed = allowed;
        self
    }

    /// Set the maximum incoming resource size in bytes.
    ///
    /// Resources advertised with `transfer_size` above this limit are
    /// rejected before any allocation. Default: `usize::MAX` (no limit).
    /// On embedded targets with limited heap, set to e.g. `8 * 1024`.
    pub fn max_incoming_resource_size(mut self, size: usize) -> Self {
        self.max_incoming_resource_size = size;
        self
    }

    /// Set the receive-window adaptation policy for incoming resources
    /// (Codeberg #85). Default: [`WindowPolicy::Current`], the historical
    /// algorithm. Applied to every `IncomingResource` this node creates.
    pub fn resource_window_policy(mut self, policy: WindowPolicy) -> Self {
        self.resource_window_policy = policy;
        self
    }

    /// Set the full transport configuration
    pub fn transport_config(mut self, config: TransportConfig) -> Self {
        self.transport_config = config;
        self
    }

    /// Check if transport mode is enabled in this builder
    pub fn is_transport_enabled(&self) -> bool {
        self.transport_config.enable_transport
    }

    /// Access the identity set on this builder (if any)
    pub fn identity_ref(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    /// Build the NodeCore instance
    ///
    /// If no identity was provided, a new one will be generated using the RNG.
    ///
    /// Returns by value, so the caller's frame carries a whole `NodeCore`.
    /// Fine when `S` keeps its data on the heap; use [`Self::build_boxed`]
    /// when `S` stores inline, or the value lands on a stack that cannot
    /// hold it.
    ///
    /// # Arguments
    /// * `rng` - Random number generator (moved into NodeCore)
    /// * `clock` - Clock instance (moved into NodeCore)
    /// * `storage` - Storage instance (moved into NodeCore)
    pub fn build<R, Clk, S>(self, mut rng: R, clock: Clk, storage: S) -> NodeCore<R, Clk, S>
    where
        R: CryptoRngCore,
        Clk: crate::traits::Clock,
        S: crate::traits::Storage,
    {
        // Get or generate identity
        let identity = match self.identity {
            Some(id) => id,
            None => Identity::generate(&mut rng),
        };

        let mut node = NodeCore::new(
            identity,
            self.transport_config,
            self.proof_strategy,
            self.max_incoming_resource_size,
            self.resource_window_policy,
            rng,
            clock,
            storage,
        );

        if self.respond_to_probes {
            node.enable_probe_responder();
        }

        if self.remote_management {
            node.enable_remote_management(self.remote_management_allowed);
        }

        if let Some(names) = self.peer_up_pull_names {
            node.set_peer_up_pull_names(names);
        }

        node
    }

    /// Build the NodeCore straight into a heap allocation.
    ///
    /// Same result as `Box::new(builder.build(..))`, but the value is never
    /// held as a by-value local of this function: the allocation is made
    /// first and the post-construction configuration (`respond_to_probes`,
    /// `remote_management`) runs through the box. `build()` cannot do that —
    /// it owns a `NodeCore` local it mutates before returning, which forces
    /// the caller's frame to carry a full-size temporary plus the return
    /// slot.
    ///
    /// That matters on embedded, where `S` is an inline-storage type: with
    /// `EmbeddedStorage` a `NodeCore` is >40 KB, and `Box::new(build(..))`
    /// put two of those on the T114's 128 KB stack (a 94 KB `main` frame,
    /// leaving ~13 KB of margin and corrupting SoftDevice RAM on the deeper
    /// paths). Drivers with a large `S` must use this instead of `build()`.
    pub fn build_boxed<R, Clk, S>(
        self,
        mut rng: R,
        clock: Clk,
        storage: S,
    ) -> alloc::boxed::Box<NodeCore<R, Clk, S>>
    where
        R: CryptoRngCore,
        Clk: crate::traits::Clock,
        S: crate::traits::Storage,
    {
        let identity = match self.identity {
            Some(id) => id,
            None => Identity::generate(&mut rng),
        };

        let mut node = alloc::boxed::Box::new(NodeCore::new(
            identity,
            self.transport_config,
            self.proof_strategy,
            self.max_incoming_resource_size,
            self.resource_window_policy,
            rng,
            clock,
            storage,
        ));

        if self.respond_to_probes {
            node.enable_probe_responder();
        }

        if self.remote_management {
            node.enable_remote_management(self.remote_management_allowed);
        }

        if let Some(names) = self.peer_up_pull_names {
            node.set_peer_up_pull_names(names);
        }

        node
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockClock, TEST_TIME_MS};
    use crate::traits::NoStorage;
    use rand_core::OsRng;

    #[test]
    fn test_builder_default() {
        let clock = MockClock::new(TEST_TIME_MS);
        let node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

        assert_eq!(node.active_link_count(), 0);
        assert_eq!(node.pending_link_count(), 0);
    }

    #[test]
    fn test_builder_with_identity() {
        let identity = Identity::generate(&mut OsRng);
        let id_hash = *identity.hash();
        let clock = MockClock::new(TEST_TIME_MS);

        let node = NodeCoreBuilder::new()
            .identity(identity)
            .build(OsRng, clock, NoStorage);

        assert_eq!(node.identity().hash(), &id_hash);
    }

    #[test]
    fn test_builder_with_proof_strategy() {
        let clock = MockClock::new(TEST_TIME_MS);
        let node = NodeCoreBuilder::new()
            .proof_strategy(ProofStrategy::All)
            .build(OsRng, clock, NoStorage);

        assert_eq!(node.default_proof_strategy(), ProofStrategy::All);
    }

    #[test]
    fn test_builder_with_transport_config() {
        let clock = MockClock::new(TEST_TIME_MS);
        let node = NodeCoreBuilder::new()
            .enable_transport(true)
            .max_hops(10)
            .path_expiry_secs(7200)
            .build(OsRng, clock, NoStorage);

        assert!(node.transport_config().enable_transport);
        assert_eq!(node.transport_config().max_hops, 10);
        assert_eq!(node.transport_config().path_expiry_secs, 7200);
    }

    #[test]
    fn test_builder_link_keepalive_reaches_config() {
        let clock = MockClock::new(TEST_TIME_MS);
        let node = NodeCoreBuilder::new()
            .link_keepalive(Some(7))
            .build(OsRng, clock, NoStorage);
        assert_eq!(node.transport_config().link_keepalive_secs, Some(7));
    }

    #[test]
    fn test_builder_chaining() {
        let identity = Identity::generate(&mut OsRng);
        let clock = MockClock::new(TEST_TIME_MS);

        let node = NodeCoreBuilder::new()
            .identity(identity)
            .proof_strategy(ProofStrategy::App)
            .enable_transport(false)
            .max_hops(5)
            .path_expiry_secs(3600)
            .announce_rate_limit_ms(5000)
            .build(OsRng, clock, NoStorage);

        assert_eq!(node.default_proof_strategy(), ProofStrategy::App);
        assert!(!node.transport_config().enable_transport);
        assert_eq!(node.transport_config().max_hops, 5);
    }

    #[test]
    fn test_respond_to_probes_creates_destination() {
        let clock = MockClock::new(TEST_TIME_MS);
        let node = NodeCoreBuilder::new()
            .respond_to_probes(true)
            .build(OsRng, clock, NoStorage);

        let probe_hash = node.probe_dest_hash();
        assert!(
            probe_hash.is_some(),
            "probe destination should be registered"
        );

        // Verify it's a real registered destination
        let hash = probe_hash.unwrap();
        assert!(node.destination(hash).is_some());
    }

    #[test]
    fn test_respond_to_probes_disabled_by_default() {
        let clock = MockClock::new(TEST_TIME_MS);
        let node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

        assert!(
            node.probe_dest_hash().is_none(),
            "probe should not be enabled by default"
        );
    }

    #[test]
    fn test_probe_destination_has_prove_all_strategy() {
        let clock = MockClock::new(TEST_TIME_MS);
        let node = NodeCoreBuilder::new()
            .respond_to_probes(true)
            .build(OsRng, clock, NoStorage);

        let hash = node.probe_dest_hash().unwrap();
        let dest = node.destination(hash).unwrap();
        assert_eq!(dest.proof_strategy(), ProofStrategy::All);
    }

    #[test]
    fn test_build_boxed_matches_build() {
        // `build_boxed` exists so a driver with a large inline `S` never puts
        // a `NodeCore` on its own stack. It must stay behaviourally identical
        // to `Box::new(build(..))`, including the post-construction steps
        // (`respond_to_probes`, `remote_management`) that now run through the
        // box instead of through a local.
        let identity = Identity::generate(&mut OsRng);
        let id_hash = *identity.hash();
        let mgmt_allowed = alloc::vec![[7u8; crate::constants::TRUNCATED_HASHBYTES]];

        let node = NodeCoreBuilder::new()
            .identity(identity)
            .proof_strategy(ProofStrategy::All)
            .enable_transport(true)
            .max_hops(9)
            .respond_to_probes(true)
            .remote_management(true, mgmt_allowed)
            .build_boxed(OsRng, MockClock::new(TEST_TIME_MS), NoStorage);

        assert_eq!(node.identity().hash(), &id_hash);
        assert_eq!(node.default_proof_strategy(), ProofStrategy::All);
        assert!(node.transport_config().enable_transport);
        assert_eq!(node.transport_config().max_hops, 9);

        let probe_hash = node.probe_dest_hash().expect("probe destination");
        assert!(node.destination(probe_hash).is_some());
        assert_eq!(
            node.destination(probe_hash).unwrap().proof_strategy(),
            ProofStrategy::All
        );
        assert!(node.remote_mgmt_dest_hash().is_some());
        // 15s base plus per-node jitter (mvr_probe_announce_phase pins why).
        let deadline = node.next_deadline().expect("mgmt announce deadline");
        assert!((TEST_TIME_MS + 15_000..TEST_TIME_MS + 20_000).contains(&deadline));
    }

    #[test]
    fn test_build_boxed_generates_identity_when_unset() {
        let node =
            NodeCoreBuilder::new().build_boxed(OsRng, MockClock::new(TEST_TIME_MS), NoStorage);
        assert!(node.probe_dest_hash().is_none());
        // A generated identity is a real one, not a zeroed placeholder.
        assert_ne!(node.identity().hash(), &[0u8; 16]);
    }

    #[test]
    fn test_probe_schedules_mgmt_announce() {
        let clock = MockClock::new(TEST_TIME_MS);
        let node = NodeCoreBuilder::new()
            .respond_to_probes(true)
            .build(OsRng, clock, NoStorage);

        // Should have a next_deadline for the mgmt announce: 15s after
        // startup plus per-node jitter (mvr_probe_announce_phase pins why
        // the instant must not be a sharp shared constant).
        let deadline = node.next_deadline();
        assert!(
            deadline.is_some(),
            "mgmt announce should schedule a deadline"
        );
        let deadline = deadline.unwrap();
        assert!(
            (TEST_TIME_MS + 15_000..TEST_TIME_MS + 20_000).contains(&deadline),
            "mgmt announce deadline {deadline} outside 15s+jitter window"
        );
    }
}
