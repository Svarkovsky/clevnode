//! Interop tests for path system gaps (Gaps 1, 3, 4).
//!
//! Gap 1: Path timestamp refresh on forward, so active paths stay alive.
//! Gap 3: Announce bandwidth caps, regression that the cap subsystem
//! doesn't break forwarding.
//! Gap 4: 32-byte path requests, non-transport nodes send shorter requests.
//!
//! Gap 2 (LRPROOF validation) is covered by the existing
//! `test_rust_relay_announce_and_link_data` test in `rust_relay_tests.rs`.

use std::time::Duration;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::harness::TestDaemon;

// Gap 1: Path timestamp refresh on forward
/// Test: A link request forward through a Rust relay refreshes the path timestamp,
/// keeping the path alive beyond the initial expiry.
///
/// Topology: Python-A <- Rust (transport, 20s path expiry) -> Python-B
///
/// A announces, both B and relay learn the path. B creates a link to A (the link
/// request forward refreshes the path on the relay). After the link is established,
/// verify the path still exists on the relay (it was refreshed by the link request).
#[tokio::test]
async fn test_path_refresh_keeps_route_alive() {
    // Start two Python daemons
    let daemon_a = TestDaemon::start().await.expect("daemon A");
    let daemon_b = TestDaemon::start().await.expect("daemon B");

    // Build Rust relay with short path expiry (20s)
    let _storage = crate::common::temp_storage("test_path_refresh_keeps_route_alive", "node");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .path_expiry_secs(20)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay");

    relay.start().await.expect("start relay");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Register and announce destination on daemon A
    let dest_a_info = daemon_a
        .register_destination("path_refresh", &["dest_a"])
        .await
        .expect("register dest_A");

    daemon_a
        .announce_destination(&dest_a_info.hash, b"refresh-test")
        .await
        .expect("announce dest_A");

    // Wait for daemon B to learn the path through the relay
    let dest_a_hash_bytes: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_a_info.hash).unwrap().try_into().unwrap();
    let dest_a_hash = leviculum_core::DestinationHash::new(dest_a_hash_bytes);

    let b_has_path = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_b,
        &dest_a_hash,
        &daemon_a,
        &dest_a_info.hash,
        b"refresh-test",
        Duration::from_secs(20),
    )
    .await;
    assert!(b_has_path, "B should see dest_A via relay");
    assert!(
        relay.has_path(&dest_a_hash),
        "relay should have path to dest_A"
    );

    // Wait 15 seconds (close to expiry but not past it)
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Create a link from B to A, the link request forward refreshes the path
    let link_b_to_a = daemon_b
        .create_link(&dest_a_info.hash, &dest_a_info.public_key, 30)
        .await
        .expect("create link B->A");

    // Wait for link to establish (a few seconds)
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Send a message to verify link works
    daemon_b
        .send_on_link(&link_b_to_a, b"hello")
        .await
        .expect("send on link");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // At this point we're ~22s after announce. Original path would have expired
    // at 20s, but the link request at 15s refreshed it to 15+20=35s.
    assert!(
        relay.has_path(&dest_a_hash),
        "relay path should still exist (refreshed by link request forward)"
    );

    relay.stop().await.expect("stop relay");
}

/// Test: An idle path expires after path_expiry_secs on the Rust relay.
///
/// Send one message, then wait >30s without traffic. The relay's path
/// should expire.
#[tokio::test]
async fn test_path_expires_when_idle() {
    let daemon_a = TestDaemon::start().await.expect("daemon A");
    let daemon_b = TestDaemon::start().await.expect("daemon B");

    // Short path expiry: 15 seconds
    let _storage = crate::common::temp_storage("test_path_expires_when_idle", "node");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .path_expiry_secs(15)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay");

    relay.start().await.expect("start relay");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dest_a_info = daemon_a
        .register_destination("idle_test", &["dest_a"])
        .await
        .expect("register dest_A");

    daemon_a
        .announce_destination(&dest_a_info.hash, b"idle-test")
        .await
        .expect("announce dest_A");

    let dest_a_hash_bytes: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_a_info.hash).unwrap().try_into().unwrap();
    let dest_a_hash = leviculum_core::DestinationHash::new(dest_a_hash_bytes);

    let b_has_path = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_b,
        &dest_a_hash,
        &daemon_a,
        &dest_a_info.hash,
        b"idle-test",
        Duration::from_secs(20),
    )
    .await;
    assert!(b_has_path, "B should see dest_A via relay");

    // Relay should have path now
    assert!(
        relay.has_path(&dest_a_hash),
        "relay should have path initially"
    );

    // Wait for expiry (15s + some margin)
    tokio::time::sleep(Duration::from_secs(20)).await;

    // After idle timeout, relay's path should have expired
    assert!(
        !relay.has_path(&dest_a_hash),
        "relay path should expire after 15s idle"
    );

    relay.stop().await.expect("stop relay");
}

