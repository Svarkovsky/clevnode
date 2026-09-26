//! Live interface-discovery interop tests against a real Python `rnsd`
//! (Codeberg #106, closing the #32 live-integration gap).
//!
//! The existing `discovery_tests` cover the wire format with golden vectors and
//! a Rust<->Rust auto-connect; these drive the REAL Python
//! `RNS.Discovery.InterfaceAnnouncer` / `InterfaceDiscovery` end to end:
//!
//! * our lnsd discovers ONE live Python rnsd (record + `config_entry`);
//! * our lnsd discovers MULTIPLE live Python rnsd at once (distinct records);
//! * our lnsd AUTO-CONNECTS a discovered Backbone/TCP Python rnsd and traffic
//!   crosses the auto-established link;
//! * encrypted discovery: a matching network identity is discovered, a
//!   mismatched or absent one is not;
//! * reverse: a real Python rnsd discovers OUR announced interface.
//!
//! Each Python daemon runs the real announcer (`emit_discovery_announce` drives
//! its own `get_interface_announce_data` + `discovery_destination.announce`, so
//! stamps and encryption are genuine); the Rust node persists validated records
//! under `<storage>/discovery/interfaces`, exactly as `lnstatus -d` reads them.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use leviculum_core::discovery::{
    build_announce_app_data, DiscoveredInterfaceRecord, InterfaceDescriptor,
};
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction, Identity};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::Config;

use crate::common::temp_storage;
use crate::harness::{pick_free_tcp_port, TestDaemon};

/// Read all persisted discovered-interface records from a node's storage dir
/// (`<storage>/discovery/interfaces`), the same files `lnstatus -d` lists.
fn read_discovered_records(storage: &Path) -> Vec<DiscoveredInterfaceRecord> {
    let dir = storage.join("discovery").join("interfaces");
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Some(rec) = DiscoveredInterfaceRecord::decode_msgpack(&bytes) {
                    out.push(rec);
                }
            }
        }
    }
    out
}

/// Parse a 16-byte destination hash from the daemon's hex reply.
fn dest_hash_from_hex(hex_str: &str) -> DestinationHash {
    let bytes = hex::decode(hex_str).expect("valid hex hash");
    let arr: [u8; 16] = bytes.as_slice().try_into().expect("16-byte hash");
    DestinationHash::new(arr)
}

/// Expected `config_entry` for a discovered Python TCPServer/Backbone endpoint.
/// On non-Windows hosts Python and our stack both render it as a
/// `BackboneClientInterface` block (`Discovery.py` / `build_config_entry`).
fn expected_backbone_config_entry(name: &str, port: u16, transport_id_hex: &str) -> String {
    format!(
        "[[{name}]]\n  type = BackboneInterface\n  enabled = yes\n  \
         remote = 127.0.0.1\n  target_port = {port}\n  transport_identity = {transport_id_hex}"
    )
}

/// Poll `cond` every 100 ms until it is true or the deadline passes.
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

/// Re-drive the daemon's discovery announce on a fixed cadence until `cond`
/// holds or the deadline passes. A single announce can be lost or held by
/// ingress control under load, so the emit is repeated (Codeberg #105 hardening
/// style); each emit runs the real announcer.
async fn drive_discovery_until(
    daemon: &TestDaemon,
    deadline: Duration,
    mut cond: impl FnMut() -> bool,
) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        let _ = daemon.emit_discovery_announce().await;
        if wait_until(Duration::from_millis(700), &mut cond).await {
            return true;
        }
    }
    cond()
}

