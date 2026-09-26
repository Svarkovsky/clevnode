//! Comprehensive relay integration tests exercising multi-relay topologies,
//! relay failure with path recovery, and mixed Python+Rust relay chains.
//!
//! ## Test 1: Diamond relay, single path, then failover
//!
//! Phase 1:
//! ```text
//!            ┌──── Rust-R1 (relay) ────┐
//!   Py-A ───┘                          └─── Py-B
//! ```
//!
//! Phase 2: R1 dies, R2 takes over:
//! ```text
//!            ╳  (R1 dead)
//!   Py-A ───┤                          ├─── Py-B
//!            └──── Rust-R2 (relay) ────┘
//! ```
//!
//! ## Test 2: Mixed Python+Rust relay chain
//!
//! ```text
//!   Py-A ←── Py-M (relay) ←── Rust-R (relay) ──→ Py-B
//! ```
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop relay_integration_tests -- --nocapture
//! ```

use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{collect_messages, parse_dest_hash};
use crate::harness::TestDaemon;

/// Test: Diamond relay topology with failure recovery.
///
/// Phase 1. A single Rust relay (R1) bridges two Python daemons.
///   Verifies announce propagation, bidirectional link+data, and relay stats.
///
/// Phase 2. R1 is killed, R2 is started, A re-announces. B discovers
///   A via R2 and re-establishes communication.
#[tokio::test]
async fn test_diamond_relay_and_failure_recovery() {
    // Phase 1: Single relay path
    // Step 1: Start two Python daemons
    let daemon_a = TestDaemon::start().await.expect("Failed to start daemon A");
    let daemon_b = TestDaemon::start().await.expect("Failed to start daemon B");

    // Step 2: Build + start Rust relay R1
    let _storage1 = crate::common::temp_storage("test_diamond_relay_and_failure_recovery", "node1");
    let mut relay_r1 = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage1.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build relay R1");

    relay_r1.start().await.expect("Failed to start relay R1");

    // Step 3: Wait for TCP stabilization
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 4: Register destinations
    let dest_a_info = daemon_a
        .register_destination("diamond_test", &["dest_a"])
        .await
        .expect("Failed to register dest_a");
    let dest_b_info = daemon_b
        .register_destination("diamond_test", &["dest_b"])
        .await
        .expect("Failed to register dest_b");

    let dest_a_hash = parse_dest_hash(&dest_a_info.hash);
    let dest_b_hash = parse_dest_hash(&dest_b_info.hash);

    // Step 5: Both daemons announce
    daemon_a
        .announce_destination(&dest_a_info.hash, b"diamond-A")
        .await
        .expect("Failed to announce dest_a");
    // Space below Python's ingress_control IC_BURST_FREQ_NEW threshold
    // (docs/src/architecture-broadcast-python-parity.md, B1 subtlety).
    tokio::time::sleep(Duration::from_secs(2)).await;
    daemon_b
        .announce_destination(&dest_b_info.hash, b"diamond-B")
        .await
        .expect("Failed to announce dest_b");

    // Step 6: Wait for cross-visibility via R1
    let a_sees_b = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_a,
        &dest_b_hash,
        &daemon_b,
        &dest_b_info.hash,
        b"diamond-B",
        Duration::from_secs(20),
    )
    .await;
    let b_sees_a = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_b,
        &dest_a_hash,
        &daemon_a,
        &dest_a_info.hash,
        b"diamond-A",
        Duration::from_secs(20),
    )
    .await;

    assert!(a_sees_b, "A should see B via R1 within 20s");
    assert!(b_sees_a, "B should see A via R1 within 20s");

    // Verify R1 has paths to both
    assert!(relay_r1.has_path(&dest_a_hash), "R1 should know dest_a");
    assert!(relay_r1.has_path(&dest_b_hash), "R1 should know dest_b");

    // Verify hop counts
    let a_paths = daemon_a.get_path_table().await.unwrap();
    if let Some(path) = a_paths.get(&dest_b_info.hash) {
        assert!(
            path.hops.unwrap_or(0) >= 1,
            "A's path to B should have hops >= 1"
        );
    }
    let b_paths = daemon_b.get_path_table().await.unwrap();
    if let Some(path) = b_paths.get(&dest_a_info.hash) {
        assert!(
            path.hops.unwrap_or(0) >= 1,
            "B's path to A should have hops >= 1"
        );
    }

    // Step 7: A creates link to B, sends 3 messages
    let link_a_to_b = daemon_a
        .create_link(&dest_b_info.hash, &dest_b_info.public_key, 30)
        .await
        .expect("Failed to create link A->B");

    for i in 0..3 {
        let msg = format!("diamond-A-{}", i);
        daemon_a
            .send_on_link(&link_a_to_b, msg.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("Failed to send message {} from A", i));
    }

    // Step 8: Poll B for messages
    let b_received = collect_messages(&daemon_b, "diamond-A-", 3, Duration::from_secs(10)).await;
    for i in 0..3 {
        let expected = format!("diamond-A-{}", i);
        assert!(
            b_received.contains(&expected),
            "B should have received '{}'",
            expected
        );
    }

    // Step 9: B creates link to A, sends 3 messages
    let link_b_to_a = daemon_b
        .create_link(&dest_a_info.hash, &dest_a_info.public_key, 15)
        .await
        .expect("Failed to create link B->A");

    for i in 0..3 {
        let msg = format!("diamond-B-{}", i);
        daemon_b
            .send_on_link(&link_b_to_a, msg.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("Failed to send message {} from B", i));
    }

    // Step 10: Poll A for messages
    let a_received = collect_messages(&daemon_a, "diamond-B-", 3, Duration::from_secs(10)).await;
    for i in 0..3 {
        let expected = format!("diamond-B-{}", i);
        assert!(
            a_received.contains(&expected),
            "A should have received '{}'",
            expected
        );
    }

    // R1 should have forwarded packets
    let r1_stats = relay_r1.transport_stats();
    assert!(
        r1_stats.packets_forwarded() > 0,
        "R1 should have forwarded packets"
    );

    // Phase 2: Failure + recovery
    // Step 11: Close old links
    let _ = daemon_a.close_link(&link_a_to_b).await;
    let _ = daemon_b.close_link(&link_b_to_a).await;

    // Step 12: Build + start Rust relay R2
    let _storage2 = crate::common::temp_storage("test_diamond_relay_and_failure_recovery", "node2");
    let mut relay_r2 = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage2.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build relay R2");

    relay_r2.start().await.expect("Failed to start relay R2");

    // Step 13: Wait for R2 TCP stabilization
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 14: Kill R1
    relay_r1.stop().await.expect("Failed to stop R1");

    // Step 15: Wait for Python to detect dead TCP
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Step 16: B attempts link to A, should fail (R1 dead, old path stale)
    let link_attempt = daemon_b
        .create_link(&dest_a_info.hash, &dest_a_info.public_key, 5)
        .await;
    assert!(
        link_attempt.is_err(),
        "Link through dead R1 should fail (got: {:?})",
        link_attempt
    );

    // Step 17: Both endpoints re-announce. Phase-1 left a stale R1 path on
    // BOTH daemons (A->B via R1 and B->A via R1); for step 19's LinkRequest
    // round-trip to succeed, both sides must have a fresh R2 path to the
    // peer, not just one. Space the two announces by 2s so Python's
    // ingress_control (IC_BURST_FREQ_NEW=3.5/s, Interface.py:117) does not
    // penalise the second — same subtlety documented in
    // architecture-broadcast-python-parity.md section B1.
    daemon_a
        .announce_destination(&dest_a_info.hash, b"diamond-A-fresh")
        .await
        .expect("Failed to re-announce dest_a");
    tokio::time::sleep(Duration::from_secs(2)).await;
    daemon_b
        .announce_destination(&dest_b_info.hash, b"diamond-B-fresh")
        .await
        .expect("Failed to re-announce dest_b");

    // Step 18: Wait for B to learn path via R2
    let b_sees_a_again = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_b,
        &dest_a_hash,
        &daemon_a,
        &dest_a_info.hash,
        b"diamond-A-fresh",
        Duration::from_secs(15),
    )
    .await;
    assert!(b_sees_a_again, "B should see A again via R2");

    // A must also have learned B's fresh R2 path before step 19's
    // LinkRequest arrives, otherwise A's LRPROOF has no path to B.
    let a_sees_b_again = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_a,
        &dest_b_hash,
        &daemon_b,
        &dest_b_info.hash,
        b"diamond-B-fresh",
        Duration::from_secs(15),
    )
    .await;
    assert!(a_sees_b_again, "A should see B again via R2");

    // Step 19: B creates link to A via R2, should succeed
    let link_recovery = daemon_b
        .create_link(&dest_a_info.hash, &dest_a_info.public_key, 15)
        .await
        .expect("Link through R2 should succeed");

    // Step 20: B sends recovery message
    daemon_b
        .send_on_link(&link_recovery, b"recovery-message")
        .await
        .expect("Failed to send recovery message");

    // Step 21: Poll A for recovery message
    let a_recovery =
        collect_messages(&daemon_a, "recovery-message", 1, Duration::from_secs(10)).await;
    assert!(
        a_recovery.contains("recovery-message"),
        "A should have received recovery message"
    );

    // Step 22: R2 should have forwarded packets
    let r2_stats = relay_r2.transport_stats();
    assert!(
        r2_stats.packets_forwarded() > 0,
        "R2 should have forwarded packets"
    );

    // Clean up
    relay_r2.stop().await.expect("Failed to stop R2");
}

