//! Runtime auto-connect of discovered interfaces (Codeberg #32, sub-task b).
//!
//! Sub-task (c) persists validated discovery announces as
//! [`DiscoveredInterfaceRecord`]s under `<storage>/discovery/interfaces`. This
//! layer turns those records into live connections: when auto-connect is
//! enabled and a record for an auto-connectable endpoint appears, it spawns a
//! TCP client to the advertised `host:port` at runtime, registers it with the
//! running event loop, and tears it down when the backing record disappears or
//! the connection stays down past a detach threshold.
//!
//! # Scope (matches Python)
//!
//! Python `RNS.Discovery.InterfaceDiscovery` only *actually* auto-connects a
//! `BackboneInterface` (`autoconnect`, `Discovery.py:626-677`);
//! `TCPClientInterface`/`I2PInterface` auto-connect is explicitly left
//! unimplemented ("add manually via `rnstatus -D`"). A `BackboneInterface`
//! reaches its peer over the same TCP client transport as a
//! `TCPServerInterface` in our stack (Codeberg #89), so both advertised types
//! map to a runtime-spawned TCP client to the advertised endpoint. Other types
//! (I2P and anything not in [`AUTOCONNECT_TYPES`]) are logged as unimplemented
//! and skipped, mirroring Python.
//!
//! # Layering
//!
//! The lifecycle state machine ([`AutoConnectManager`]) is medium-agnostic: it
//! decides *when* to spawn and tear down, and drives those through an
//! [`AutoConnectSpawner`]. The production spawner (in the driver event loop)
//! owns the TCP-client-specific wiring (address resolution, the reconnecting
//! interface task, event-loop registration). This keeps the carrier specifics
//! in the interface layer and the runtime-management logic here, and makes the
//! spawn/register/teardown lifecycle unit-testable against a mock spawner.

use std::collections::BTreeSet;

use leviculum_core::crypto::full_hash;
use leviculum_core::discovery::DiscoveredInterfaceRecord;
use leviculum_core::transport::InterfaceId;

/// Interface types we auto-connect at runtime (Python
/// `InterfaceDiscovery.AUTOCONNECT_TYPES`). Both advertise a reachable
/// `host:port` and are reached over a TCP client in our stack.
pub(crate) const AUTOCONNECT_TYPES: [&str; 2] = ["BackboneInterface", "TCPServerInterface"];

/// How long an auto-connected interface may report offline before it is torn
/// down (Python `InterfaceDiscovery.DETACH_THRESHOLD`, in seconds).
pub(crate) const DETACH_THRESHOLD_SECS: f64 = 12.0;

/// The spawn + teardown surface [`AutoConnectManager`] drives. Split out so the
/// lifecycle is unit-testable against a mock, while production wires it to the
/// running event loop's interface channels and online map.
pub(crate) trait AutoConnectSpawner {
    /// Spawn a TCP client interface to `host:port` at runtime and register it
    /// with the transport. `rec` is the backing discovery record; the spawner
    /// resolves the interface's IFAC from it (advertised netname/netkey, or
    /// inherited from the interface the announce was heard on — Codeberg #151).
    /// Returns the assigned [`InterfaceId`], or `None` if the endpoint could
    /// not be resolved, the interface could not be spawned, or the spawner
    /// refused the endpoint (fail closed: IFAC required but unresolvable).
    fn spawn_tcp_client(
        &mut self,
        name: &str,
        host: &str,
        port: u16,
        rec: &DiscoveredInterfaceRecord,
    ) -> Option<InterfaceId>;

    /// Tear down a previously spawned auto-connected interface.
    fn teardown(&mut self, id: InterfaceId);

    /// Whether the interface is currently online (its transport carrier is up).
    fn is_online(&self, id: InterfaceId) -> bool;
}

/// One live auto-connected interface, tracked for dedup and teardown.
struct Active {
    id: InterfaceId,
    /// `full_hash(reachable_on[:port])` — Python's `endpoint_hash`, the dedup
    /// key so the same endpoint is never auto-connected twice.
    endpoint_hash: [u8; 32],
    /// Wall-clock (Unix seconds) the interface was first seen offline, or
    /// `None` while it is online. Drives the detach-threshold teardown.
    down_since: Option<f64>,
}