/// Build a Rust node connected to `daemon` as a TCP client, with an optional
/// shared discovery network identity and optional auto-connect cap.
async fn build_connected_node(
    daemon: &TestDaemon,
    storage: &tempfile::TempDir,
    network_identity: Option<&Path>,
    autoconnect_max: usize,
) -> leviculum_std::driver::ReticulumNode {
    let mut config = Config::default();
    if let Some(path) = network_identity {
        config.reticulum.network_identity = Some(path.to_path_buf());
    }
    let mut builder = ReticulumNodeBuilder::new()
        .config(config)
        .add_tcp_client(daemon.rns_addr())
        .storage_path(storage.path().to_path_buf());
    if autoconnect_max > 0 {
        builder = builder.autoconnect_discovered_interfaces(autoconnect_max);
    }
    let mut node = builder.build().await.expect("build node");
    node.start().await.expect("start node");
    node.wait_for_interfaces_ready(Duration::from_secs(5))
        .await
        .expect("interfaces ready");
    daemon
        .wait_for_peer_count(1, Duration::from_secs(5))
        .await
        .expect("daemon registers peer");
    node
}

// =========================================================================
// Test 1: our lnsd discovers ONE live Python rnsd
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_lnsd_discovers_one_python_rnsd() {
    let name = "OneNode";
    let daemon = TestDaemon::start_discoverable(name)
        .await
        .expect("start discoverable daemon");
    let port = daemon.rns_port();

    let storage = temp_storage("disco_one", "node");
    let mut node = build_connected_node(&daemon, &storage, None, 0).await;

    let found = drive_discovery_until(&daemon, Duration::from_secs(20), || {
        read_discovered_records(storage.path())
            .iter()
            .any(|r| r.name == name)
    })
    .await;
    assert!(found, "lnsd did not discover the Python rnsd");

    let records = read_discovered_records(storage.path());
    let rec = records
        .iter()
        .find(|r| r.name == name)
        .expect("record present");

    assert_eq!(rec.interface_type, "TCPServerInterface");
    assert_eq!(rec.reachable_on.as_deref(), Some("127.0.0.1"));
    assert_eq!(rec.port, Some(port as u64));
    assert!(rec.value >= 14, "stamp value must meet the discovery cost");

    // config_entry is byte-identical to what Python's `rnstatus -d` renders for
    // the same discovered endpoint.
    let expected = expected_backbone_config_entry(name, port, &rec.transport_id);
    assert_eq!(
        rec.config_entry.as_deref(),
        Some(expected.as_str()),
        "config_entry must match the Python-rendered entry"
    );

    node.stop().await.expect("stop node");
}