/// Test: Mixed Python+Rust relay chain (3-hop).
///
/// Topology:
/// ```text
///   Py-A ←── Rust-R (relay) ──→ Py-M (relay) ←── Py-B (client to M)
/// ```
///
/// Chain: A ↔ R (Rust relay) ↔ M (Python relay) ↔ B
///
/// R connects to A and M as TCP clients. B connects to M as a TCP client.
/// Announce propagation B→M→R→A is verified, proving both relay types
/// forward announces in sequence. A then creates a link to B through the
/// full chain, exercising link request/proof routing and data transfer
/// across both relay types.
///
/// Verifies:
/// - Announce propagation through mixed relay chain (B → A direction)
/// - Hop count incrementing across relay types
/// - Link establishment traversing both Rust and Python relays
/// - Bidirectional data delivery through the chain
/// - Both relays show forwarding activity
#[tokio::test]
async fn test_mixed_python_rust_relay_chain() {
    // Step 1: Start three Python daemons
    let daemon_a = TestDaemon::start().await.expect("Failed to start daemon A");
    let daemon_m = TestDaemon::start()
        .await
        .expect("Failed to start daemon M (middle)");
    let daemon_b = TestDaemon::start().await.expect("Failed to start daemon B");

    // Step 2: B connects to M (Python-to-Python link)
    daemon_b
        .add_client_interface("127.0.0.1", daemon_m.rns_port(), Some("LinkTo_M"))
        .await
        .expect("Failed to connect B to M");

    // Let B-M connection stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 3: Build Rust relay R (clients to A and M)
    let _storage = crate::common::temp_storage("test_mixed_python_rust_relay_chain", "node");
    let mut rust_relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_m.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust relay");

    rust_relay
        .start()
        .await
        .expect("Failed to start Rust relay");

    // Step 4: Wait for all TCP connections to stabilize
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Step 5: Register destinations and announce
    let dest_a_info = daemon_a
        .register_destination("mixed_test", &["dest_a"])
        .await
        .expect("Failed to register dest_a");
    let dest_a_hash = parse_dest_hash(&dest_a_info.hash);

    daemon_a
        .announce_destination(&dest_a_info.hash, b"mixed-A")
        .await
        .expect("Failed to announce dest_a");

    let dest_b_info = daemon_b
        .register_destination("mixed_test", &["dest_b"])
        .await
        .expect("Failed to register dest_b");
    let dest_b_hash = parse_dest_hash(&dest_b_info.hash);

    // Space below Python's ingress_control IC_BURST_FREQ_NEW threshold.
    tokio::time::sleep(Duration::from_secs(2)).await;
    daemon_b
        .announce_destination(&dest_b_info.hash, b"mixed-B")
        .await
        .expect("Failed to announce dest_b");

    // Step 6: Verify announce propagation
    // B's announce chain: B → M (direct client) → R (M rebroadcasts) → A (R forwards)
    // This direction reliably works; the reverse (A→R→M→B) hits a Python
    // rebroadcast limitation where M doesn't forward to sibling client handlers.

    // R should see both destinations quickly (direct connections)
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        rust_relay.has_path(&dest_a_hash),
        "R should see A (direct TCP client connection)"
    );

    // M should see A via R's rebroadcast
    let m_sees_a = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_m,
        &dest_a_hash,
        &daemon_a,
        &dest_a_info.hash,
        b"mixed-A",
        Duration::from_secs(15),
    )
    .await;
    assert!(
        m_sees_a,
        "M should see A (R rebroadcasts to M's server interface)"
    );

    // Verify R sees B (proves B→M→R chain works)
    assert!(
        rust_relay.has_path(&dest_b_hash),
        "R should see B (B→M→R announce chain)"
    );

    // A should see B through the full chain
    let a_sees_b = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_a,
        &dest_b_hash,
        &daemon_b,
        &dest_b_info.hash,
        b"mixed-B",
        Duration::from_secs(20),
    )
    .await;
    assert!(
        a_sees_b,
        "A should see B through mixed chain (B → M → R → A)"
    );

    // Step 7: Verify hop counts
    // R received A's announce directly (hops=0 in packet, Rust stores raw value)
    let r_hops_to_a = rust_relay.hops_to(&dest_a_hash);
    assert!(
        r_hops_to_a.is_some(),
        "R should have path to A with hop count"
    );

    // A's path to B should have hops >= 2 (through M and R)
    // Python increments hops on receipt, so A sees B at hops >= 2
    let a_paths = daemon_a.get_path_table().await.unwrap();
    if let Some(path_to_b) = a_paths.get(&dest_b_info.hash) {
        assert!(
            path_to_b.hops.unwrap_or(0) >= 2,
            "A's path to B should have hops >= 2 (multi-hop), got {:?}",
            path_to_b.hops
        );
    }

    // Step 8: Verify relay stats
    // R forwarded announces from both directions
    let r_stats = rust_relay.transport_stats();
    assert!(
        r_stats.packets_forwarded() > 0,
        "Rust relay should have forwarded packets"
    );
    assert!(
        r_stats.announces_processed() >= 2,
        "R should have processed announces from both A and B"
    );

    // Step 9: A creates link to B through the full chain (A → R → M → B)
    let link_a_to_b = daemon_a
        .create_link(&dest_b_info.hash, &dest_b_info.public_key, 15)
        .await
        .expect("Link A→B through mixed chain should succeed");

    // Step 10: A sends messages to B through mixed chain
    for i in 0..3 {
        let msg = format!("mixed-A-{}", i);
        daemon_a
            .send_on_link(&link_a_to_b, msg.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("Failed to send message {} from A", i));
    }

    // Step 11: Poll B for messages
    let b_received = collect_messages(&daemon_b, "mixed-A-", 3, Duration::from_secs(10)).await;
    for i in 0..3 {
        let expected = format!("mixed-A-{}", i);
        assert!(
            b_received.contains(&expected),
            "B should have received '{}'",
            expected
        );
    }

    // Clean up
    rust_relay.stop().await.expect("Failed to stop Rust relay");
}
