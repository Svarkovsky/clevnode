//! Integration tests for the high-level Node API
//!
//! These tests verify that the ReticulumNode API works correctly with
//! a real Python Reticulum daemon, mirroring the functionality shown
//! in the examples (simple_send, echo_server, chat).
//!
//! # Test Coverage
//!
//! - Node creation and startup
//! - Event reception (announces, paths)
//! - Link management
//! - Graceful shutdown

use std::time::Duration;

use tokio::time::timeout;

use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::test_support::event_log::init_event_log;
use leviculum_std::NodeEvent;

use crate::harness::TestDaemon;

/// Test that a node can be built and started successfully
#[tokio::test]
async fn test_node_creation_and_startup() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let _storage = crate::common::temp_storage("test_node_creation_and_startup", "node");

    // Build a node connecting to the daemon
    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    // Start the node
    node.start().await.expect("Failed to start node");

    // Verify node is running
    assert!(node.is_running(), "Node should be running after start()");

    // Stop the node
    node.stop().await.expect("Failed to stop node");

    // Verify node is stopped
    assert!(
        !node.is_running(),
        "Node should not be running after stop()"
    );
}

/// Test that a node can receive announce events from Python daemon
/// (mirrors simple_send.rs example behavior)
#[tokio::test]
async fn test_node_receives_announce_from_daemon() {
    // Codeberg #49 P3 instrumentation: capture the Stage-6 event-log so a
    // failed run produces a structured trace for jl/jldiff analysis.
    let _evlog = init_event_log();
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let _storage = crate::common::temp_storage("test_node_receives_announce_from_daemon", "node");

    // Build and start node
    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");
    node.wait_for_interfaces_ready(Duration::from_secs(2))
        .await
        .expect("Interfaces should become ready");
    daemon
        .wait_for_peer_count(1, Duration::from_secs(2))
        .await
        .expect("Daemon should register peer");

    // Take event receiver
    let mut events = node
        .take_event_receiver()
        .expect("Failed to get event receiver");

    // Register a destination on daemon and announce it
    let dest = daemon
        .register_destination("test", &["announce_test"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest.hash, b"test announce data")
        .await
        .expect("Failed to announce");

    // Wait for AnnounceReceived event (skip other events like PathFound)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while tokio::time::Instant::now() < deadline {
        let event_result = timeout(deadline - tokio::time::Instant::now(), events.recv()).await;

        match event_result {
            Ok(Some(NodeEvent::AnnounceReceived { announce, .. })) => {
                let received_hash = hex::encode(announce.destination_hash());
                assert_eq!(
                    received_hash, dest.hash,
                    "Announce destination hash should match"
                );
                found = true;
                break;
            }
            Ok(Some(_other)) => {
                // Skip non-announce events (e.g. PathFound)
                continue;
            }
            Ok(None) => panic!("Event channel closed"),
            Err(_) => break,
        }
    }
    assert!(
        found,
        "Should receive AnnounceReceived event within timeout"
    );

    // Clean up
    node.stop().await.expect("Failed to stop node");
}

/// Test that a node can receive multiple events
/// (mirrors echo_server.rs event handling pattern)
#[tokio::test]
async fn test_node_receives_multiple_announces() {
    // Codeberg #49 P3 instrumentation.
    let _evlog = init_event_log();
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let _storage = crate::common::temp_storage("test_node_receives_multiple_announces", "node");

    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");
    node.wait_for_interfaces_ready(Duration::from_secs(2))
        .await
        .expect("Interfaces should become ready");
    daemon
        .wait_for_peer_count(1, Duration::from_secs(2))
        .await
        .expect("Daemon should register peer");

    let mut events = node
        .take_event_receiver()
        .expect("Failed to get event receiver");

    // Register and announce multiple destinations
    let dest1 = daemon
        .register_destination("app1", &["service1"])
        .await
        .expect("Failed to register dest1");

    let dest2 = daemon
        .register_destination("app2", &["service2"])
        .await
        .expect("Failed to register dest2");

    daemon
        .announce_destination(&dest1.hash, b"data1")
        .await
        .expect("Failed to announce dest1");

    // Small delay between announces
    tokio::time::sleep(Duration::from_millis(100)).await;

    daemon
        .announce_destination(&dest2.hash, b"data2")
        .await
        .expect("Failed to announce dest2");

    // Collect events with timeout
    let mut received_hashes = Vec::new();
    let collection_timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();

    while received_hashes.len() < 2 && start.elapsed() < collection_timeout {
        match timeout(Duration::from_millis(500), events.recv()).await {
            Ok(Some(NodeEvent::AnnounceReceived { announce, .. })) => {
                received_hashes.push(hex::encode(announce.destination_hash()));
            }
            Ok(Some(_)) => {
                // Other event types, continue
            }
            Ok(None) => break,  // Channel closed
            Err(_) => continue, // Timeout, try again
        }
    }

    // Verify both announces received
    assert!(
        received_hashes.contains(&dest1.hash),
        "Should receive first announce"
    );
    assert!(
        received_hashes.contains(&dest2.hash),
        "Should receive second announce"
    );

    node.stop().await.expect("Failed to stop node");
}