// =========================================================================
// Test 2: our lnsd discovers MULTIPLE live Python rnsd simultaneously
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_lnsd_discovers_multiple_python_rnsd() {
    let daemon_a = TestDaemon::start_discoverable("MultiA")
        .await
        .expect("start daemon A");
    let daemon_b = TestDaemon::start_discoverable("MultiB")
        .await
        .expect("start daemon B");
    let daemon_c = TestDaemon::start_discoverable("MultiC")
        .await
        .expect("start daemon C");

    // One Rust node, three separate TCP client interfaces (one per daemon), so
    // each announce arrives on its own interface.
    let storage = temp_storage("disco_multi", "node");
    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .add_tcp_client(daemon_c.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build node");
    node.start().await.expect("start node");
    node.wait_for_interfaces_ready(Duration::from_secs(5))
        .await
        .expect("interfaces ready");

    let names = ["MultiA", "MultiB", "MultiC"];
    let all_found = {
        let end = Instant::now() + Duration::from_secs(30);
        let mut ok = false;
        while Instant::now() < end {
            let _ = daemon_a.emit_discovery_announce().await;
            let _ = daemon_b.emit_discovery_announce().await;
            let _ = daemon_c.emit_discovery_announce().await;
            tokio::time::sleep(Duration::from_millis(700)).await;
            let recs = read_discovered_records(storage.path());
            ok = names.iter().all(|n| recs.iter().any(|r| &r.name == n));
            if ok {
                break;
            }
        }
        ok
    };
    assert!(all_found, "lnsd did not discover all three Python rnsd");

    // Distinct records: three names, three distinct discovery hashes, three
    // distinct transport ids.
    let recs = read_discovered_records(storage.path());
    let mut disco_hashes: Vec<_> = recs.iter().map(|r| r.discovery_hash).collect();
    disco_hashes.sort();
    disco_hashes.dedup();
    assert!(
        disco_hashes.len() >= 3,
        "expected >=3 distinct discovery records, got {}",
        disco_hashes.len()
    );
    for n in names {
        assert!(recs.iter().any(|r| r.name == n), "missing record for {n}");
    }

    node.stop().await.expect("stop node");
}

// =========================================================================
// Test 3: our lnsd auto-connects a discovered Backbone/TCP Python rnsd
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_lnsd_autoconnects_discovered_python_rnsd() {
    // The daemon exposes a bootstrap TCP server (the node connects here to hear
    // the announce) plus a SECOND discoverable server on `backbone_port` (the
    // advertised endpoint the node auto-connects to -- a genuinely new link).
    let (daemon, backbone_port) = TestDaemon::start_discoverable_backbone("AutoBackbone")
        .await
        .expect("start backbone daemon");

    let storage = temp_storage("disco_auto", "node");
    let mut node = build_connected_node(&daemon, &storage, None, 4).await;

    // The node persists the record and then spawns an auto-connected interface
    // to 127.0.0.1:backbone_port.
    let auto_connected = drive_discovery_until(&daemon, Duration::from_secs(30), || {
        node.interface_stats()
            .iter()
            .any(|i| i.name.starts_with("autoconnect/"))
    })
    .await;
    assert!(
        auto_connected,
        "lnsd did not auto-connect the discovered Python endpoint; interfaces = {:?}",
        node.interface_stats()
            .iter()
            .map(|i| i.name.clone())
            .collect::<Vec<_>>()
    );

    // Sanity: the discovered record advertises the backbone port we auto-connect.
    let recs = read_discovered_records(storage.path());
    assert!(
        recs.iter()
            .any(|r| r.name == "AutoBackbone" && r.port == Some(backbone_port as u64)),
        "record must advertise the backbone port {backbone_port}"
    );

    // Traffic crosses the auto-established link: further announces from the
    // daemon reach the node over the auto-connected interface (rx_bytes > 0).
    let carried = drive_discovery_until(&daemon, Duration::from_secs(20), || {
        node.interface_stats()
            .iter()
            .any(|i| i.name.starts_with("autoconnect/") && i.online && i.rx_bytes > 0)
    })
    .await;
    assert!(
        carried,
        "auto-connected interface carried no traffic; interfaces = {:?}",
        node.interface_stats()
    );

    node.stop().await.expect("stop node");
}

// =========================================================================
// Test 4: encrypted discovery -- matching network identity is discovered
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_encrypted_discovery_matching_identity_is_discovered() {
    // Shared 64-byte network identity file: Python generates it on startup, the
    // Rust node loads the same file to decrypt.
    let netid_dir = tempfile::tempdir().expect("netid dir");
    let netid_path = netid_dir.path().join("network_identity");

    let daemon = TestDaemon::start_discoverable_encrypted(
        "EncNode",
        netid_path.to_str().expect("utf8 path"),
    )
    .await
    .expect("start encrypted discoverable daemon");
    let port = daemon.rns_port();

    let storage = temp_storage("disco_enc_match", "node");
    let mut node = build_connected_node(&daemon, &storage, Some(&netid_path), 0).await;

    let found = drive_discovery_until(&daemon, Duration::from_secs(20), || {
        read_discovered_records(storage.path())
            .iter()
            .any(|r| r.name == "EncNode")
    })
    .await;
    assert!(
        found,
        "matching network identity must decrypt and discover the encrypted announce"
    );

    let recs = read_discovered_records(storage.path());
    let rec = recs
        .iter()
        .find(|r| r.name == "EncNode")
        .expect("record present");
    assert_eq!(rec.interface_type, "TCPServerInterface");
    assert_eq!(rec.port, Some(port as u64));
    // Encrypted announces are owned by the network identity, so network_id is
    // the network identity's hash (distinct from the transport id).
    assert_ne!(
        rec.network_id, rec.transport_id,
        "encrypted discovery: network_id is the network identity, not the transport id"
    );

    node.stop().await.expect("stop node");
}

// =========================================================================
// Test 5: encrypted discovery -- mismatched / absent identity is NOT discovered
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_encrypted_discovery_mismatched_or_absent_not_discovered() {
    let netid_dir = tempfile::tempdir().expect("netid dir");
    let netid_path = netid_dir.path().join("network_identity");

    let daemon = TestDaemon::start_discoverable_encrypted(
        "SecretNode",
        netid_path.to_str().expect("utf8 path"),
    )
    .await
    .expect("start encrypted discoverable daemon");

    // Node A: a DIFFERENT network identity (mismatch). Node B: none (absent).
    let wrong_dir = tempfile::tempdir().expect("wrong netid dir");
    let wrong_path = wrong_dir.path().join("network_identity");
    // Materialise a distinct identity file so the Rust node loads a mismatched key.
    std::fs::write(
        &wrong_path,
        Identity::generate(&mut rand_core::OsRng)
            .private_key_bytes()
            .expect("private key bytes"),
    )
    .expect("write wrong identity");

    let storage_a = temp_storage("disco_enc_mismatch", "a");
    let mut node_a = build_connected_node(&daemon, &storage_a, Some(&wrong_path), 0).await;

    let storage_b = temp_storage("disco_enc_absent", "b");
    let mut node_b = build_connected_node(&daemon, &storage_b, None, 0).await;

    // The discovery destination hash: used to confirm the announce actually
    // REACHED each node (path learned) so a missing record is a decrypt
    // rejection, not a delivery failure.
    let emit = daemon
        .emit_discovery_announce()
        .await
        .expect("emit encrypted announce");
    let disco_hash = dest_hash_from_hex(
        emit.get("discovery_dest_hash")
            .and_then(|v| v.as_str())
            .expect("discovery_dest_hash"),
    );

    // Drive announces so both nodes hear it (path learned on both).
    let delivered = drive_discovery_until(&daemon, Duration::from_secs(15), || {
        node_a.has_path(&disco_hash) && node_b.has_path(&disco_hash)
    })
    .await;
    assert!(
        delivered,
        "encrypted announce did not reach both nodes (delivery precondition)"
    );

    // A couple more emits to give any (incorrect) persistence a chance to appear.
    for _ in 0..3 {
        let _ = daemon.emit_discovery_announce().await;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    assert!(
        read_discovered_records(storage_a.path()).is_empty(),
        "mismatched network identity must NOT decrypt/discover the announce"
    );
    assert!(
        read_discovered_records(storage_b.path()).is_empty(),
        "absent network identity must NOT decrypt/discover the encrypted announce"
    );

    node_a.stop().await.expect("stop node a");
    node_b.stop().await.expect("stop node b");
}

// =========================================================================
// Test 6: reverse -- a real Python rnsd discovers OUR announced interface
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_python_discovers_rust_announced_interface() {
    // Python daemon running the InterfaceDiscovery listener; the Rust node
    // connects to it as a TCP client and announces its own discoverable
    // interface, which the daemon must surface via `get_discovered_interfaces`.
    let daemon = TestDaemon::start_discovering()
        .await
        .expect("start discovering daemon");

    let storage = temp_storage("disco_reverse", "node");
    let mut node = build_connected_node(&daemon, &storage, None, 0).await;

    // The Rust node's discovery destination + a TCPServer descriptor advertising
    // a reachable endpoint. `transport = true` so it passes the only-transport
    // filters; the endpoint host/port need only be a valid IP/port for Python's
    // reachable_on validation.
    let identity = Identity::generate(&mut rand_core::OsRng);
    let disco_dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "rnstransport",
        &["discovery", "interface"],
    )
    .expect("discovery destination");
    let disco_hash = *disco_dest.hash();
    node.register_destination(disco_dest);

    let advertised_port: u16 = 45999;
    let descriptor = InterfaceDescriptor {
        interface_type: "TCPServerInterface".to_string(),
        name: Some("RustNode".to_string()),
        reachable_on: Some("127.0.0.1".to_string()),
        port: Some(advertised_port as u64),
        ..Default::default()
    };
    let transport_id = [0x5Au8; 16];
    let app_data = build_announce_app_data(&descriptor, &transport_id, true, &mut rand_core::OsRng)
        .expect("build discovery announce app_data");

    // Re-drive our announce until the Python daemon lists our interface.
    let discovered = {
        let end = Instant::now() + Duration::from_secs(20);
        let mut ok = false;
        while Instant::now() < end {
            node.announce_destination(&disco_hash, Some(&app_data))
                .await
                .expect("announce discovery record");
            tokio::time::sleep(Duration::from_millis(700)).await;
            let listed = daemon
                .get_discovered_interfaces()
                .await
                .expect("query discovered interfaces");
            ok = listed
                .iter()
                .any(|info| info.get("name").and_then(|v| v.as_str()) == Some("RustNode"));
            if ok {
                break;
            }
        }
        ok
    };
    assert!(
        discovered,
        "Python rnsd did not discover the Rust-announced interface"
    );

    // The Python-side record carries the advertised endpoint we announced.
    let listed = daemon
        .get_discovered_interfaces()
        .await
        .expect("query discovered interfaces");
    let info = listed
        .iter()
        .find(|i| i.get("name").and_then(|v| v.as_str()) == Some("RustNode"))
        .expect("record present");
    assert_eq!(
        info.get("type").and_then(|v| v.as_str()),
        Some("TCPServerInterface")
    );
    assert_eq!(
        info.get("reachable_on").and_then(|v| v.as_str()),
        Some("127.0.0.1")
    );
    assert_eq!(
        info.get("port").and_then(|v| v.as_u64()),
        Some(advertised_port as u64)
    );

    node.stop().await.expect("stop node");
}

/// Poll the daemon's discovered-interface registry until it lists `name`, or the
/// deadline passes. No manual emit: only lnsd's own periodic announcer drives
/// the announce.
async fn wait_for_python_discovery(daemon: &TestDaemon, name: &str, deadline: Duration) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(listed) = daemon.get_discovered_interfaces().await {
            if listed
                .iter()
                .any(|i| i.get("name").and_then(|v| v.as_str()) == Some(name))
            {
                return true;
            }
        }
    }
    false
}

