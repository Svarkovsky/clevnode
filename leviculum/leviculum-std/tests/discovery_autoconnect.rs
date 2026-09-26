//! End-to-end integration test for runtime auto-connect of discovered
//! interfaces (Codeberg #32, sub-task b).
//!
//! Proves the whole discovery -> auto-connect -> traffic chain with two of our
//! own nodes over TCP loopback:
//!
//!   * Node B is discoverable: it runs a TCP server on its "backbone" port and
//!     emits a discovery announce (32a wire format) advertising that endpoint.
//!   * Node A has auto-connect enabled and reaches B over a separate bootstrap
//!     link. When A hears B's discovery announce it persists the record and,
//!     at runtime, spawns a TCP client to B's advertised host:port and
//!     registers it with the transport.
//!   * Traffic then crosses the auto-established link: B re-announces over the
//!     accepted connection, so A's auto-connected interface receives bytes.
//!
//! The bootstrap link (A -> B:bootstrap_port) exists only so A can *hear* B's
//! discovery announce; the endpoint A auto-connects to is B's *second*,
//! separately-advertised server port, so the auto-connected interface is a
//! genuinely new link, distinct from the bootstrap one.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use leviculum_core::discovery::{
    build_announce_app_data, DiscoveredInterface, DiscoveredInterfaceRecord, InterfaceDescriptor,
};
use leviculum_core::{Destination, DestinationType, Direction, Identity};
use leviculum_std::config::{Config, InterfaceConfig};
use leviculum_std::driver::ReticulumNodeBuilder;

/// Host-wide listener-port allocator, shared with the `mvr` and
/// `rnsd_interop` suites.
#[path = "support/port_alloc.rs"]
#[allow(dead_code)]
mod port_alloc;

/// This file used to draw from a private counter based at 53100 — inside the
/// default `ip_local_port_range`, where an OS-assigned `bind(0)` anywhere on
/// the host can take a number between the probe and the consumer's bind. The
/// shared band sits above that ceiling; see `port_alloc`.
fn next_port() -> u16 {
    port_alloc::free_tcp_port()
}