/// Test that node inner() accessor works for direct NodeCore access
/// (mirrors chat.rs /status command pattern)
#[tokio::test]
async fn test_node_inner_accessor() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let _storage = crate::common::temp_storage("test_node_inner_accessor", "node");

    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");

    // Verify link counts are accessible via wrapper methods
    assert_eq!(
        node.active_link_count(),
        0,
        "Should have no active links initially"
    );
    assert_eq!(
        node.pending_link_count(),
        0,
        "Should have no pending links initially"
    );

    node.stop().await.expect("Failed to stop node");
}

/// Test node shutdown behavior is graceful
#[tokio::test]
async fn test_node_graceful_shutdown() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let _storage = crate::common::temp_storage("test_node_graceful_shutdown", "node");

    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");
    node.wait_for_interfaces_ready(Duration::from_secs(2))
        .await
        .expect("Interfaces should become ready");
    daemon
        .wait_for_peer_count(1, Duration::from_secs(2))
        .await
        .expect("Daemon should register peer");
    let mut events = node.take_event_receiver().unwrap();

    // Register and announce to ensure activity
    let dest = daemon
        .register_destination("shutdown_test", &["app"])
        .await
        .expect("Failed to register");
    daemon.announce_destination(&dest.hash, b"data").await.ok();

    // Let one event come through
    let _ = timeout(Duration::from_secs(2), events.recv()).await;

    // Stop should complete without hanging
    let stop_result = timeout(Duration::from_secs(5), node.stop()).await;

    assert!(stop_result.is_ok(), "Stop should complete within timeout");
    assert!(stop_result.unwrap().is_ok(), "Stop should succeed");
}

/// Test that node can be started and stopped multiple times
#[tokio::test]
async fn test_node_restart() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let _storage = crate::common::temp_storage("test_node_restart", "node");

    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    // First start/stop cycle
    node.start().await.expect("First start failed");
    assert!(node.is_running());
    node.stop().await.expect("First stop failed");
    assert!(!node.is_running());

    // Second start/stop cycle
    node.start().await.expect("Second start failed");
    assert!(node.is_running());
    node.stop().await.expect("Second stop failed");
    assert!(!node.is_running());
}