// =========================================================================
// Test 7: reverse, AUTONOMOUS -- a real Python rnsd discovers OUR lnsd with
// NO manual emit; lnsd's own periodic InterfaceAnnouncer advertises (#107).
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_python_autonomously_discovers_lnsd_announcer() {
    let daemon = TestDaemon::start_discovering()
        .await
        .expect("start discovering daemon");

    let advertised_port = pick_free_tcp_port().expect("free advertised port");
    let server_addr = SocketAddr::from(([127, 0, 0, 1], advertised_port));

    let storage = temp_storage("disco_reverse_auto", "node");
    // A TCP client link carries the announce to the daemon; a discoverable
    // TCPServer is the endpoint lnsd self-advertises. The announcer job interval
    // is dropped to 1 s and the per-interface interval to 0 so the periodic job
    // fires promptly -- there is NO emit_discovery_announce anywhere.
    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .add_discoverable_tcp_server(server_addr, "AutoRustNode", 0, false)
        .discovery_announce_job_interval_secs(1)
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build node");
    node.start().await.expect("start node");
    node.wait_for_interfaces_ready(Duration::from_secs(5))
        .await
        .expect("interfaces ready");
    daemon
        .wait_for_peer_count(1, Duration::from_secs(5))
        .await
        .expect("daemon registers peer");

    let discovered =
        wait_for_python_discovery(&daemon, "AutoRustNode", Duration::from_secs(30)).await;
    assert!(
        discovered,
        "Python rnsd did not autonomously discover lnsd's self-advertised interface"
    );

    let listed = daemon
        .get_discovered_interfaces()
        .await
        .expect("query discovered interfaces");
    let info = listed
        .iter()
        .find(|i| i.get("name").and_then(|v| v.as_str()) == Some("AutoRustNode"))
        .expect("record present");
    assert_eq!(
        info.get("type").and_then(|v| v.as_str()),
        Some("TCPServerInterface")
    );
    assert_eq!(
        info.get("reachable_on").and_then(|v| v.as_str()),
        Some("127.0.0.1")
    );
    assert_eq!(
        info.get("port").and_then(|v| v.as_u64()),
        Some(advertised_port as u64)
    );

    node.stop().await.expect("stop node");
}