/// Runtime auto-connect lifecycle for discovered interfaces.
///
/// A single integer gates and bounds the feature (matching Python, where
/// `autoconnect_discovered_interfaces` is both the on/off flag and the cap):
/// `0` disables it, `N > 0` enables it with at most `N` concurrently
/// auto-connected interfaces.
pub(crate) struct AutoConnectManager {
    max_interfaces: usize,
    active: Vec<Active>,
    /// Endpoints (`discovery_hash`) already warned about as unimplemented, so
    /// the repeated poll does not spam the log.
    warned_unimplemented: BTreeSet<[u8; 32]>,
}

impl AutoConnectManager {
    /// Create a manager. `max_interfaces == 0` leaves auto-connect disabled.
    pub(crate) fn new(max_interfaces: usize) -> Self {
        Self {
            max_interfaces,
            active: Vec::new(),
            warned_unimplemented: BTreeSet::new(),
        }
    }

    /// Whether auto-connect is enabled.
    pub(crate) fn enabled(&self) -> bool {
        self.max_interfaces > 0
    }

    /// Number of currently auto-connected interfaces (Python
    /// `autoconnect_count`).
    #[cfg(test)]
    pub(crate) fn active_count(&self) -> usize {
        self.active.len()
    }

    /// The endpoint dedup key Python auto-connections are keyed by:
    /// `full_hash(reachable_on + optional(":" + port))` (`Discovery.py:601-606`).
    pub(crate) fn endpoint_hash(reachable_on: &str, port: Option<u64>) -> [u8; 32] {
        let mut spec = String::from(reachable_on);
        if let Some(p) = port {
            spec.push(':');
            spec.push_str(&p.to_string());
        }
        full_hash(spec.as_bytes())
    }

    /// Resolve a record to a `(host, port)` TCP connect target, or `None` if
    /// the record is not an auto-connectable type or lacks a usable endpoint.
    fn connect_target(rec: &DiscoveredInterfaceRecord) -> Option<(&str, u16)> {
        if !AUTOCONNECT_TYPES.contains(&rec.interface_type.as_str()) {
            return None;
        }
        let host = rec.reachable_on.as_deref()?;
        let port = u16::try_from(rec.port?).ok()?;
        Some((host, port))
    }

    /// Drop internal tracking for an interface removed out-of-band (the event
    /// loop saw `Disconnected` and removed it). A later [`poll`](Self::poll)
    /// may re-auto-connect if the endpoint is still discovered.
    pub(crate) fn on_interface_removed(&mut self, id: InterfaceId) {
        self.active.retain(|a| a.id != id);
    }

