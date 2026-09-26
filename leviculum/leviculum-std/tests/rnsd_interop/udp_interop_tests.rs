//! Interop tests for UDPInterface with Python Reticulum.
//!
//! Verifies that Rust and Python can exchange announces, discover
//! paths, and establish links over a point-to-point UDP link.

use std::time::Duration;

use leviculum_std::config::{Config, InterfaceConfig};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::{Destination, DestinationType, Direction, Identity, NodeEvent};
use tokio::time::timeout;

use crate::common::{
    extract_signing_key, generate_test_payload, parse_dest_hash, wait_for_link_established,
};
use crate::harness::TestDaemon;

/// Test: Rust receives an announce from Python over a UDP link.
///
/// Setup:
/// - Python daemon has TCP (for control) + UDP (listen_port=A, forward_port=B)
/// - Rust node has UDP (listen_port=B, forward_port=A)
/// - Python registers a destination and announces it
/// - Rust should receive the AnnounceReceived event
#[tokio::test]
async fn test_rust_receives_announce_from_python_over_udp() {
    let daemon = TestDaemon::start_with_udp()
        .await
        .expect("Failed to start daemon with UDP");

    let py_listen = daemon
        .udp_listen_addr()
        .expect("daemon should have UDP listen addr");
    let rust_listen = daemon
        .udp_forward_addr()
        .expect("daemon should have UDP forward addr");

    // Rust listens where Python forwards, and forwards where Python listens
    let _storage =
        crate::common::temp_storage("test_rust_receives_announce_from_python_over_udp", "node");
    let mut node = ReticulumNodeBuilder::new()
        .add_udp_interface(rust_listen, py_listen)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");

    let mut events = node
        .take_event_receiver()
        .expect("Failed to get event receiver");

    // Wait for UDP interface to settle
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Register and announce a destination on the Python side
    let dest = daemon
        .register_destination("test", &["udp_announce"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest.hash, b"udp-test-data")
        .await
        .expect("Failed to announce");

    // Wait for AnnounceReceived event on Rust side
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, events.recv()).await {
            Ok(Some(NodeEvent::AnnounceReceived { announce, .. })) => {
                let received_hash = hex::encode(announce.destination_hash());
                if received_hash == dest.hash {
                    found = true;
                    break;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(found, "Rust should receive announce from Python over UDP");

    node.stop().await.expect("Failed to stop node");
}

/// Test: Python receives an announce from Rust over a UDP link.
///
/// Setup is the same, but the Rust node announces and Python verifies.
#[tokio::test]
async fn test_python_receives_announce_from_rust_over_udp() {
    let daemon = TestDaemon::start_with_udp()
        .await
        .expect("Failed to start daemon with UDP");

    let py_listen = daemon
        .udp_listen_addr()
        .expect("daemon should have UDP listen addr");
    let rust_listen = daemon
        .udp_forward_addr()
        .expect("daemon should have UDP forward addr");

    let _storage =
        crate::common::temp_storage("test_python_receives_announce_from_rust_over_udp", "node");
    let mut node = ReticulumNodeBuilder::new()
        .add_udp_interface(rust_listen, py_listen)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");

    // Wait for UDP interface to settle
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Register and announce a destination on the Rust side
    let identity = Identity::generate(&mut rand_core::OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "test",
        &["udp_rust_announce"],
    )
    .expect("Failed to create destination");

    let dest_hash = *dest.hash();
    node.register_destination(dest);

    node.announce_destination(&dest_hash, Some(b"rust-udp-data"))
        .await
        .expect("Failed to announce");

    // Wait for Python to process the announce and register the path
    let found = crate::common::wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &node,
        b"rust-udp-data",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        found,
        "Python should have path to Rust destination announced over UDP (hash: {})",
        hex::encode(dest_hash.as_bytes())
    );

    node.stop().await.expect("Failed to stop node");
}

/// Test: Python receives an announce from Rust when the Rust node's config
/// names the UDP peer by hostname (Codeberg #148).
///
/// This is the container case that found the gap: `forward_ip` holds a
/// service name, not an address. The same config keys work against rnsd
/// because Python passes `forward_ip` straight to `sendto`; lnsd must
/// resolve the name and deliver over the resolved address. Driven through
/// the real config path (`Config` → interface builder), not a builder
/// convenience method, so this exercises exactly what a config file does.
#[tokio::test]
async fn test_python_receives_announce_from_rust_over_udp_hostname_forward() {
    let daemon = TestDaemon::start_with_udp()
        .await
        .expect("Failed to start daemon with UDP");

    let py_listen = daemon
        .udp_listen_addr()
        .expect("daemon should have UDP listen addr");
    let rust_listen = daemon
        .udp_forward_addr()
        .expect("daemon should have UDP forward addr");

    // Same shape an rnsd-style config file produces, with the peer named
    // by hostname instead of by address.
    let mut config = Config::default();
    config.interfaces.insert(
        "udp".to_string(),
        InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            listen_ip: Some(rust_listen.ip().to_string()),
            listen_port: Some(rust_listen.port()),
            forward_ip: Some("localhost".to_string()),
            forward_port: Some(py_listen.port()),
            ..Default::default()
        },
    );

    let _storage = crate::common::temp_storage(
        "test_python_receives_announce_from_rust_over_udp_hostname_forward",
        "node",
    );
    let mut node = ReticulumNodeBuilder::new()
        .config(config)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("a hostname forward_ip must build — rnsd accepts this config");

    node.start().await.expect("Failed to start node");

    // Wait for UDP interface to settle
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Register and announce a destination on the Rust side
    let identity = Identity::generate(&mut rand_core::OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "test",
        &["udp_hostname_announce"],
    )
    .expect("Failed to create destination");

    let dest_hash = *dest.hash();
    node.register_destination(dest);

    node.announce_destination(&dest_hash, Some(b"rust-udp-hostname"))
        .await
        .expect("Failed to announce");

    let found = crate::common::wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &node,
        b"rust-udp-hostname",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        found,
        "Python should have path to Rust destination announced over a \
         hostname-configured UDP forward (hash: {})",
        hex::encode(dest_hash.as_bytes())
    );

    node.stop().await.expect("Failed to stop node");
}