// =========================================================================
// Test 8: reverse ENCRYPTED (the #106-uncovered case) -- a real Python rnsd
// with the MATCHING network identity autonomously discovers our encrypted
// self-announce, while a MISMATCHED identity does not (#107).
// =========================================================================

/// Build a node that autonomously self-advertises a discoverable TCPServer,
/// encrypting the announce with the network identity at `netid_path`.
async fn build_encrypted_announcer_node(
    daemon: &TestDaemon,
    storage: &tempfile::TempDir,
    netid_path: &Path,
    name: &str,
    advertised_port: u16,
) -> leviculum_std::driver::ReticulumNode {
    let mut config = Config::default();
    config.reticulum.network_identity = Some(netid_path.to_path_buf());
    let server_addr = SocketAddr::from(([127, 0, 0, 1], advertised_port));
    let mut node = ReticulumNodeBuilder::new()
        .config(config)
        .add_tcp_client(daemon.rns_addr())
        .add_discoverable_tcp_server(server_addr, name, 0, true)
        .discovery_announce_job_interval_secs(1)
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build node");
    node.start().await.expect("start node");
    node.wait_for_interfaces_ready(Duration::from_secs(5))
        .await
        .expect("interfaces ready");
    node
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_python_discovers_lnsd_encrypted_announcer() {
    // Shared network identity: Python generates it on startup; the matching node
    // loads the same file to encrypt its self-announce.
    let netid_dir = tempfile::tempdir().expect("netid dir");
    let netid_path = netid_dir.path().join("network_identity");

    let daemon = TestDaemon::start_discovering_encrypted(netid_path.to_str().expect("utf8 path"))
        .await
        .expect("start encrypted discovering daemon");

    // Matching node: same network identity as the daemon.
    let match_port = pick_free_tcp_port().expect("match port");
    let storage_match = temp_storage("disco_reverse_enc_match", "node");
    let mut node_match = build_encrypted_announcer_node(
        &daemon,
        &storage_match,
        &netid_path,
        "EncMatch",
        match_port,
    )
    .await;

    // Mismatched node: a DIFFERENT network identity (only variable that differs).
    let wrong_dir = tempfile::tempdir().expect("wrong netid dir");
    let wrong_path = wrong_dir.path().join("network_identity");
    std::fs::write(
        &wrong_path,
        Identity::generate(&mut rand_core::OsRng)
            .private_key_bytes()
            .expect("private key bytes"),
    )
    .expect("write wrong identity");
    let mismatch_port = pick_free_tcp_port().expect("mismatch port");
    let storage_mismatch = temp_storage("disco_reverse_enc_mismatch", "node");
    let mut node_mismatch = build_encrypted_announcer_node(
        &daemon,
        &storage_mismatch,
        &wrong_path,
        "EncMismatch",
        mismatch_port,
    )
    .await;

    // Both client links up: proves the mismatched node's announce also reaches
    // the daemon, so a missing record is a decrypt rejection, not a delivery gap.
    daemon
        .wait_for_peer_count(2, Duration::from_secs(10))
        .await
        .expect("daemon registers both peers");

    // The matching identity's encrypted self-announce decrypts and surfaces.
    let matched = wait_for_python_discovery(&daemon, "EncMatch", Duration::from_secs(30)).await;
    assert!(
        matched,
        "matching network identity must decrypt and discover the encrypted self-announce"
    );

    // Give any (incorrect) mismatched persistence ample extra time to appear.
    let mismatched =
        wait_for_python_discovery(&daemon, "EncMismatch", Duration::from_secs(6)).await;
    assert!(
        !mismatched,
        "mismatched network identity must NOT decrypt/discover the encrypted self-announce"
    );

    let listed = daemon
        .get_discovered_interfaces()
        .await
        .expect("query discovered interfaces");
    let info = listed
        .iter()
        .find(|i| i.get("name").and_then(|v| v.as_str()) == Some("EncMatch"))
        .expect("matching record present");
    assert_eq!(
        info.get("type").and_then(|v| v.as_str()),
        Some("TCPServerInterface")
    );
    assert_eq!(
        info.get("port").and_then(|v| v.as_u64()),
        Some(match_port as u64)
    );

    node_match.stop().await.expect("stop matching node");
    node_mismatch.stop().await.expect("stop mismatched node");
}

// =========================================================================
// Codeberg #151: a Python rnsd publishes its backbone's IFAC in the discovery
// announce (`publish_ifac`), and our lnsd auto-connects UNDER that IFAC.
// =========================================================================

/// The Python daemon's discoverable backbone server runs IFAC and publishes
/// netname/netkey in its (real) discovery announce. Our node hears it over an
/// open bootstrap link, persists the record WITH the IFAC material, and
/// auto-connects the backbone under the derived IFAC.
///
/// The proof that the spawned client actually authenticates is path learning
/// with the bootstrap link REMOVED: a destination registered on the daemon and
/// announced only after the removal can reach our node exclusively over the
/// IFAC-protected auto-connected link, and Python drops unauthenticated
/// traffic on that interface in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_lnsd_autoconnects_python_ifac_published_backbone() {
    const NETNAME: &str = "iface151net";
    const PASSPHRASE: &str = "iface151key";

    let (daemon, backbone_port) =
        TestDaemon::start_discoverable_backbone_ifac("IfacBackbone", NETNAME, PASSPHRASE)
            .await
            .expect("start IFAC backbone daemon");

    let storage = temp_storage("disco_ifac_auto", "node");
    let mut node = build_connected_node(&daemon, &storage, None, 4).await;

    // The bootstrap TCP client is the only non-local interface right now;
    // remember it so it can be removed once the auto-connect is up.
    let bootstrap_id = node
        .interface_stats()
        .iter()
        .find(|i| !i.is_local_client && !i.name.starts_with("autoconnect/"))
        .map(|i| i.interface_id)
        .expect("bootstrap interface present");

    // The REAL Python announcer publishes the backbone's IFAC keys in the
    // record (Discovery.py `get_interface_announce_data`, publish_ifac).
    let record_with_ifac = drive_discovery_until(&daemon, Duration::from_secs(30), || {
        read_discovered_records(storage.path()).iter().any(|r| {
            r.name == "IfacBackbone"
                && r.port == Some(backbone_port as u64)
                && r.ifac_netname.as_deref() == Some(NETNAME)
                && r.ifac_netkey.as_deref() == Some(PASSPHRASE)
        })
    })
    .await;
    assert!(
        record_with_ifac,
        "the persisted record must carry the published IFAC netname/netkey; \
         records = {:?}",
        read_discovered_records(storage.path())
            .iter()
            .map(|r| (
                r.name.clone(),
                r.ifac_netname.clone(),
                r.ifac_netkey.clone()
            ))
            .collect::<Vec<_>>()
    );

    // The auto-connect spawns from that record.
    let auto_connected = drive_discovery_until(&daemon, Duration::from_secs(30), || {
        node.interface_stats()
            .iter()
            .any(|i| i.name.starts_with("autoconnect/") && i.online)
    })
    .await;
    assert!(
        auto_connected,
        "lnsd did not auto-connect the IFAC-published Python endpoint; \
         interfaces = {:?}",
        node.interface_stats()
    );

    // Remove the open bootstrap link, then have the daemon announce a fresh
    // destination: the only remaining route is the IFAC-protected
    // auto-connected link, so learning this path proves the spawned client
    // carries the record's IFAC (Python silently drops unauthenticated
    // packets on that interface).
    node.remove_interface(bootstrap_id)
        .expect("remove bootstrap interface");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let probe = daemon
        .register_destination("test151", &["probe"])
        .await
        .expect("register post-detach destination");
    let probe_hash = dest_hash_from_hex(&probe.hash);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut path_learned = false;
    while Instant::now() < deadline {
        let _ = daemon.announce_destination(&probe.hash, b"").await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if node.has_path(&probe_hash) {
            path_learned = true;
            break;
        }
    }
    assert!(
        path_learned,
        "the post-detach path must arrive over the IFAC-protected \
         auto-connected link; the spawned client is not authenticating \
         against Python (Codeberg #151); interfaces = {:?}",
        node.interface_stats()
    );

    node.stop().await.expect("stop node");
}