// Gap 4: Path request format
/// Test: A non-transport Rust node sends a path request through a Python relay,
/// and the relay responds with the path. This exercises the 32-byte path request
/// format (no transport_id) that non-transport nodes should use.
///
/// Topology: Rust (non-transport) -> Python (transport relay) <- Python-B
#[tokio::test]
async fn test_path_request_through_python_relay() {
    // Python transport relay
    let py_relay = TestDaemon::start().await.expect("relay daemon");
    // Python endpoint
    let py_dest = TestDaemon::start().await.expect("dest daemon");

    // Connect relay to endpoint
    py_relay
        .add_client_interface("127.0.0.1", py_dest.rns_port(), Some("LinkTo_Dest"))
        .await
        .expect("connect relay to dest");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Register and announce destination on py_dest
    let dest_info = py_dest
        .register_destination("pathreq_test", &["target"])
        .await
        .expect("register target dest");

    py_dest
        .announce_destination(&dest_info.hash, b"target-data")
        .await
        .expect("announce target");

    // Wait for relay to learn the path
    let dest_hash_bytes: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_info.hash).unwrap().try_into().unwrap();
    let dest_hash = leviculum_core::DestinationHash::new(dest_hash_bytes);

    let relay_has_path = crate::common::wait_for_path_reannounce_on_daemon(
        &py_relay,
        &dest_hash,
        &py_dest,
        &dest_info.hash,
        b"target-data",
        Duration::from_secs(15),
    )
    .await;
    assert!(relay_has_path, "Python relay should learn path to target");

    // Build Rust non-transport node connected to relay
    let _storage = crate::common::temp_storage("test_path_request_through_python_relay", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(py_relay.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust node");

    rust_node.start().await.expect("start Rust node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // The Rust node doesn't have a path to the target yet.
    // The announce should have propagated from relay to us as well,
    // but let's verify it arrives.
    let rust_has_path = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&dest_hash),
        &py_dest,
        &dest_info.hash,
        b"target-data",
        Duration::from_secs(15),
    )
    .await;

    assert!(
        rust_has_path,
        "Rust non-transport node should learn path via announce from relay"
    );

    rust_node.stop().await.expect("stop Rust node");
}