    /// Reconcile the live discovered set against currently auto-connected
    /// interfaces (Python `connect_discovered` + `__monitor_job`).
    ///
    /// Teardown pass: an auto-connected interface is torn down when its backing
    /// record is gone from `live` (record expiry/removal) or it has reported
    /// offline continuously for at least [`DETACH_THRESHOLD_SECS`] (connection
    /// death). Spawn pass: each transport-capable auto-connectable record with
    /// an endpoint we are not already connected to is spawned, up to the
    /// configured cap. `now` is wall-clock seconds (for the offline timer).
    pub(crate) fn poll(
        &mut self,
        live: &[DiscoveredInterfaceRecord],
        now: f64,
        spawner: &mut impl AutoConnectSpawner,
    ) {
        if !self.enabled() {
            return;
        }

        // Endpoints still backed by a live autoconnectable record.
        let live_endpoints: BTreeSet<[u8; 32]> = live
            .iter()
            .filter(|r| Self::connect_target(r).is_some())
            .map(|r| Self::endpoint_hash(r.reachable_on.as_deref().unwrap_or(""), r.port))
            .collect();

        // Teardown pass. Endpoints torn down this tick are not re-spawned in the
        // spawn pass below: for an offline (connection-death) teardown the
        // record is still live, and instantly reconnecting the just-detached
        // endpoint would thrash. A later poll may re-establish it.
        let mut torn_this_tick: BTreeSet<[u8; 32]> = BTreeSet::new();
        let mut i = 0;
        while i < self.active.len() {
            let record_gone = !live_endpoints.contains(&self.active[i].endpoint_hash);
            let detach = if record_gone {
                true
            } else if spawner.is_online(self.active[i].id) {
                self.active[i].down_since = None;
                false
            } else {
                match self.active[i].down_since {
                    None => {
                        self.active[i].down_since = Some(now);
                        false
                    }
                    Some(t) => now - t >= DETACH_THRESHOLD_SECS,
                }
            };

            if detach {
                let id = self.active[i].id;
                torn_this_tick.insert(self.active[i].endpoint_hash);
                spawner.teardown(id);
                self.active.remove(i);
            } else {
                i += 1;
            }
        }

        // Spawn pass. `live` is caller-sorted best-first (Python
        // list_discovered_interfaces order), so the cap keeps the best peers.
        for rec in live {
            if self.active.len() >= self.max_interfaces {
                break;
            }
            // Python lists candidates with only_transport=True.
            if !rec.transport {
                continue;
            }
            let Some((host, port)) = Self::connect_target(rec) else {
                self.maybe_warn_unimplemented(rec);
                continue;
            };
            let endpoint_hash = Self::endpoint_hash(host, rec.port);
            if torn_this_tick.contains(&endpoint_hash) {
                continue; // just detached this tick; do not immediately reconnect
            }
            if self.active.iter().any(|a| a.endpoint_hash == endpoint_hash) {
                continue; // already auto-connected to this endpoint
            }
            let name = format!("autoconnect/{}", rec.name);
            if let Some(id) = spawner.spawn_tcp_client(&name, host, port, rec) {
                tracing::info!(
                    "discovery: auto-connecting {} \"{}\" at {}:{}",
                    rec.interface_type,
                    rec.name,
                    host,
                    port
                );
                self.active.push(Active {
                    id,
                    endpoint_hash,
                    down_since: None,
                });
            }
        }
    }