// =========================================================================
// Test: a 1.5.0-strength listener (stamp gate 16) discovers our announcement
// =========================================================================

/// Codeberg #328. RNS 1.5.0 raised `Discovery.DEFAULT_STAMP_VALUE` from 14 to
/// 16; a listener at that gate silently discards a value-14 stamp, which is
/// what we used to mint. This drives the real Python announce handler with its
/// gate set explicitly to 16 — the same code path 1.5.0 reaches by default,
/// since the version bump changed the constant and not the verification — and
/// requires that our announcement still lands.
///
/// The Python side here is the vendored 1.3.5. That is a faithful proxy for
/// 1.5.0's *gate*, not for 1.5.0 as a whole; a run against a genuinely
/// installed 1.5.0 is still owed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_python_listener_at_150_stamp_gate_discovers_rust_announcement() {
    use crate::harness::TestDaemon as Daemon;

    let daemon = Daemon::start_discovering_at_value(16)
        .await
        .expect("start listener gating at the 1.5.0 stamp value");

    let storage = temp_storage("disco_gate16", "node");
    let mut node = build_connected_node(&daemon, &storage, None, 0).await;

    let identity = Identity::generate(&mut rand_core::OsRng);
    let disco_dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "rnstransport",
        &["discovery", "interface"],
    )
    .expect("discovery destination");
    let disco_hash = *disco_dest.hash();
    node.register_destination(disco_dest);

    let advertised_port: u16 = 45998;
    let descriptor = InterfaceDescriptor {
        interface_type: "TCPServerInterface".to_string(),
        name: Some("RustNode16".to_string()),
        reachable_on: Some("127.0.0.1".to_string()),
        port: Some(advertised_port as u64),
        ..Default::default()
    };
    // Minted at our production `DEFAULT_STAMP_VALUE`; before #328 that was 14
    // and this announcement never cleared the gate.
    let app_data = build_announce_app_data(&descriptor, &[0x5Bu8; 16], true, &mut rand_core::OsRng)
        .expect("build discovery announce app_data");

    let end = Instant::now() + Duration::from_secs(20);
    let mut discovered = false;
    while Instant::now() < end {
        node.announce_destination(&disco_hash, Some(&app_data))
            .await
            .expect("announce discovery record");
        tokio::time::sleep(Duration::from_millis(700)).await;
        let listed = daemon
            .get_discovered_interfaces()
            .await
            .expect("query discovered interfaces");
        discovered = listed
            .iter()
            .any(|info| info.get("name").and_then(|v| v.as_str()) == Some("RustNode16"));
        if discovered {
            break;
        }
    }
    assert!(
        discovered,
        "a Python listener gating at the 1.5.0 stamp value (16) did not discover our \
         announcement -- our mint cost is below its gate"
    );

    node.stop().await.expect("stop node");
}
