//! Packet flooding and loop prevention tests
//!
//! These tests exercise redundant network topologies where announces can
//! echo back to the originating node via multiple paths. They verify that
//! the dedup cache prevents a node from learning a path to itself.
//!
//! ## Topologies
//!
//! - **Triangle**: Rust-Node connected to Py-A and Py-B, which are interconnected
//! - **Diamond**: Rust-Node connected to Py-Entry and Py-Exit, both connected to Py-Bypass
//! - **Diamond link**: Py-Src, Py-Hub, Py-Dst with Rust-Relay as alternate path
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop flood_tests -- --nocapture
//! ```

use std::time::Duration;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::harness::TestDaemon;

// =========================================================================
// Smoke test: announce_destination() works at all
// =========================================================================

/// Smoke test: Rust node announces a destination, Python daemon learns the path.
///
/// Topology: Rust-Node -> Py-A (linear, no loops)
///
/// Verifies that `ReticulumNode::announce_destination()` correctly builds
/// and broadcasts an announce packet that a Python daemon can process.
#[tokio::test]
async fn test_announce_destination_smoke() {
    // Start one Python daemon
    let py_a = TestDaemon::start().await.expect("Failed to start daemon");

    // Build Rust node connected to daemon (transport disabled, no relay)
    let _storage = crate::common::temp_storage("test_announce_destination_smoke", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .add_tcp_client(py_a.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    rust_node.start().await.expect("Failed to start node");

    // Wait for TCP connection to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create and register a destination on the Rust node
    let identity = Identity::generate(&mut rand_core::OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "flood_test",
        &["smoke"],
    )
    .expect("Failed to create destination");

    let dest_hash = *dest.hash();
    rust_node.register_destination(dest);

    // Announce the destination
    rust_node
        .announce_destination(&dest_hash, Some(b"smoke-test"))
        .await
        .expect("announce_destination should succeed");

    // Wait for Python daemon to learn the path
    let found = crate::common::wait_for_node_reannounce_on_daemon(
        &py_a,
        &dest_hash,
        &rust_node,
        b"smoke-test",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Python daemon should learn path to Rust destination");

    // Verify hop count is 1 (direct. Python increments hops by 1 on reception)
    let paths = py_a.get_path_table().await.unwrap();
    let hex_hash = hex::encode(dest_hash.as_bytes());
    let entry = paths
        .get(&hex_hash)
        .expect("Path entry should exist in path table");
    assert_eq!(
        entry.hops,
        Some(1),
        "Direct announce should have hops=1 (Python increments on recv), got {:?}",
        entry.hops
    );

    // Clean up
    rust_node.stop().await.expect("Failed to stop node");
}

// =========================================================================
// Test 1: Triangle echo prevention
// =========================================================================

/// Test that outbound announces are added to the dedup cache, preventing
/// echoes from returning through redundant paths.
///
/// Topology:
/// ```text
/// Rust-Node <---> Py-A <---> Py-B <---> Rust-Node
///   (iface 0)                  (iface 1)
/// ```
///
/// When Rust-Node announces a destination, the announce propagates to both
/// Py-A and Py-B. Each Python daemon rebroadcasts (hops+1), and the echoes
/// return to Rust-Node via the other interface.
///
/// Expected: Rust-Node must NOT learn a path to its own destination.
#[tokio::test]
async fn test_triangle_echo_prevention() {
    // Start two Python daemons
    let py_a = TestDaemon::start().await.expect("Failed to start Py-A");
    let py_b = TestDaemon::start().await.expect("Failed to start Py-B");

    // Connect Py-B to Py-A (creates the triangle's third edge)
    py_b.add_client_interface("127.0.0.1", py_a.rns_port(), Some("LinkTo_A"))
        .await
        .expect("Failed to connect Py-B to Py-A");

    // Wait for daemon interconnection to stabilize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Build Rust node connected to BOTH daemons (two interfaces)
    let _storage = crate::common::temp_storage("test_triangle_echo_prevention", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(py_a.rns_addr())
        .add_tcp_client(py_b.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    rust_node.start().await.expect("Failed to start node");

    // Wait for TCP connections to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create and register a destination on the Rust node
    let identity = Identity::generate(&mut rand_core::OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "flood_test",
        &["triangle"],
    )
    .expect("Failed to create destination");

    let dest_hash = *dest.hash();
    let hex_hash = hex::encode(dest_hash.as_bytes());
    rust_node.register_destination(dest);

    // Announce the destination, broadcasts on both interfaces
    rust_node
        .announce_destination(&dest_hash, Some(b"triangle-test"))
        .await
        .expect("announce_destination should succeed");

    // Wait for propagation: both Python daemons should learn the path
    // (PATHFINDER_G + jitter + processing)
    let a_found = crate::common::wait_for_node_reannounce_on_daemon(
        &py_a,
        &dest_hash,
        &rust_node,
        b"triangle-test",
        Duration::from_secs(20),
    )
    .await;
    let b_found = crate::common::wait_for_node_reannounce_on_daemon(
        &py_b,
        &dest_hash,
        &rust_node,
        b"triangle-test",
        Duration::from_secs(20),
    )
    .await;

    assert!(a_found, "Py-A should learn path to Rust destination");
    assert!(b_found, "Py-B should learn path to Rust destination");

    // Verify hop counts on Python daemons (direct = hops 1, Python increments on recv)
    let pa = py_a.get_path_table().await.unwrap();
    assert_eq!(
        pa.get(&hex_hash).and_then(|e| e.hops),
        Some(1),
        "Py-A should have hops=1 (direct from Rust-Node, Python +1 on recv)"
    );

    let pb = py_b.get_path_table().await.unwrap();
    assert_eq!(
        pb.get(&hex_hash).and_then(|e| e.hops),
        Some(1),
        "Py-B should have hops=1 (direct from Rust-Node, Python +1 on recv)"
    );

    // Wait for echoes to have time to arrive and be (mis)processed.
    // Python rebroadcasts with PATHFINDER_G grace period (~2s) plus
    // 0–500ms jitter delay, and the triangle has two hops so echoes
    // can arrive with up to ~5s total delay.
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Diagnostic: dump transport stats for investigation
    let stats = rust_node.transport_stats();
    eprintln!(
        "[triangle_echo] Transport stats: recv={}, fwd={}, drop={}, ann={}",
        stats.packets_received(),
        stats.packets_forwarded(),
        stats.packets_dropped(),
        stats.announces_processed()
    );

    // CRITICAL ASSERTION: Rust-Node must NOT have a path to its own destination
    assert!(
        !rust_node.has_path(&dest_hash),
        "Node must not learn path to itself from network echo"
    );

    // Storm detection: packet count must stabilize (no ongoing receive storm)
    let stats_1 = rust_node.transport_stats();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let stats_2 = rust_node.transport_stats();
    assert_eq!(
        stats_1.packets_received(),
        stats_2.packets_received(),
        "Packet count must stabilize (no receive storm): {} vs {}",
        stats_1.packets_received(),
        stats_2.packets_received()
    );

    // Clean up
    rust_node.stop().await.expect("Failed to stop node");
}

// =========================================================================
// Test 2: Diamond originator echo
// =========================================================================

/// Test echo prevention in a diamond topology with 3 Python intermediaries.
///
/// Topology:
/// ```text
///         Rust-Node (originator)
///        /          \
///   Py-Entry      Py-Exit
///        \          /
///         Py-Bypass
/// ```
///
/// Both Test 1 and Test 2 fail for the same reason (outbound announce hash
/// not added to packet_cache) and will pass after the same fix. Test 2
/// validates the fix works with more complex redundant paths where echoes
/// can arrive via multiple intermediary chains.
#[tokio::test]
async fn test_diamond_originator_echo() {
    // Start three Python daemons
    let py_entry = TestDaemon::start().await.expect("Failed to start Py-Entry");
    let py_exit = TestDaemon::start().await.expect("Failed to start Py-Exit");
    let py_bypass = TestDaemon::start()
        .await
        .expect("Failed to start Py-Bypass");

    // Connect Py-Bypass to both Py-Entry and Py-Exit (forming the diamond bottom)
    py_bypass
        .add_client_interface("127.0.0.1", py_entry.rns_port(), Some("LinkTo_Entry"))
        .await
        .expect("Failed to connect Py-Bypass to Py-Entry");
    py_bypass
        .add_client_interface("127.0.0.1", py_exit.rns_port(), Some("LinkTo_Exit"))
        .await
        .expect("Failed to connect Py-Bypass to Py-Exit");

    // Wait for daemon interconnections to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Build Rust node connected to Py-Entry and Py-Exit (diamond top)
    let _storage = crate::common::temp_storage("test_diamond_originator_echo", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(py_entry.rns_addr())
        .add_tcp_client(py_exit.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    rust_node.start().await.expect("Failed to start node");

    // Wait for TCP connections to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create and register a destination on the Rust node
    let identity = Identity::generate(&mut rand_core::OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "flood_test",
        &["diamond"],
    )
    .expect("Failed to create destination");

    let dest_hash = *dest.hash();
    let hex_hash = hex::encode(dest_hash.as_bytes());
    rust_node.register_destination(dest);

    // Announce the destination
    rust_node
        .announce_destination(&dest_hash, Some(b"diamond-test"))
        .await
        .expect("announce_destination should succeed");

    // Wait for Entry and Exit to learn the path (direct from Rust-Node)
    let entry_found = crate::common::wait_for_node_reannounce_on_daemon(
        &py_entry,
        &dest_hash,
        &rust_node,
        b"diamond-test",
        Duration::from_secs(20),
    )
    .await;
    let exit_found = crate::common::wait_for_node_reannounce_on_daemon(
        &py_exit,
        &dest_hash,
        &rust_node,
        b"diamond-test",
        Duration::from_secs(20),
    )
    .await;

    assert!(
        entry_found,
        "Py-Entry should learn path to Rust destination"
    );
    assert!(exit_found, "Py-Exit should learn path to Rust destination");

    // Verify hop counts (Python increments hops by 1 on reception)
    let pe = py_entry.get_path_table().await.unwrap();
    assert_eq!(
        pe.get(&hex_hash).and_then(|e| e.hops),
        Some(1),
        "Py-Entry should have hops=1 (direct from Rust-Node, Python +1 on recv)"
    );

    let px = py_exit.get_path_table().await.unwrap();
    assert_eq!(
        px.get(&hex_hash).and_then(|e| e.hops),
        Some(1),
        "Py-Exit should have hops=1 (direct from Rust-Node, Python +1 on recv)"
    );

    // Wait for all 3 announce retransmits to complete (~16.5s from announce time).
    // Locally-originated announces are retransmitted 3 times for LoRa reliability
    // (transport.rs:1189-1191, Rust extension. Python does not retransmit these).
    // Each retransmit increments packets_forwarded via forward_on_all_except().
    // After retries exhaust (retries > PATHFINDER_RETRIES), the entry is removed
    // and no further forwarding occurs.
    tokio::time::sleep(Duration::from_secs(25)).await;

    // Diagnostic: dump transport stats for investigation
    let stats = rust_node.transport_stats();
    eprintln!(
        "[diamond_echo] Transport stats: recv={}, fwd={}, drop={}, ann={}",
        stats.packets_received(),
        stats.packets_forwarded(),
        stats.packets_dropped(),
        stats.announces_processed()
    );

    // CRITICAL ASSERTION: Rust-Node must NOT have a path to its own destination
    // Even with 3 Python intermediaries creating multiple echo paths,
    // the dedup cache should prevent self-path learning.
    assert!(
        !rust_node.has_path(&dest_hash),
        "Node must not learn path to itself from diamond echo"
    );

    // Storm detection
    let stats_1 = rust_node.transport_stats();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let stats_2 = rust_node.transport_stats();
    assert_eq!(
        stats_1.packets_received(),
        stats_2.packets_received(),
        "No receive storm after propagation: {} vs {}",
        stats_1.packets_received(),
        stats_2.packets_received()
    );
    assert_eq!(
        stats_1.packets_forwarded(),
        stats_2.packets_forwarded(),
        "No forwarding storm after propagation: {} vs {}",
        stats_1.packets_forwarded(),
        stats_2.packets_forwarded()
    );

    // Clean up
    rust_node.stop().await.expect("Failed to stop node");
}

// =========================================================================
// Test 3: Diamond link with redundant paths
// =========================================================================

/// Test that link establishment and data delivery work correctly in a
/// diamond topology with redundant paths.
///
/// Topology:
/// ```text
///   Py-Src <-> Py-Hub <-> Py-Dst
///      \                   /
///       <-- Rust-Relay -->
/// ```
///
/// Two paths between Py-Src and Py-Dst: via Py-Hub (2 hops) and via
/// Rust-Relay (2 hops).
///
/// Verifies that link data crosses the diamond correctly, with dedup
/// preventing duplicate delivery of the same packet via both paths.
///
/// The test daemon echoes every received packet (creating a new
/// RNS.Packet with fresh encryption/hash each time), which causes echo
/// ping-pong between Py-Src and Py-Dst. The assertion checks that the
/// payload was delivered at least once, ignoring echo-generated copies.
#[tokio::test]
async fn test_diamond_link_redundant_paths() {
    // Start three Python daemons
    let py_hub = TestDaemon::start().await.expect("Failed to start Py-Hub");
    let py_src = TestDaemon::start().await.expect("Failed to start Py-Src");
    let py_dst = TestDaemon::start().await.expect("Failed to start Py-Dst");

    // Connect Py-Src and Py-Dst to Py-Hub
    py_src
        .add_client_interface("127.0.0.1", py_hub.rns_port(), Some("LinkTo_Hub"))
        .await
        .expect("Failed to connect Py-Src to Py-Hub");
    py_dst
        .add_client_interface("127.0.0.1", py_hub.rns_port(), Some("LinkTo_Hub"))
        .await
        .expect("Failed to connect Py-Dst to Py-Hub");

    // Wait for daemon interconnections to stabilize (longer for complex topology)
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Build Rust relay connected to Py-Src and Py-Dst (alternate path)
    let _storage = crate::common::temp_storage("test_diamond_link_redundant_paths", "node");
    let mut rust_relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(py_src.rns_addr())
        .add_tcp_client(py_dst.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build relay node");

    rust_relay.start().await.expect("Failed to start relay");

    // Wait for TCP connections to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Register destination on Py-Dst and announce it
    let dest_info = py_dst
        .register_destination("flood_test", &["diamond_link"])
        .await
        .expect("Failed to register destination on Py-Dst");

    let dest_hash_bytes: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_info.hash).unwrap().try_into().unwrap();
    let dest_hash = DestinationHash::new(dest_hash_bytes);

    py_dst
        .announce_destination(&dest_info.hash, b"diamond-link-test")
        .await
        .expect("Failed to announce destination");

    // Wait for Py-Src to learn the path (via Hub or Rust-Relay)
    let src_found = crate::common::wait_for_path_reannounce_on_daemon(
        &py_src,
        &dest_hash,
        &py_dst,
        &dest_info.hash,
        b"diamond-link-test",
        Duration::from_secs(20),
    )
    .await;
    assert!(src_found, "Py-Src should learn path to Py-Dst destination");

    // Verify Rust relay also has the path
    // Give relay a moment to process the announce
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        rust_relay.has_path(&dest_hash),
        "Rust relay should have path to Py-Dst destination"
    );

    // Py-Hub should also have the path
    assert!(
        py_hub.has_path(&dest_hash).await,
        "Py-Hub should have path to Py-Dst destination"
    );

    // Create link from Py-Src to Py-Dst (create_link waits for ACTIVE internally)
    let link_hash = py_src
        .create_link(&dest_info.hash, &dest_info.public_key, 30)
        .await
        .expect("Link creation must succeed (create_link waits for ACTIVE)");

    // Send data over the link
    let test_data = b"flood-test-payload-exact-bytes";
    py_src
        .send_on_link(&link_hash, test_data)
        .await
        .expect("Send must succeed");

    // Wait for data delivery
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify data was received (echo ping-pong will create many copies;
    // we only care that the original message was delivered at least once).
    //
    // The test daemon echoes every received packet with a fresh RNS.Packet
    // (new IV = new hash), so dedup cannot stop echoes. At LAN latency,
    // Py-Src and Py-Dst ping-pong echoes for the full sleep duration.
    // This is the same behavior documented in rust_relay_tests.rs.
    let received = py_dst
        .get_received_packets()
        .await
        .expect("Should get received packets");

    let matching: Vec<_> = received.iter().filter(|p| p.data == test_data).collect();

    // Diagnostic: log packet counts for investigation
    eprintln!(
        "[diamond_link] Py-Dst received {} total packets, {} matching payload",
        received.len(),
        matching.len()
    );

    let relay_stats = rust_relay.transport_stats();
    eprintln!(
        "[diamond_link] Rust-Relay stats: recv={}, fwd={}, drop={}, ann={}",
        relay_stats.packets_received(),
        relay_stats.packets_forwarded(),
        relay_stats.packets_dropped(),
        relay_stats.announces_processed()
    );

    assert!(
        !matching.is_empty(),
        "Payload should be received at least once, got 0 out of {} total packets",
        received.len()
    );

    // Storm detection: measure whether packet rate is stabilizing
    // (non-asserting, collect data for future analysis)
    let stats_1 = rust_relay.transport_stats();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let stats_2 = rust_relay.transport_stats();
    let delta = stats_2
        .packets_received()
        .saturating_sub(stats_1.packets_received());
    eprintln!(
        "[diamond_link] Packet delta over 5s: {} (recv {} -> {})",
        delta,
        stats_1.packets_received(),
        stats_2.packets_received()
    );

    // Clean up
    rust_relay.stop().await.expect("Failed to stop relay");
}