    /// Log (once per endpoint) that a discovered type is recognised but its
    /// auto-connect is not implemented, mirroring Python's warning for I2P.
    fn maybe_warn_unimplemented(&mut self, rec: &DiscoveredInterfaceRecord) {
        if rec.interface_type == "I2PInterface"
            && self.warned_unimplemented.insert(rec.discovery_hash)
        {
            tracing::warn!(
                "discovery: auto-connecting discovered I2P interfaces is not yet implemented; \
                 obtain the config entry and add it manually via `lnstatus -D`"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_core::discovery::DiscoveredInterface;
    use leviculum_core::discovery::STAMP_SIZE;

    /// Records spawner calls so the lifecycle can be asserted deterministically.
    #[derive(Default)]
    struct MockSpawner {
        next_id: usize,
        spawned: Vec<(String, String, u16, InterfaceId)>,
        /// IFAC netname/netkey of the record each spawn was handed (#151).
        spawned_ifac: Vec<(Option<String>, Option<String>)>,
        torn_down: Vec<InterfaceId>,
        /// Interface ids currently reporting offline (default: online).
        offline: BTreeSet<usize>,
        /// If set, the next spawn returns `None` (resolution failure).
        fail_next_spawn: bool,
    }

    impl AutoConnectSpawner for MockSpawner {
        fn spawn_tcp_client(
            &mut self,
            name: &str,
            host: &str,
            port: u16,
            rec: &DiscoveredInterfaceRecord,
        ) -> Option<InterfaceId> {
            if self.fail_next_spawn {
                self.fail_next_spawn = false;
                return None;
            }
            let id = InterfaceId(self.next_id);
            self.next_id += 1;
            self.spawned
                .push((name.to_string(), host.to_string(), port, id));
            self.spawned_ifac
                .push((rec.ifac_netname.clone(), rec.ifac_netkey.clone()));
            Some(id)
        }
        fn teardown(&mut self, id: InterfaceId) {
            self.torn_down.push(id);
        }
        fn is_online(&self, id: InterfaceId) -> bool {
            !self.offline.contains(&id.0)
        }
    }

    fn backbone_rec(name: &str, host: &str, port: u16, seed: u8) -> DiscoveredInterfaceRecord {
        let di = DiscoveredInterface {
            interface_type: "BackboneInterface".to_string(),
            transport: true,
            name: name.to_string(),
            transport_id: [seed; 16],
            network_id: [seed; 16],
            value: 20,
            stamp: [seed; STAMP_SIZE],
            latitude: None,
            longitude: None,
            height: None,
            reachable_on: Some(host.to_string()),
            port: Some(port as u64),
            frequency: None,
            bandwidth: None,
            spreadingfactor: None,
            codingrate: None,
            ifac_netname: None,
            ifac_netkey: None,
            discovery_hash: [seed; STAMP_SIZE],
        };
        DiscoveredInterfaceRecord::from_discovered(&di, 1, 1000.0, 1000.0, 1000.0, 0)
    }

    fn i2p_rec(seed: u8) -> DiscoveredInterfaceRecord {
        let di = DiscoveredInterface {
            interface_type: "I2PInterface".to_string(),
            transport: true,
            name: "i2p".to_string(),
            transport_id: [seed; 16],
            network_id: [seed; 16],
            value: 20,
            stamp: [seed; STAMP_SIZE],
            latitude: None,
            longitude: None,
            height: None,
            reachable_on: Some("abcd.b32.i2p".to_string()),
            port: None,
            frequency: None,
            bandwidth: None,
            spreadingfactor: None,
            codingrate: None,
            ifac_netname: None,
            ifac_netkey: None,
            discovery_hash: [seed; STAMP_SIZE],
        };
        DiscoveredInterfaceRecord::from_discovered(&di, 1, 1000.0, 1000.0, 1000.0, 0)
    }

    #[test]
    fn disabled_manager_never_spawns() {
        let mut mgr = AutoConnectManager::new(0);
        let mut sp = MockSpawner::default();
        assert!(!mgr.enabled());
        mgr.poll(&[backbone_rec("B", "10.0.0.5", 4965, 1)], 1000.0, &mut sp);
        assert!(sp.spawned.is_empty(), "disabled manager must not spawn");
    }

    #[test]
    fn discovered_record_spawns_and_registers_once() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        let rec = backbone_rec("Hub", "10.0.0.5", 4965, 1);

        mgr.poll(std::slice::from_ref(&rec), 1000.0, &mut sp);
        assert_eq!(sp.spawned.len(), 1, "one interface spawned + registered");
        assert_eq!(sp.spawned[0].1, "10.0.0.5");
        assert_eq!(sp.spawned[0].2, 4965);
        assert_eq!(mgr.active_count(), 1);

        // Re-polling the same record must not double-connect the endpoint.
        mgr.poll(std::slice::from_ref(&rec), 1001.0, &mut sp);
        assert_eq!(sp.spawned.len(), 1, "endpoint dedup prevents re-spawn");
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn record_removal_tears_down_interface() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        let rec = backbone_rec("Hub", "10.0.0.5", 4965, 1);

        mgr.poll(std::slice::from_ref(&rec), 1000.0, &mut sp);
        let id = sp.spawned[0].3;
        assert_eq!(mgr.active_count(), 1);

        // Record gone from the live set (expired/removed) -> teardown.
        mgr.poll(&[], 1100.0, &mut sp);
        assert_eq!(
            sp.torn_down,
            vec![id],
            "removed record unregisters interface"
        );
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn offline_past_threshold_tears_down() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        let rec = backbone_rec("Hub", "10.0.0.5", 4965, 1);

        mgr.poll(std::slice::from_ref(&rec), 1000.0, &mut sp);
        let id = sp.spawned[0].3;
        sp.offline.insert(id.0);

        // First offline poll only stamps down_since; no teardown yet.
        mgr.poll(std::slice::from_ref(&rec), 1001.0, &mut sp);
        assert!(sp.torn_down.is_empty(), "one offline poll must not detach");
        assert_eq!(mgr.active_count(), 1);

        // Still offline past the detach threshold -> teardown.
        mgr.poll(
            std::slice::from_ref(&rec),
            1001.0 + DETACH_THRESHOLD_SECS,
            &mut sp,
        );
        assert_eq!(sp.torn_down, vec![id], "sustained offline detaches");
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn recovered_before_threshold_is_not_torn_down() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        let rec = backbone_rec("Hub", "10.0.0.5", 4965, 1);

        mgr.poll(std::slice::from_ref(&rec), 1000.0, &mut sp);
        let id = sp.spawned[0].3;

        sp.offline.insert(id.0);
        mgr.poll(std::slice::from_ref(&rec), 1001.0, &mut sp); // stamp down
        sp.offline.remove(&id.0); // back online before threshold
        mgr.poll(
            std::slice::from_ref(&rec),
            1001.0 + DETACH_THRESHOLD_SECS,
            &mut sp,
        );
        assert!(sp.torn_down.is_empty(), "recovery clears the offline timer");
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn cap_bounds_concurrent_autoconnects() {
        let mut mgr = AutoConnectManager::new(1);
        let mut sp = MockSpawner::default();
        let recs = vec![
            backbone_rec("A", "10.0.0.5", 4965, 1),
            backbone_rec("B", "10.0.0.6", 4965, 2),
        ];
        mgr.poll(&recs, 1000.0, &mut sp);
        assert_eq!(sp.spawned.len(), 1, "cap of 1 permits only one autoconnect");
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn non_transport_records_are_skipped() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        let mut rec = backbone_rec("Hub", "10.0.0.5", 4965, 1);
        rec.transport = false;
        mgr.poll(std::slice::from_ref(&rec), 1000.0, &mut sp);
        assert!(
            sp.spawned.is_empty(),
            "non-transport peer is not auto-connected"
        );
    }

    #[test]
    fn i2p_type_is_unimplemented_not_spawned() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        mgr.poll(&[i2p_rec(9)], 1000.0, &mut sp);
        assert!(sp.spawned.is_empty(), "I2P autoconnect is unimplemented");
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn failed_spawn_is_not_tracked_and_retried() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        let rec = backbone_rec("Hub", "10.0.0.5", 4965, 1);

        sp.fail_next_spawn = true;
        mgr.poll(std::slice::from_ref(&rec), 1000.0, &mut sp);
        assert_eq!(mgr.active_count(), 0, "a failed spawn is not tracked");

        // Next poll retries and succeeds.
        mgr.poll(std::slice::from_ref(&rec), 1001.0, &mut sp);
        assert_eq!(sp.spawned.len(), 1);
        assert_eq!(mgr.active_count(), 1);
    }

    /// #151: the manager hands the backing record to the spawner, so the
    /// spawner can resolve the interface's IFAC from the advertised
    /// netname/netkey (or refuse the endpoint, fail closed).
    #[test]
    fn spawner_receives_the_records_ifac_material() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        let mut rec = backbone_rec("Hub", "10.0.0.5", 4965, 1);
        rec.ifac_netname = Some("closednet".to_string());
        rec.ifac_netkey = Some("closedkey".to_string());

        mgr.poll(std::slice::from_ref(&rec), 1000.0, &mut sp);
        assert_eq!(sp.spawned.len(), 1);
        assert_eq!(
            sp.spawned_ifac[0],
            (Some("closednet".to_string()), Some("closedkey".to_string())),
            "record IFAC material must reach the spawner"
        );
    }

    #[test]
    fn removed_interface_can_reconnect_on_rediscovery() {
        let mut mgr = AutoConnectManager::new(4);
        let mut sp = MockSpawner::default();
        let rec = backbone_rec("Hub", "10.0.0.5", 4965, 1);

        mgr.poll(std::slice::from_ref(&rec), 1000.0, &mut sp);
        let id = sp.spawned[0].3;

        // Event loop saw a hard Disconnected and removed it out-of-band.
        mgr.on_interface_removed(id);
        assert_eq!(mgr.active_count(), 0);

        // Still discovered -> a later poll re-auto-connects.
        mgr.poll(std::slice::from_ref(&rec), 1001.0, &mut sp);
        assert_eq!(sp.spawned.len(), 2, "rediscovery re-auto-connects");
        assert_eq!(mgr.active_count(), 1);
    }
}
