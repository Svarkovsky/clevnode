//! Shared instance interop tests.
//!
//! Tests that a Rust client can connect to a Python daemon's shared instance
//! Unix socket and exchange HDLC-framed packets. Also tests end-to-end link
//! establishment through a Rust daemon's shared instance.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};
use leviculum_core::packet::Packet;
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::NodeEvent;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationType, Direction};

use crate::common::{
    build_announce_raw, extract_signing_key, init_tracing, parse_dest_hash, wait_for_data_event,
    wait_for_event, wait_for_responder_established_link, DAEMON_PROCESS_TIME,
};
use crate::harness::{find_available_ports, TestDaemon};

/// Connect to an abstract Unix socket.
///
/// Python's shared instance listens on `\0rns/{instance_name}`.
fn connect_abstract_unix(instance_name: &str) -> std::io::Result<tokio::net::UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    let abstract_name = format!("rns/{}", instance_name);
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes())?;
    let std_stream = std::os::unix::net::UnixStream::connect_addr(&addr)?;
    std_stream.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(std_stream)
}

/// Start a shared instance daemon with a unique instance name to avoid
/// conflicts with parallel tests and any real system daemon.
async fn start_shared_daemon() -> (TestDaemon, String) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let instance_name = format!(
        "test_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let daemon = TestDaemon::start_with_shared_instance(&instance_name)
        .await
        .expect("Failed to start shared instance daemon");
    (daemon, instance_name)
}