/// Poll `cond` every 100 ms until it returns true or the deadline passes.
async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    cond()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovered_backbone_endpoint_is_auto_connected_and_carries_traffic() {
    let bootstrap_port = next_port();
    let backbone_port = next_port();
    let bootstrap_addr: SocketAddr = format!("127.0.0.1:{bootstrap_port}").parse().unwrap();
    let backbone_addr: SocketAddr = format!("127.0.0.1:{backbone_port}").parse().unwrap();

    // Node B: discoverable server. Bootstrap port carries the discovery
    // announce to A; the backbone port is the endpoint A will auto-connect to.
    let b_storage = tempfile::tempdir().expect("tempdir b");
    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_server(bootstrap_addr)
        .add_tcp_server(backbone_addr)
        .storage_path(b_storage.path().to_path_buf())
        .build()
        .await
        .expect("build b");
    node_b.start().await.expect("start b");

    // Node A: auto-connect enabled, bootstrap client to B.
    let a_storage = tempfile::tempdir().expect("tempdir a");
    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .autoconnect_discovered_interfaces(4)
        .add_tcp_client(bootstrap_addr)
        .storage_path(a_storage.path().to_path_buf())
        .build()
        .await
        .expect("build a");
    node_a.start().await.expect("start a");

    // Let the bootstrap link peer before B announces itself.
    let bootstrap_up = wait_until(Duration::from_secs(10), || {
        node_a
            .interface_stats()
            .iter()
            .any(|i| i.online && !i.is_local_client)
    })
    .await;
    assert!(bootstrap_up, "bootstrap A->B link did not come online");

    // B emits a discovery announce advertising its backbone endpoint. The
    // descriptor carries reachable_on/port; the announce app_data is the 32a
    // wire format (PoW-stamped msgpack). `transport = true` so it passes the
    // only-transport auto-connect filter.
    let disco_identity = Identity::generate(&mut rand_core::OsRng);
    let disco_dest = Destination::new(
        Some(disco_identity),
        Direction::In,
        DestinationType::Single,
        "rnstransport",
        &["discovery", "interface"],
    )
    .expect("discovery destination");
    let disco_hash = *disco_dest.hash();
    node_b.register_destination(disco_dest);

    let descriptor = InterfaceDescriptor {
        interface_type: "BackboneInterface".to_string(),
        name: Some("B Backbone".to_string()),
        reachable_on: Some("127.0.0.1".to_string()),
        port: Some(backbone_port as u64),
        ..Default::default()
    };
    let transport_id = [0xB0u8; 16];
    let app_data = build_announce_app_data(&descriptor, &transport_id, true, &mut rand_core::OsRng)
        .expect("build discovery announce app_data");

    node_b
        .announce_destination(&disco_hash, Some(&app_data))
        .await
        .expect("announce discovery record");

    // A should persist the record, spawn a TCP client to B's backbone port,
    // and register it. The auto-connected interface is named `autoconnect/*`.
    let auto_connected = wait_until(Duration::from_secs(20), || {
        node_a
            .interface_stats()
            .iter()
            .any(|i| i.name.starts_with("autoconnect/"))
    })
    .await;
    assert!(
        auto_connected,
        "A did not auto-connect a discovered interface; interfaces = {:?}",
        node_a
            .interface_stats()
            .iter()
            .map(|i| i.name.clone())
            .collect::<Vec<_>>()
    );

    // Traffic crosses the auto-established link: B re-announces so the accepted
    // connection carries protocol bytes back to A's auto-connected interface.
    node_b
        .announce_destination(&disco_hash, Some(&app_data))
        .await
        .expect("re-announce over auto link");

    let carried_traffic = wait_until(Duration::from_secs(20), || {
        node_a
            .interface_stats()
            .iter()
            .any(|i| i.name.starts_with("autoconnect/") && i.online && i.rx_bytes > 0)
    })
    .await;
    assert!(
        carried_traffic,
        "auto-connected interface carried no traffic; interfaces = {:?}",
        node_a.interface_stats()
    );

    let _ = node_a.stop().await;
    let _ = node_b.stop().await;
}

// ===========================================================================
// Codeberg #151: discovered peers get their IFAC, and fail closed when they
// cannot.
// ===========================================================================

/// Unix wall-clock seconds, like the driver's `now_unix_secs`.
fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Seed `storage` with a persisted discovered-interface record advertising a
/// TCP endpoint at `127.0.0.1:port`, optionally carrying IFAC netname/netkey.
/// Writing the record directly (instead of hearing an announce) keeps the test
/// deterministic and interface-free: the auto-connect poll reads it from disk.
fn seed_discovered_record(
    storage: &std::path::Path,
    port: u16,
    ifac_netname: Option<&str>,
    ifac_netkey: Option<&str>,
    seed: u8,
) {
    let di = DiscoveredInterface {
        interface_type: "TCPServerInterface".to_string(),
        transport: true,
        name: format!("Seeded-{seed}"),
        transport_id: [seed; 16],
        network_id: [seed; 16],
        value: 20,
        stamp: [seed; 32],
        latitude: None,
        longitude: None,
        height: None,
        reachable_on: Some("127.0.0.1".to_string()),
        port: Some(port as u64),
        frequency: None,
        bandwidth: None,
        spreadingfactor: None,
        codingrate: None,
        ifac_netname: ifac_netname.map(str::to_string),
        ifac_netkey: ifac_netkey.map(str::to_string),
        discovery_hash: [seed; 32],
    };
    let now = now_unix();
    let rec = DiscoveredInterfaceRecord::from_discovered(&di, 1, now, now, now, 0);
    let dir = storage.join("discovery").join("interfaces");
    std::fs::create_dir_all(&dir).expect("create discovery dir");
    let mut name = String::new();
    for b in di.discovery_hash {
        name.push_str(&format!("{b:02x}"));
    }
    std::fs::write(dir.join(name), rec.encode_msgpack()).expect("write record");
}