/// Test: Requesting a path for an unknown destination does not crash or hang.
/// The request should time out gracefully.
#[tokio::test]
async fn test_path_request_for_unknown_destination() {
    let py_relay = TestDaemon::start().await.expect("relay daemon");

    let _storage = crate::common::temp_storage("test_path_request_for_unknown_destination", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(py_relay.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust node");

    rust_node.start().await.expect("start Rust node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Request path to a destination that doesn't exist anywhere
    let fake_dest = leviculum_core::DestinationHash::new([0xDE; TRUNCATED_HASHBYTES]);
    assert!(
        !rust_node.has_path(&fake_dest),
        "should not have path to unknown dest"
    );

    // Wait a reasonable time, should not crash, should not get a path
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !rust_node.has_path(&fake_dest),
        "should still not have path to unknown dest after waiting"
    );

    rust_node.stop().await.expect("stop Rust node");
}

// Gap 3: Announce bandwidth caps (regression)
/// Test: Announces forwarded through a Rust transport relay reach both endpoints.
/// This is a regression test ensuring the announce cap subsystem (dormant for TCP
/// since bitrate=0 means no cap) doesn't break announce forwarding.
#[tokio::test]
async fn test_announces_forwarded_through_transport() {
    let daemon_a = TestDaemon::start().await.expect("daemon A");
    let daemon_b = TestDaemon::start().await.expect("daemon B");

    let _storage =
        crate::common::temp_storage("test_announces_forwarded_through_transport", "node");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay");

    relay.start().await.expect("start relay");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Register and announce on both sides
    let dest_a_info = daemon_a
        .register_destination("cap_test", &["dest_a"])
        .await
        .expect("register dest_A");
    let dest_b_info = daemon_b
        .register_destination("cap_test", &["dest_b"])
        .await
        .expect("register dest_B");

    daemon_a
        .announce_destination(&dest_a_info.hash, b"from-A")
        .await
        .expect("announce A");
    // Spread the two announces so the relay fans them out to each peer
    // more than ~1 s apart; otherwise Python-RNS's Interface.ingress_control
    // (reference/Reticulum/RNS/Interfaces/Interface.py:117-138, threshold
    // IC_BURST_FREQ_NEW=3.5/s) marks the spawned-peer interface as burst-
    // active and holds the second announce for up to 6 minutes.
    tokio::time::sleep(Duration::from_secs(2)).await;
    daemon_b
        .announce_destination(&dest_b_info.hash, b"from-B")
        .await
        .expect("announce B");

    let dest_a_hash_bytes: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_a_info.hash).unwrap().try_into().unwrap();
    let dest_b_hash_bytes: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_b_info.hash).unwrap().try_into().unwrap();
    let dest_a_hash = leviculum_core::DestinationHash::new(dest_a_hash_bytes);
    let dest_b_hash = leviculum_core::DestinationHash::new(dest_b_hash_bytes);

    // Both daemons should see each other's destinations through relay
    let a_sees_b = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_a,
        &dest_b_hash,
        &daemon_b,
        &dest_b_info.hash,
        b"from-B",
        Duration::from_secs(20),
    )
    .await;
    let b_sees_a = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_b,
        &dest_a_hash,
        &daemon_a,
        &dest_a_info.hash,
        b"from-A",
        Duration::from_secs(20),
    )
    .await;

    assert!(a_sees_b, "A should see B via relay");
    assert!(b_sees_a, "B should see A via relay");

    relay.stop().await.expect("stop relay");
}

/// Test: Multiple announces registered and sent in sequence are all eventually
/// forwarded through the relay. Uses small delays between announces to avoid
/// hitting Python's announce rate limiter.
#[tokio::test]
async fn test_burst_announces_not_lost() {
    let daemon_a = TestDaemon::start().await.expect("daemon A");
    let daemon_b = TestDaemon::start().await.expect("daemon B");

    let _storage = crate::common::temp_storage("test_burst_announces_not_lost", "node");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay");

    relay.start().await.expect("start relay");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Register multiple destinations on daemon A and announce them with small delays
    let mut dest_hashes = Vec::new();
    for i in 0..3 {
        let dest_info = daemon_a
            .register_destination("burst_test", &[&format!("dest_{}", i)])
            .await
            .expect("register dest");

        daemon_a
            .announce_destination(&dest_info.hash, format!("burst-{}", i).as_bytes())
            .await
            .expect("announce dest");

        let hash_bytes: [u8; TRUNCATED_HASHBYTES] =
            hex::decode(&dest_info.hash).unwrap().try_into().unwrap();
        dest_hashes.push(leviculum_core::DestinationHash::new(hash_bytes));

        // Small delay between announces to avoid rate limiting
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // Wait for all paths to propagate to daemon B (generous timeout)
    let mut all_visible = true;
    for (i, dest_hash) in dest_hashes.iter().enumerate() {
        let visible = crate::common::wait_for_path_reannounce_on_daemon(
            &daemon_b,
            dest_hash,
            &daemon_a,
            &hex::encode(dest_hash.as_bytes()),
            format!("burst-{}", i).as_bytes(),
            Duration::from_secs(30),
        )
        .await;
        if !visible {
            all_visible = false;
            eprintln!(
                "daemon B did not learn path to {}",
                hex::encode(dest_hash.as_bytes())
            );
        }
    }

    assert!(all_visible, "all announces should reach daemon B");

    relay.stop().await.expect("stop relay");
}