/// Test: Python daemon announces a destination → announce arrives on the
/// Rust client's Unix socket (HDLC-framed).
#[tokio::test]
async fn test_shared_instance_receive_announce() {
    init_tracing();

    let (daemon, instance_name) = start_shared_daemon().await;

    // Register a destination on the Python daemon
    let dest_info = daemon
        .register_destination("sharedtest", &["announce"])
        .await
        .expect("Failed to register destination");

    // Connect Rust client to the daemon's shared instance socket
    let mut client =
        connect_abstract_unix(&instance_name).expect("Failed to connect to shared instance socket");

    // Give the connection time to be accepted and registered
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Tell Python to announce
    daemon
        .announce_destination(&dest_info.hash, b"")
        .await
        .expect("Failed to announce");

    // Read HDLC-framed packets from the Unix socket
    // The daemon should forward the announce to local clients
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 4096];
    let mut received_announce = false;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), client.read(&mut buf)).await {
            Ok(Ok(0)) => break, // EOF
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for r in results {
                    if let DeframeResult::Frame(data) = r {
                        // Try to parse as a packet
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == leviculum_core::packet::PacketType::Announce
                            {
                                // Check if dest hash matches
                                let pkt_hash_hex = hex::encode(pkt.destination_hash);
                                if pkt_hash_hex == dest_info.hash {
                                    received_announce = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                if received_announce {
                    break;
                }
            }
            Ok(Err(e)) => {
                panic!("Read error on Unix socket: {}", e);
            }
            Err(_) => continue, // timeout, try again
        }
    }

    assert!(
        received_announce,
        "Should have received the Python announce on the Unix socket"
    );
}

/// Test: Rust sends an announce over the Unix socket → Python daemon
/// creates a path for it.
#[tokio::test]
async fn test_shared_instance_send_announce() {
    init_tracing();

    let (daemon, instance_name) = start_shared_daemon().await;

    // Connect Rust client to the daemon's shared instance socket
    let mut client =
        connect_abstract_unix(&instance_name).expect("Failed to connect to shared instance socket");

    // Give the connection time to be accepted
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Build an announce packet
    let (raw, dest_hash, _dest) = build_announce_raw("rustclient", &["sharedtest"], b"hello");

    // Send HDLC-framed announce over the Unix socket
    let mut framed = Vec::new();
    frame(&raw, &mut framed);
    client
        .write_all(&framed)
        .await
        .expect("Failed to write announce");
    client.flush().await.expect("Failed to flush");

    // Wait for Python daemon to process the announce. Local client announces
    // go through Transport.inbound which may queue the announce for rebroadcast
    // before updating the path table. Allow ample time.
    let mut has_path = false;
    for _ in 0..10 {
        tokio::time::sleep(DAEMON_PROCESS_TIME).await;
        if daemon.has_path(dest_hash.as_bytes()).await {
            has_path = true;
            break;
        }
    }
    assert!(
        has_path,
        "Python daemon should have a path after receiving announce via Unix socket"
    );
}

/// Test: Full link establishment through a Rust daemon's shared instance.
///
/// Topology:
/// ```text
/// Rust node A ── TCP ── Rust daemon (in-process) ── Unix socket ── Python (shared instance client)
/// (initiator)           (transport, TCP server,                     (responder, registers dest,
///                        share_instance=true)                        announces, accepts links)
/// ```
///
/// This exercises the full pipeline: announce forwarding from local client to
/// network, link request routing from network to local client, proof routing
/// from local client to network, and bidirectional data exchange over the
/// established link.
#[tokio::test]
async fn test_shared_instance_link_through_daemon() {
    init_tracing();

    // Phase 1: Port allocation
    let (ports, _port_alloc) = find_available_ports::<3>()
        .await
        .expect("Failed to allocate ports");
    let [daemon_tcp_port, py_rns_port, py_cmd_port] = ports;
    let instance_name = format!("linktest_{}", std::process::id());

    // Phase 2: Start Rust daemon (in-process)
    // The Rust daemon owns the abstract Unix socket and has a TCP server
    // for Rust node A to connect to.
    let daemon_tcp_addr: SocketAddr = format!("127.0.0.1:{}", daemon_tcp_port).parse().unwrap();

    let _storage1 =
        crate::common::temp_storage("test_shared_instance_link_through_daemon", "node1");
    let mut daemon_node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .share_instance(true)
        .instance_name(instance_name.clone())
        .add_tcp_server(daemon_tcp_addr)
        .storage_path(_storage1.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust daemon node");
    daemon_node
        .start()
        .await
        .expect("Failed to start Rust daemon node");

    // Wait for Unix socket listener to be ready
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 3: Start Python as shared instance client
    // Python detects the existing socket and falls back to LocalClientInterface.
    // Its TCP server is NOT created (Python skips external interfaces when
    // is_connected_to_shared_instance=True). JSON-RPC command port works
    // independently.
    let py_client =
        TestDaemon::start_with_shared_instance_echo(py_rns_port, py_cmd_port, &instance_name)
            .await
            .expect("Failed to start Python shared instance client");

    // Phase 4: Register destination and announce on Python
    let dest_info = py_client
        .register_destination("sharedlinktest", &["echo"])
        .await
        .expect("Failed to register destination");

    py_client
        .announce_destination(&dest_info.hash, b"link-test")
        .await
        .expect("Failed to announce destination");

    let dest_hash = parse_dest_hash(&dest_info.hash);
    let signing_key = extract_signing_key(&dest_info.public_key);

    // Phase 5: Start Rust node A, wait for announce
    // Rust node A connects to the daemon's TCP server as a non-transport endpoint.
    let _storage2 =
        crate::common::temp_storage("test_shared_instance_link_through_daemon", "node2");
    let mut rust_a = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon_tcp_addr)
        .storage_path(_storage2.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust node A");

    let mut event_rx = rust_a
        .take_event_receiver()
        .expect("Event receiver should be available");
    rust_a.start().await.expect("Failed to start Rust node A");

    // Wait for announce to propagate: Python → Unix → daemon → TCP → Rust A
    assert!(
        crate::common::wait_for_path_reannounce(
            || rust_a.has_path(&dest_hash),
            &py_client,
            &dest_info.hash,
            b"link-test",
            Duration::from_secs(15),
        )
        .await,
        "Rust A should learn path to Python destination through daemon"
    );

    // Phase 6: Establish link
    // Link request: Rust A → TCP → daemon → Unix socket → Python
    // Link proof:   Python → Unix socket → daemon → TCP → Rust A
    let mut link_handle = rust_a
        .connect(&dest_hash, &signing_key)
        .await
        .expect("Failed to initiate link");

    let link_id = *link_handle.link_id();

    let established = wait_for_event(
        &mut event_rx,
        Duration::from_secs(15),
        |event| match event {
            NodeEvent::LinkEstablished {
                link_id: id,
                is_initiator,
                ..
            } if id == link_id && is_initiator => Some(()),
            _ => None,
        },
    )
    .await;
    assert!(
        established.is_some(),
        "Link should be established within 15s"
    );

    // Verify Python also shows the link
    let mut py_has_link = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(links) = py_client.get_links().await {
            if !links.is_empty() {
                py_has_link = true;
                break;
            }
        }
    }
    assert!(py_has_link, "Python should have an established link");

    // Phase 7: Bidirectional data exchange
    // Rust A → Python (via Channel)
    link_handle
        .try_send(b"hello-from-rust")
        .await
        .expect("Failed to send channel message");

    // Poll Python for received data
    let mut rust_to_py_ok = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(packets) = py_client.get_received_packets().await {
            for p in &packets {
                if String::from_utf8_lossy(&p.data).contains("hello-from-rust") {
                    rust_to_py_ok = true;
                    break;
                }
            }
        }
        if rust_to_py_ok {
            break;
        }
    }
    assert!(
        rust_to_py_ok,
        "Python should receive channel message from Rust A"
    );

    // Python → Rust A (echo via Channel. Python echoes the channel message back)
    let echo = wait_for_data_event(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(echo.is_some(), "Should receive echo from Python");
    assert_eq!(
        echo.unwrap(),
        b"hello-from-rust",
        "Echo should match sent data"
    );

    // Python → Rust A (raw packet via send_on_link)
    let link_hash = {
        let links = py_client.get_links().await.expect("Failed to get links");
        links.keys().next().expect("Should have a link").clone()
    };

    py_client
        .send_on_link(&link_hash, b"hello-from-python")
        .await
        .expect("Python send_on_link should succeed");

    let raw_data = wait_for_data_event(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(raw_data.is_some(), "Should receive raw data from Python");
    assert_eq!(
        raw_data.unwrap(),
        b"hello-from-python",
        "Raw data from Python should match"
    );

    // Cleanup
    link_handle.close().await.expect("Failed to close link");
    rust_a.stop().await.expect("Failed to stop Rust node A");
    daemon_node
        .stop()
        .await
        .expect("Failed to stop Rust daemon");
}

/// Test: Python local client initiates a link through the Rust daemon to a
/// Python remote node. Verifies Codeberg issue #24 behaviors:
/// - Link request flows: Python A (Unix) → daemon → Python B (TCP)
/// - Link proof flows back: Python B (TCP) → daemon → Python A (Unix)
/// - Bidirectional data exchange works through the daemon
///
/// Topology:
/// ```text
/// Python A (shared instance client) ── Unix ── Rust daemon ── TCP ── Python B (responder)
/// (initiator, creates link)            (transport, share_instance)    (registers dest, announces)
/// ```
#[tokio::test]
async fn test_local_client_initiates_link_through_daemon() {
    init_tracing();

    // Phase 1: Port allocation
    let (ports, _port_alloc) = find_available_ports::<4>()
        .await
        .expect("Failed to allocate ports");
    let [daemon_tcp_port, py_b_rns_port, py_b_cmd_port, py_a_cmd_port] = ports;
    let instance_name = format!("lclink_{}", std::process::id());

    // Phase 2: Start Python B (remote responder, TCP server)
    let py_b = TestDaemon::start_with_ports(py_b_rns_port, py_b_cmd_port)
        .await
        .expect("Failed to start Python B");

    let dest_info = py_b
        .register_destination("sharedlinktest", &["echo"])
        .await
        .expect("Failed to register destination on Python B");

    // Phase 3: Start Rust daemon (shared instance + TCP client to B)
    let py_b_addr: SocketAddr = format!("127.0.0.1:{}", py_b_rns_port).parse().unwrap();
    let daemon_tcp_addr: SocketAddr = format!("127.0.0.1:{}", daemon_tcp_port).parse().unwrap();

    let _storage =
        crate::common::temp_storage("test_local_client_initiates_link_through_daemon", "daemon");
    let mut daemon_node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .share_instance(true)
        .instance_name(instance_name.clone())
        .add_tcp_server(daemon_tcp_addr)
        .add_tcp_client(py_b_addr)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust daemon");
    daemon_node
        .start()
        .await
        .expect("Failed to start Rust daemon");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Phase 4: Start Python A (shared instance client, initiator)
    let (py_a_ports, _py_a_port_alloc) = find_available_ports::<2>().await.expect("ports");
    let py_a_rns_port = py_a_ports[0];
    let py_a =
        TestDaemon::start_with_shared_instance_ports(py_a_rns_port, py_a_cmd_port, &instance_name)
            .await
            .expect("Failed to start Python A as shared instance client");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Phase 5: Announce AFTER all nodes are connected
    // Python B announces now so the daemon sees it and forwards to Python A.
    py_b.announce_destination(&dest_info.hash, b"issue24-link-test")
        .await
        .expect("Failed to announce on Python B");

    // Announce flow: Python B → TCP → daemon → Unix → Python A
    let dest_hash = parse_dest_hash(&dest_info.hash);
    assert!(
        crate::common::wait_for_path_reannounce_on_daemon(
            &py_a,
            &dest_hash,
            &py_b,
            &dest_info.hash,
            b"issue24-link-test",
            Duration::from_secs(15),
        )
        .await,
        "Python A should discover Python B's destination through the daemon"
    );

    // Phase 6: Python A creates link to Python B
    // Link request: Python A → Unix → daemon → TCP → Python B
    // Link proof:   Python B → TCP → daemon → Unix → Python A
    let link_hash = py_a
        .create_link(&dest_info.hash, &dest_info.public_key, 15)
        .await
        .expect("Python A should establish link to Python B through daemon");

    // Verify Python B also has the link
    let mut b_has_link = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(links) = py_b.get_links().await {
            if !links.is_empty() {
                b_has_link = true;
                break;
            }
        }
    }
    assert!(
        b_has_link,
        "Python B should have the established link from Python A"
    );

    // Phase 7: Bidirectional data exchange
    // Python A → Python B
    py_a.send_on_link(&link_hash, b"hello-from-local-client")
        .await
        .expect("Failed to send data from Python A");

    let mut a_to_b_ok = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(packets) = py_b.get_received_packets().await {
            if packets
                .iter()
                .any(|p| String::from_utf8_lossy(&p.data).contains("hello-from-local-client"))
            {
                a_to_b_ok = true;
                break;
            }
        }
    }
    assert!(
        a_to_b_ok,
        "Python B should receive data from Python A through daemon"
    );

    // Python B → Python A
    let b_link_hash = {
        let links = py_b.get_links().await.expect("get_links");
        links.keys().next().expect("link").clone()
    };
    py_b.send_on_link(&b_link_hash, b"hello-from-remote")
        .await
        .expect("Failed to send data from Python B");

    let mut b_to_a_ok = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(packets) = py_a.get_received_packets().await {
            if packets
                .iter()
                .any(|p| String::from_utf8_lossy(&p.data).contains("hello-from-remote"))
            {
                b_to_a_ok = true;
                break;
            }
        }
    }
    assert!(
        b_to_a_ok,
        "Python A should receive data from Python B through daemon"
    );
}