const IFAC_NETNAME: &str = "closednet-151";
const IFAC_NETKEY: &str = "closedkey-151";

/// #151 case 1, end to end: the discovery record advertises IFAC
/// netname/netkey, node B's advertised TCP server runs the same IFAC, and node
/// A (no IFAC configured anywhere) auto-connects. A must derive the advertised
/// IFAC for the spawned client, otherwise B drops every packet A sends and A
/// drops every (masked) announce B sends -- so A learning a path to B's probe
/// destination proves the spawned client authenticates in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advertised_ifac_record_connects_under_that_ifac() {
    let server_port = next_port();
    let server_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse().unwrap();

    // Node B: an IFAC-protected TCP server, built through the production
    // config path (network_name/passphrase -> build_ifac_config).
    let mut b_config = Config::default();
    b_config.interfaces.insert(
        "Protected Server".to_string(),
        InterfaceConfig {
            interface_type: "TCPServerInterface".to_string(),
            listen_ip: Some("127.0.0.1".to_string()),
            listen_port: Some(server_port),
            networkname: Some(IFAC_NETNAME.to_string()),
            passphrase: Some(IFAC_NETKEY.to_string()),
            ..Default::default()
        },
    );
    let _ = server_addr;
    let b_storage = tempfile::tempdir().expect("tempdir b");
    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .config(b_config)
        .storage_path(b_storage.path().to_path_buf())
        .build()
        .await
        .expect("build b");
    node_b.start().await.expect("start b");

    // B's probe destination; its announce is the traffic A must authenticate.
    let probe_identity = Identity::generate(&mut rand_core::OsRng);
    let probe_dest = Destination::new(
        Some(probe_identity),
        Direction::In,
        DestinationType::Single,
        "test151",
        &["probe"],
    )
    .expect("probe destination");
    let probe_hash = *probe_dest.hash();
    node_b.register_destination(probe_dest);

    // Node A: auto-connect enabled, NO interfaces, NO IFAC configured. The
    // pre-seeded record is its only knowledge of B.
    let a_storage = tempfile::tempdir().expect("tempdir a");
    seed_discovered_record(
        a_storage.path(),
        server_port,
        Some(IFAC_NETNAME),
        Some(IFAC_NETKEY),
        0x51,
    );
    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .autoconnect_discovered_interfaces(4)
        .storage_path(a_storage.path().to_path_buf())
        .build()
        .await
        .expect("build a");
    node_a.start().await.expect("start a");

    // A spawns the auto-connect client from the seeded record.
    let auto_connected = wait_until(Duration::from_secs(15), || {
        node_a
            .interface_stats()
            .iter()
            .any(|i| i.name.starts_with("autoconnect/"))
    })
    .await;
    assert!(
        auto_connected,
        "A did not auto-connect the seeded record; interfaces = {:?}",
        node_a.interface_stats()
    );

    // B announces its probe destination until A has authenticated the masked
    // announce and learned the path. Without the advertised IFAC on the
    // spawned client this never happens: B's announces fail A's IFAC check
    // (and A's own packets are dropped by B).
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut path_learned = false;
    while Instant::now() < deadline {
        node_b
            .announce_destination(&probe_hash, None)
            .await
            .expect("announce probe");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if node_a.has_path(&probe_hash) {
            path_learned = true;
            break;
        }
    }
    assert!(
        path_learned,
        "A never learned B's probe path over the IFAC-protected auto-connect \
         link; the spawned client is not authenticating (Codeberg #151)"
    );

    let _ = node_a.stop().await;
    let _ = node_b.stop().await;
}

