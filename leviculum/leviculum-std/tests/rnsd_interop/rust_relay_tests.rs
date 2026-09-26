//! Tests for Rust node acting as a transport relay between two Python daemons.
//!
//! Topology: Python-A (TCPServer) <- Rust (TCPClient×2, transport=true) -> Python-B (TCPServer)
//!
//! The Rust node connects to both daemons as a TCP client and relays
//! announces, path requests, and link traffic between them.
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop rust_relay_tests -- --nocapture
//! ```

use std::time::Duration;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::harness::TestDaemon;

/// Test: Rust relay forwards announces and routes link data between two Python daemons.
///
/// Topology: Python-A (TCPServer) <- Rust (transport=true) -> Python-B (TCPServer)
///
/// Verifies:
/// - Announce rebroadcast by Rust relay (bidirectional)
/// - Path learning at relay and edge nodes
/// - Hop count incrementing
/// - Link request forwarding (both directions)
/// - Link proof routing back through relay
/// - Bidirectional data routing via link table
/// - Transport stats tracking
#[tokio::test]
async fn test_rust_relay_announce_and_link_data() {
    // Step 1: Start two independent Python daemons
    let daemon_a = TestDaemon::start().await.expect("Failed to start daemon A");
    let daemon_b = TestDaemon::start().await.expect("Failed to start daemon B");

    // Step 2: Build Rust node with transport enabled, connecting to both daemons
    let _storage = crate::common::temp_storage("test_rust_relay_announce_and_link_data", "node");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build relay node");

    relay.start().await.expect("Failed to start relay node");
    assert!(
        relay.is_transport_enabled(),
        "Relay should have transport enabled"
    );

    // Wait for TCP connections to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 3: Register destinations on both daemons
    let dest_a_info = daemon_a
        .register_destination("relay_test", &["dest_a"])
        .await
        .expect("Failed to register dest_A on daemon A");

    let dest_b_info = daemon_b
        .register_destination("relay_test", &["dest_b"])
        .await
        .expect("Failed to register dest_B on daemon B");

    let dest_a_hash_bytes: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_a_info.hash).unwrap().try_into().unwrap();
    let dest_b_hash_bytes: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_b_info.hash).unwrap().try_into().unwrap();
    let dest_a_hash = leviculum_core::DestinationHash::new(dest_a_hash_bytes);
    let dest_b_hash = leviculum_core::DestinationHash::new(dest_b_hash_bytes);

    // Step 4: Both daemons announce their destinations.
    // Space below Python's ingress_control IC_BURST_FREQ_NEW threshold
    // so the Rust relay's fanout doesn't trip Python's per-interface
    // burst detector (see architecture-broadcast-python-parity.md, B1
    // subtlety).
    daemon_a
        .announce_destination(&dest_a_info.hash, b"hello-from-A")
        .await
        .expect("Failed to announce dest_A");
    tokio::time::sleep(Duration::from_secs(2)).await;
    daemon_b
        .announce_destination(&dest_b_info.hash, b"hello-from-B")
        .await
        .expect("Failed to announce dest_B");

    // Step 5: Wait for cross-visibility via Rust relay
    let a_sees_b = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_a,
        &dest_b_hash,
        &daemon_b,
        &dest_b_info.hash,
        b"hello-from-B",
        Duration::from_secs(20),
    )
    .await;
    let b_sees_a = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_b,
        &dest_a_hash,
        &daemon_a,
        &dest_a_info.hash,
        b"hello-from-A",
        Duration::from_secs(20),
    )
    .await;

    assert!(
        a_sees_b,
        "Python-A should see dest_B via Rust relay within 20s"
    );
    assert!(
        b_sees_a,
        "Python-B should see dest_A via Rust relay within 20s"
    );

    // Step 6: Verify hop counts and relay state
    let a_paths = daemon_a.get_path_table().await.unwrap();
    let b_paths = daemon_b.get_path_table().await.unwrap();

    if let Some(path) = a_paths.get(&dest_b_info.hash) {
        assert!(
            path.hops.unwrap_or(0) >= 1,
            "Python-A should see dest_B with hops >= 1 (through Rust relay)"
        );
    }

    if let Some(path) = b_paths.get(&dest_a_info.hash) {
        assert!(
            path.hops.unwrap_or(0) >= 1,
            "Python-B should see dest_A with hops >= 1 (through Rust relay)"
        );
    }

    // Verify Rust relay has paths to both destinations
    assert!(
        relay.has_path(&dest_a_hash),
        "Rust relay should have path to dest_A"
    );
    assert!(
        relay.has_path(&dest_b_hash),
        "Rust relay should have path to dest_B"
    );

    // Allow time for path/transport info to settle
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Step 7: Python-A creates link to dest_B and sends messages
    let link_a_to_b = daemon_a
        .create_link(&dest_b_info.hash, &dest_b_info.public_key, 30)
        .await
        .expect("Failed to create link from A to B");

    // Send 5 messages from A to B
    for i in 0..5 {
        let msg = format!("msg-from-A-{}", i);
        daemon_a
            .send_on_link(&link_a_to_b, msg.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("Failed to send message {} from A", i));
    }

    // Step 8: Wait and verify Python-B received the messages
    // the test daemon echoes received packets, which causes echo ping-pong.
    // We check that each unique message was received at least once.
    let mut b_received_set = std::collections::HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while b_received_set.len() < 5 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let packets = daemon_b.get_received_packets().await.unwrap_or_default();
        for p in &packets {
            let s = String::from_utf8_lossy(&p.data);
            if s.starts_with("msg-from-A-") {
                b_received_set.insert(s.to_string());
            }
        }
    }

    for i in 0..5 {
        let expected = format!("msg-from-A-{}", i);
        assert!(
            b_received_set.contains(&expected),
            "Python-B should have received '{}'",
            expected
        );
    }

    // Step 9: Python-B creates link to dest_A and sends messages
    let link_b_to_a = daemon_b
        .create_link(&dest_a_info.hash, &dest_a_info.public_key, 15)
        .await
        .expect("Failed to create link from B to A");

    for i in 0..5 {
        let msg = format!("msg-from-B-{}", i);
        daemon_b
            .send_on_link(&link_b_to_a, msg.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("Failed to send message {} from B", i));
    }

    // Step 10: Wait and verify Python-A received the messages
    let mut a_received_set = std::collections::HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while a_received_set.len() < 5 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let packets = daemon_a.get_received_packets().await.unwrap_or_default();
        for p in &packets {
            let s = String::from_utf8_lossy(&p.data);
            if s.starts_with("msg-from-B-") {
                a_received_set.insert(s.to_string());
            }
        }
    }

    for i in 0..5 {
        let expected = format!("msg-from-B-{}", i);
        assert!(
            a_received_set.contains(&expected),
            "Python-A should have received '{}'",
            expected
        );
    }

    // Step 11: Verify relay stats
    let stats = relay.transport_stats();
    assert!(
        stats.packets_forwarded() > 0,
        "Rust relay should have forwarded packets"
    );

    // Clean up
    relay.stop().await.expect("Failed to stop relay");
}