/// Test: Non-transport Rust daemon still relays link traffic for shared
/// instance clients. The `from_local` / `for_local` / `for_local_link` gates
/// bypass the `enable_transport` check (Python Transport.py:1404).
///
/// Topology:
/// ```text
/// Python A (shared instance client) ── Unix ── Rust daemon ── TCP ── Python B (responder)
/// (initiator, creates link)            (NO TRANSPORT!)                (registers dest, announces)
/// ```
#[tokio::test]
async fn test_non_transport_daemon_relays_local_client_link() {
    init_tracing();

    // Phase 1: Port allocation
    let (ports, _port_alloc) = find_available_ports::<4>()
        .await
        .expect("Failed to allocate ports");
    let [daemon_tcp_port, py_b_rns_port, py_b_cmd_port, py_a_cmd_port] = ports;
    let instance_name = format!("notransport_{}", std::process::id());

    // Phase 2: Start Python B (remote responder, TCP server)
    let py_b = TestDaemon::start_with_ports(py_b_rns_port, py_b_cmd_port)
        .await
        .expect("Failed to start Python B");

    let dest_info = py_b
        .register_destination("notransporttest", &["echo"])
        .await
        .expect("Failed to register destination on Python B");

    // Phase 3: Start Rust daemon, transport DISABLED
    let py_b_addr: SocketAddr = format!("127.0.0.1:{}", py_b_rns_port).parse().unwrap();
    let daemon_tcp_addr: SocketAddr = format!("127.0.0.1:{}", daemon_tcp_port).parse().unwrap();

    let _storage = crate::common::temp_storage(
        "test_non_transport_daemon_relays_local_client_link",
        "daemon",
    );
    let mut daemon_node = ReticulumNodeBuilder::new()
        .enable_transport(false) // <── KEY DIFFERENCE: transport disabled
        .share_instance(true)
        .instance_name(instance_name.clone())
        .add_tcp_server(daemon_tcp_addr)
        .add_tcp_client(py_b_addr)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust daemon (no transport)");
    daemon_node
        .start()
        .await
        .expect("Failed to start Rust daemon (no transport)");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Phase 4: Start Python A (shared instance client)
    let (py_a_ports, _py_a_port_alloc) = find_available_ports::<2>().await.expect("ports");
    let py_a_rns_port = py_a_ports[0];
    let py_a =
        TestDaemon::start_with_shared_instance_ports(py_a_rns_port, py_a_cmd_port, &instance_name)
            .await
            .expect("Failed to start Python A");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Phase 5: Announce AFTER all nodes are connected
    // Announce forwarding to local clients is NOT gated on enable_transport.
    py_b.announce_destination(&dest_info.hash, b"no-transport-test")
        .await
        .expect("Failed to announce on Python B");
    let dest_hash = parse_dest_hash(&dest_info.hash);
    assert!(
        crate::common::wait_for_path_reannounce_on_daemon(
            &py_a,
            &dest_hash,
            &py_b,
            &dest_info.hash,
            b"no-transport-test",
            Duration::from_secs(15),
        )
        .await,
        "Python A should discover Python B even with non-transport daemon"
    );

    // Phase 6: Python A creates link to Python B
    // from_local gate allows link request routing without transport.
    // for_local_link gate allows proof routing back without transport.
    let link_hash = py_a
        .create_link(&dest_info.hash, &dest_info.public_key, 15)
        .await
        .expect("Link should establish through non-transport daemon");

    // Phase 7: Bidirectional data exchange
    py_a.send_on_link(&link_hash, b"through-non-transport")
        .await
        .expect("Failed to send from Python A");

    let mut a_to_b_ok = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(packets) = py_b.get_received_packets().await {
            if packets
                .iter()
                .any(|p| String::from_utf8_lossy(&p.data).contains("through-non-transport"))
            {
                a_to_b_ok = true;
                break;
            }
        }
    }
    assert!(
        a_to_b_ok,
        "Data should flow through non-transport daemon: A → daemon → B"
    );

    let b_link_hash = {
        let links = py_b.get_links().await.expect("get_links");
        links.keys().next().expect("link").clone()
    };
    py_b.send_on_link(&b_link_hash, b"reply-through-non-transport")
        .await
        .expect("Failed to send from Python B");

    let mut b_to_a_ok = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(packets) = py_a.get_received_packets().await {
            if packets
                .iter()
                .any(|p| String::from_utf8_lossy(&p.data).contains("reply-through-non-transport"))
            {
                b_to_a_ok = true;
                break;
            }
        }
    }
    assert!(
        b_to_a_ok,
        "Data should flow through non-transport daemon: B → daemon → A"
    );
}