/// Test that announce processing creates a path in the node
///
/// The PathFound event may or may not be exposed depending on how the
/// high-level ReticulumNode processes events. This test verifies that after
/// receiving an announce, the node knows a path to the destination.
#[tokio::test]
async fn test_node_learns_path_from_announce() {
    // Codeberg #49 P3 instrumentation.
    let _evlog = init_event_log();
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let _storage = crate::common::temp_storage("test_node_learns_path_from_announce", "node");

    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");
    node.wait_for_interfaces_ready(Duration::from_secs(2))
        .await
        .expect("Interfaces should become ready");
    daemon
        .wait_for_peer_count(1, Duration::from_secs(2))
        .await
        .expect("Daemon should register peer");

    let mut events = node
        .take_event_receiver()
        .expect("Failed to get event receiver");

    // Register a destination and announce
    let dest = daemon
        .register_destination("path_test", &["service"])
        .await
        .expect("Failed to register");

    daemon
        .announce_destination(&dest.hash, b"path test data")
        .await
        .expect("Failed to announce");

    // Wait for announce event
    let mut saw_announce = false;
    let collection_timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();

    while !saw_announce && start.elapsed() < collection_timeout {
        match timeout(Duration::from_millis(500), events.recv()).await {
            Ok(Some(NodeEvent::AnnounceReceived { announce, .. })) => {
                let received_hash = hex::encode(announce.destination_hash());
                if received_hash == dest.hash {
                    saw_announce = true;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    assert!(saw_announce, "Should receive announce event");

    // Verify the node now has a path to the destination
    let dest_hash_bytes: [u8; 16] = hex::decode(&dest.hash)
        .expect("Invalid hex")
        .try_into()
        .expect("Wrong length");
    let dest_hash = leviculum_core::DestinationHash::new(dest_hash_bytes);

    assert!(
        node.has_path(&dest_hash),
        "Node should have a path to the announced destination"
    );

    node.stop().await.expect("Failed to stop node");
}

/// Test node builder can create node without immediately connecting
#[tokio::test]
async fn test_node_builder_creates_node_without_interfaces() {
    // This tests that the builder works even without daemon (no interfaces)
    let _storage =
        crate::common::temp_storage("test_node_builder_creates_node_without_interfaces", "node");
    let node = ReticulumNodeBuilder::new()
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node without interfaces");

    // Node should be created but have no interfaces
    // Verify identity exists
    assert_eq!(
        node.identity_hash().len(),
        16,
        "Identity hash should be 16 bytes"
    );
}

/// PR #61 accessor coverage: the path/rate-table accessors against a
/// real relayed path (rnsd announces, a Rust relay forwards, this node
/// learns a 2-hop entry with next_hop set).
///
/// Asserts: path_table_entries() reflects the learned path,
/// get_path_clone() finds it, remove_path() evicts it (idempotency on
/// the second call), a re-announce re-learns it, and
/// drop_all_paths_via(next_hop) bulk-evicts with a matching count.
/// now_ms() and rate_table_entries() are exercised along the way.
#[tokio::test]
async fn test_path_table_accessor_lifecycle() {
    let _evlog = init_event_log();
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Rust relay bridging daemon <-> node, so the node's path to the
    // daemon's destination is RELAYED (next_hop = relay identity).
    let relay_port = crate::harness::pick_free_tcp_port().expect("free port");
    let _storage_r = crate::common::temp_storage("test_path_table_accessor_lifecycle", "relay");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon.rns_addr())
        .add_tcp_server(([127, 0, 0, 1], relay_port).into())
        .storage_path(_storage_r.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build relay");
    relay.start().await.expect("Failed to start relay");

    let _storage_n = crate::common::temp_storage("test_path_table_accessor_lifecycle", "node");
    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client(([127, 0, 0, 1], relay_port).into())
        .storage_path(_storage_n.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");
    node.start().await.expect("Failed to start node");
    node.wait_for_interfaces_ready(Duration::from_secs(5))
        .await
        .expect("node interfaces ready");

    let t0 = node.now_ms();

    let dest = daemon
        .register_destination("accessor_test", &["lifecycle"])
        .await
        .expect("Failed to register");
    daemon
        .announce_destination(&dest.hash, b"accessor test")
        .await
        .expect("Failed to announce");

    let dest_hash = crate::common::parse_dest_hash(&dest.hash);
    let learned = crate::common::wait_for_path_reannounce(
        || node.has_path(&dest_hash),
        &daemon,
        &dest.hash,
        b"accessor test",
        Duration::from_secs(20),
    )
    .await;
    assert!(learned, "node should learn the relayed path within 20s");

    // path_table_entries() reflects the learned path.
    let entries = node.path_table_entries();
    let entry = entries
        .iter()
        .find(|e| e.hash == *dest_hash.as_bytes())
        .expect("path_table_entries must contain the learned destination");
    assert!(entry.hops >= 2, "relayed path must be >= 2 hops");
    let via_bytes = entry
        .next_hop
        .expect("relayed path must carry a next_hop (the relay)");

    // get_path_clone() finds the same entry.
    let clone = node
        .get_path_clone(&dest_hash)
        .expect("get_path_clone must find the learned path");
    assert_eq!(clone.hops, entry.hops);
    assert_eq!(clone.next_hop, entry.next_hop);

    // now_ms() is live and monotonic across the learn phase.
    assert!(node.now_ms() >= t0, "now_ms must not go backwards");

    // rate_table_entries() is callable and consistent (announce rate
    // tracking may or may not have an entry for a single announce).
    let _rates = node.rate_table_entries();

    // remove_path(): evicts, reports eviction, idempotent second call.
    assert!(node.remove_path(&dest_hash), "first remove_path -> true");
    assert!(!node.has_path(&dest_hash), "path gone after remove_path");
    assert!(node.get_path_clone(&dest_hash).is_none());
    assert!(!node.remove_path(&dest_hash), "second remove_path -> false");

    // A re-announce re-learns the path (local cache surgery only).
    tokio::time::sleep(Duration::from_secs(2)).await;
    daemon
        .announce_destination(&dest.hash, b"accessor test again")
        .await
        .expect("Failed to re-announce");
    let relearned = crate::common::wait_for_path_reannounce(
        || node.has_path(&dest_hash),
        &daemon,
        &dest.hash,
        b"accessor test again",
        Duration::from_secs(20),
    )
    .await;
    assert!(relearned, "re-announce must re-learn the evicted path");

    // drop_all_paths_via(): bulk-evicts everything routed via the relay.
    let via = leviculum_core::DestinationHash::new(via_bytes);
    let dropped = node.drop_all_paths_via(&via);
    assert!(dropped >= 1, "must drop at least the re-learned path");
    assert!(
        !node.has_path(&dest_hash),
        "path gone after drop_all_paths_via"
    );

    node.stop().await.expect("Failed to stop node");
    relay.stop().await.expect("Failed to stop relay");
}