/// #151 case 2, end to end, through the production hearing path: the discovery
/// record advertises NO IFAC material, but A hears the announce over its
/// IFAC-protected bootstrap client, so the spawned auto-connect client must
/// inherit the hearing interface's IFAC (the `AutoInterface.py:559-561`
/// parent-child rule). This exercises the real insert side of the inherit rule
/// (`record_discovery_announce` -> heard-IFAC map), which the seeded-record
/// tests bypass.
///
/// Proof of authentication: after the bootstrap link is removed, a probe
/// destination registered on B afterwards can reach A only over the
/// auto-connected link, and B's backbone server drops unauthenticated traffic
/// in both directions. Pre-#151 this test is red twice over: without the
/// inherit rule an IFAC-running A refuses the endpoint entirely, and without
/// any IFAC on the spawned client the path is never learned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn autoconnect_inherits_ifac_of_hearing_interface() {
    let bootstrap_port = next_port();
    let backbone_port = next_port();

    // Node B: bootstrap + backbone TCP servers, both under the operator's
    // IFAC (one closed network), built through the production config path.
    let mut b_config = Config::default();
    for (name, port) in [("Bootstrap", bootstrap_port), ("Backbone", backbone_port)] {
        b_config.interfaces.insert(
            name.to_string(),
            InterfaceConfig {
                interface_type: "TCPServerInterface".to_string(),
                listen_ip: Some("127.0.0.1".to_string()),
                listen_port: Some(port),
                networkname: Some(IFAC_NETNAME.to_string()),
                passphrase: Some(IFAC_NETKEY.to_string()),
                ..Default::default()
            },
        );
    }
    let b_storage = tempfile::tempdir().expect("tempdir b");
    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .config(b_config)
        .storage_path(b_storage.path().to_path_buf())
        .build()
        .await
        .expect("build b");
    node_b.start().await.expect("start b");

    // Node A: an IFAC'd bootstrap client to B (the hearing interface) and
    // auto-connect enabled. No seeded records: A learns of the backbone only
    // by hearing B's announce over the protected bootstrap link.
    let mut a_config = Config::default();
    a_config.interfaces.insert(
        "Bootstrap Client".to_string(),
        InterfaceConfig {
            interface_type: "TCPClientInterface".to_string(),
            target_host: Some("127.0.0.1".to_string()),
            target_port: Some(bootstrap_port),
            networkname: Some(IFAC_NETNAME.to_string()),
            passphrase: Some(IFAC_NETKEY.to_string()),
            ..Default::default()
        },
    );
    let a_storage = tempfile::tempdir().expect("tempdir a");
    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .autoconnect_discovered_interfaces(4)
        .config(a_config)
        .storage_path(a_storage.path().to_path_buf())
        .build()
        .await
        .expect("build a");
    node_a.start().await.expect("start a");

    let bootstrap_up = wait_until(Duration::from_secs(10), || {
        node_a
            .interface_stats()
            .iter()
            .any(|i| i.online && !i.is_local_client)
    })
    .await;
    assert!(bootstrap_up, "bootstrap A->B link did not come online");
    let bootstrap_id = node_a
        .interface_stats()
        .iter()
        .find(|i| !i.is_local_client && !i.name.starts_with("autoconnect/"))
        .map(|i| i.interface_id)
        .expect("bootstrap interface present");

    // B advertises its backbone endpoint. The descriptor carries NO IFAC
    // fields — exactly what our own advertisement path publishes
    // (discovery.rs sets ifac_netname: None) — so only inheritance can
    // protect the spawned client.
    let disco_identity = Identity::generate(&mut rand_core::OsRng);
    let disco_dest = Destination::new(
        Some(disco_identity),
        Direction::In,
        DestinationType::Single,
        "rnstransport",
        &["discovery", "interface"],
    )
    .expect("discovery destination");
    let disco_hash = *disco_dest.hash();
    node_b.register_destination(disco_dest);
    let descriptor = InterfaceDescriptor {
        interface_type: "TCPServerInterface".to_string(),
        name: Some("B Protected Backbone".to_string()),
        reachable_on: Some("127.0.0.1".to_string()),
        port: Some(backbone_port as u64),
        ..Default::default()
    };
    let transport_id = [0xB1u8; 16];
    let app_data = build_announce_app_data(&descriptor, &transport_id, true, &mut rand_core::OsRng)
        .expect("build discovery announce app_data");

    // Announce until A has heard it (over the IFAC'd bootstrap) and spawned
    // the auto-connect client. An IFAC-running A without the inherit rule
    // refuses the endpoint here (fail closed), so this wait itself is the
    // first red assertion pre-#151.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut auto_connected = false;
    while Instant::now() < deadline {
        node_b
            .announce_destination(&disco_hash, Some(&app_data))
            .await
            .expect("announce discovery record");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if node_a
            .interface_stats()
            .iter()
            .any(|i| i.name.starts_with("autoconnect/") && i.online)
        {
            auto_connected = true;
            break;
        }
    }
    assert!(
        auto_connected,
        "A did not auto-connect the heard endpoint under inherited IFAC; \
         interfaces = {:?}",
        node_a.interface_stats()
    );

    // Remove the bootstrap link; from here on, only the auto-connected link
    // remains, and it only carries traffic if it inherited the IFAC.
    node_a
        .remove_interface(bootstrap_id)
        .expect("remove bootstrap interface");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let probe_identity = Identity::generate(&mut rand_core::OsRng);
    let probe_dest = Destination::new(
        Some(probe_identity),
        Direction::In,
        DestinationType::Single,
        "test151",
        &["inherit", "probe"],
    )
    .expect("probe destination");
    let probe_hash = *probe_dest.hash();
    node_b.register_destination(probe_dest);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut path_learned = false;
    while Instant::now() < deadline {
        node_b
            .announce_destination(&probe_hash, None)
            .await
            .expect("announce probe");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if node_a.has_path(&probe_hash) {
            path_learned = true;
            break;
        }
    }
    assert!(
        path_learned,
        "A never learned B's probe path over the auto-connected link; the \
         spawned client did not inherit the hearing interface's IFAC \
         (Codeberg #151)"
    );

    let _ = node_a.stop().await;
    let _ = node_b.stop().await;
}