/// Test: Full link establishment through a Rust daemon's shared instance
/// using `connect_to_shared_instance()` builder API (no raw socket code).
///
/// Topology:
/// ```text
/// Rust node A ── TCP ── Rust daemon (in-process) ── Unix socket ── Rust client B (builder)
/// (initiator)           (transport, TCP server,                     (responder, registers dest,
///                        share_instance=true)                        announces, accepts links)
/// ```
#[tokio::test]
async fn test_local_client_builder_link_through_daemon() {
    init_tracing();

    // Phase 1: Port allocation
    let (ports, _port_alloc) = find_available_ports::<2>()
        .await
        .expect("Failed to allocate ports");
    let [daemon_tcp_port, _spare] = ports;
    let instance_name = format!("clientbuilder_{}", std::process::id());

    // Each node needs a unique storage path to avoid identity collisions
    let tmp = std::env::temp_dir().join(format!("leviculum_si_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let daemon_storage = tmp.join("daemon");
    let client_b_storage = tmp.join("client_b");
    let rust_a_storage = tmp.join("rust_a");

    // Phase 2: Start Rust daemon (in-process)
    let daemon_tcp_addr: SocketAddr = format!("127.0.0.1:{}", daemon_tcp_port).parse().unwrap();

    let mut daemon_node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .share_instance(true)
        .instance_name(instance_name.clone())
        .add_tcp_server(daemon_tcp_addr)
        .storage_path(daemon_storage)
        .build()
        .await
        .expect("Failed to build Rust daemon node");
    daemon_node
        .start()
        .await
        .expect("Failed to start Rust daemon node");

    // Wait for Unix socket listener to be ready
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 3: Build Rust client B via connect_to_shared_instance
    let mut client_b = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .connect_to_shared_instance(&instance_name)
        .storage_path(client_b_storage)
        .build()
        .await
        .expect("Failed to build Rust client B");

    let mut client_b_events = client_b
        .take_event_receiver()
        .expect("Event receiver should be available");
    client_b
        .start()
        .await
        .expect("Failed to start Rust client B");

    // Phase 4: Register destination and announce on client B
    let client_b_identity = Identity::generate(&mut rand_core::OsRng);
    let client_b_signing_key = {
        let pk = client_b_identity.public_key_bytes();
        let mut key = [0u8; 32];
        key.copy_from_slice(&pk[32..64]);
        key
    };
    let mut dest = Destination::new(
        Some(client_b_identity),
        Direction::In,
        DestinationType::Single,
        "localclienttest",
        &["echo"],
    )
    .expect("Failed to create destination");
    dest.set_accepts_links(true);

    let dest_hash = *dest.hash();
    client_b.register_destination(dest);
    client_b
        .announce_destination(&dest_hash, Some(b"client-b-test"))
        .await
        .expect("Failed to announce destination on client B");

    // Phase 5: Start Rust node A, wait for path
    let mut rust_a = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon_tcp_addr)
        .storage_path(rust_a_storage)
        .build()
        .await
        .expect("Failed to build Rust node A");

    let mut rust_a_events = rust_a
        .take_event_receiver()
        .expect("Event receiver should be available");
    rust_a.start().await.expect("Failed to start Rust node A");

    assert!(
        crate::common::wait_for_node_reannounce(
            || rust_a.has_path(&dest_hash),
            &client_b,
            &dest_hash,
            b"client-b-test",
            Duration::from_secs(15),
        )
        .await,
        "Rust A should learn path to client B's destination through daemon"
    );

    // Phase 6: Establish link
    // connect() sends the link request; client B auto-accepts and proves it.
    let mut link_handle = rust_a
        .connect(&dest_hash, &client_b_signing_key)
        .await
        .expect("Failed to initiate link");

    let link_id = *link_handle.link_id();

    // Client B's incoming link auto-establishes; learn its link_id from the
    // responder-side LinkEstablished event and mint a writable handle.
    let client_b_link_id =
        wait_for_responder_established_link(&mut client_b_events, Duration::from_secs(15))
            .await
            .expect("Client B should establish incoming link");

    let client_b_link = client_b.link_handle(&client_b_link_id);

    // Now wait for LinkEstablished on initiator side (Rust A)
    let established =
        wait_for_event(
            &mut rust_a_events,
            Duration::from_secs(15),
            |event| match event {
                NodeEvent::LinkEstablished {
                    link_id: id,
                    is_initiator,
                    ..
                } if id == link_id && is_initiator => Some(()),
                _ => None,
            },
        )
        .await;
    assert!(
        established.is_some(),
        "Link should be established within 15s"
    );

    // Phase 7: Bidirectional data exchange
    // Rust A → Client B
    link_handle
        .try_send(b"hello-from-a")
        .await
        .expect("Failed to send from A");

    let data = wait_for_data_event(
        &mut client_b_events,
        &client_b_link_id,
        Duration::from_secs(10),
    )
    .await;
    assert!(data.is_some(), "Client B should receive data from A");
    assert_eq!(data.unwrap(), b"hello-from-a");

    // Client B → Rust A
    client_b_link
        .try_send(b"hello-from-b")
        .await
        .expect("Failed to send from B");

    let data = wait_for_data_event(&mut rust_a_events, &link_id, Duration::from_secs(10)).await;
    assert!(data.is_some(), "Rust A should receive data from client B");
    assert_eq!(data.unwrap(), b"hello-from-b");

    // Cleanup
    link_handle.close().await.expect("Failed to close link");
    rust_a.stop().await.expect("Failed to stop Rust node A");
    client_b.stop().await.expect("Failed to stop client B");
    daemon_node
        .stop()
        .await
        .expect("Failed to stop Rust daemon");
    let _ = std::fs::remove_dir_all(&tmp);
}