/// Test: Rust initiates a link to Python over UDP, sends data, verifies receipt.
///
/// Setup:
/// - Python daemon has TCP (for control) + UDP (listen_port=A, forward_port=B)
/// - Rust node has UDP (listen_port=B, forward_port=A)
/// - Python registers a destination that accepts links
/// - Python announces the destination
/// - Rust learns the path, initiates a link, sends data
#[tokio::test]
async fn test_rust_to_python_link_over_udp() {
    let daemon = TestDaemon::start_with_udp()
        .await
        .expect("Failed to start daemon with UDP");

    let py_listen = daemon
        .udp_listen_addr()
        .expect("daemon should have UDP listen addr");
    let rust_listen = daemon
        .udp_forward_addr()
        .expect("daemon should have UDP forward addr");

    let _storage = crate::common::temp_storage("test_rust_to_python_link_over_udp", "node");
    let mut node = ReticulumNodeBuilder::new()
        .add_udp_interface(rust_listen, py_listen)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");

    let mut events = node
        .take_event_receiver()
        .expect("Failed to get event receiver");

    // Wait for UDP interface to settle
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Register a link-accepting destination on Python side
    let dest_info = daemon
        .register_destination("test", &["udp_link"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest_info.hash, b"udp-link-test")
        .await
        .expect("Failed to announce");

    // Wait for Rust to learn the path
    let dest_hash = parse_dest_hash(&dest_info.hash);
    assert!(
        crate::common::wait_for_path_reannounce(
            || node.has_path(&dest_hash),
            &daemon,
            &dest_info.hash,
            b"udp-link-test",
            Duration::from_secs(5),
        )
        .await,
        "Rust should learn path to Python destination over UDP"
    );

    // Initiate link
    let signing_key = extract_signing_key(&dest_info.public_key);
    let link_handle = node
        .connect(&dest_hash, &signing_key)
        .await
        .expect("Failed to connect");
    let link_id = *link_handle.link_id();

    // Wait for link to be established
    assert!(
        wait_for_link_established(&mut events, &link_id, Duration::from_secs(10)).await,
        "Link should be established over UDP"
    );

    // Send test payload
    let payload = generate_test_payload(128);
    link_handle.send(&payload).await.expect("Send failed");

    // Wait for Python to receive, verify via get_received_packets
    tokio::time::sleep(Duration::from_secs(1)).await;
    let packets = daemon.get_received_packets().await.unwrap_or_default();
    assert!(
        !packets.is_empty(),
        "Python daemon should receive data over UDP link"
    );

    println!("SUCCESS: Rust-to-Python link over UDP — link established, data sent");

    node.stop().await.expect("Failed to stop node");
}