/// #151 case 3, end to end: this node runs IFAC (an operator-configured UDP
/// interface), the seeded record offers no IFAC material, and no hearing
/// interface can be inherited from (the record was read from disk). The
/// auto-connect must fail closed: no `autoconnect/*` interface may appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ifac_node_refuses_open_discovered_endpoint() {
    let udp_listen = next_port();
    let udp_forward = next_port();
    let target_port = next_port(); // nothing listens; must not even be dialed

    let mut config = Config::default();
    config.interfaces.insert(
        "Protected UDP".to_string(),
        InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            listen_ip: Some("127.0.0.1".to_string()),
            listen_port: Some(udp_listen),
            forward_ip: Some("127.0.0.1".to_string()),
            forward_port: Some(udp_forward),
            networkname: Some(IFAC_NETNAME.to_string()),
            passphrase: Some(IFAC_NETKEY.to_string()),
            ..Default::default()
        },
    );

    let storage = tempfile::tempdir().expect("tempdir");
    seed_discovered_record(storage.path(), target_port, None, None, 0x52);
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .autoconnect_discovered_interfaces(4)
        .config(config)
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build node");
    node.start().await.expect("start node");

    // Several auto-connect polls (1 s interval) worth of settling time: the
    // record is live and auto-connectable, so pre-#151 the spawner would have
    // registered an `autoconnect/*` interface on the first poll.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let spawned: Vec<String> = node
        .interface_stats()
        .iter()
        .map(|i| i.name.clone())
        .filter(|n| n.starts_with("autoconnect/"))
        .collect();
    assert!(
        spawned.is_empty(),
        "IFAC-running node must NOT auto-connect an endpoint with no \
         resolvable IFAC (fail closed, Codeberg #151); spawned = {spawned:?}"
    );

    let _ = node.stop().await;
}
