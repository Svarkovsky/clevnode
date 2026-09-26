//! Test: Rust-side path recovery on link timeout via LRPROOF dropping.
//!
//! Exercises the `LinkEvent::LinkClosed { reason: Timeout }` recovery code
//! in `node/mod.rs` that calls `expire_path()` + `request_path()`. This code
//! path is specific to non-transport nodes (enable_transport = false).
//!
//! ## Topology (single relay, no kill/restart)
//!
//! ```text
//! Phase 1:  Rust-Node ──→ Py-Relay ──→ Py-Dest     (normal, link works)
//! Phase 2:  Rust-Node ──→ Py-Relay ──→ Py-Dest     (relay drops LRPROOF, timeout fires)
//! Phase 3:  Rust-Node ──→ Py-Relay ──→ Py-Dest     (stop dropping, new link works)
//! ```
//!
//! ## Key Assertion
//!
//! Phase 2: `has_path() == false` after timeout. This can ONLY be caused by
//! `expire_path()` at `node/mod.rs:988` because:
//! - TCP connection is alive → `handle_interface_down()` never fires
//! - No other code path removes this path entry
//! - `expire_path()` is the only call in the timeout handler that modifies path state
//!
//! ## Running
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop path_recovery_tests -- --nocapture
//! ```

use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{
    collect_messages, extract_signing_key, parse_dest_hash, wait_for_link_closed_event,
    wait_for_link_established, wait_for_path_on_node,
};
use crate::harness::TestDaemon;

/// Smoke test: verify announce propagation between two Python daemons
/// when one connects to the other at runtime via add_client_interface.
#[tokio::test]
async fn test_announce_propagation_between_daemons() {
    let daemon_a = TestDaemon::start().await.expect("Failed to start daemon A");
    let daemon_b = TestDaemon::start().await.expect("Failed to start daemon B");

    // B connects to A
    daemon_b
        .add_client_interface("127.0.0.1", daemon_a.rns_port(), Some("LinkTo_A"))
        .await
        .expect("Failed to connect B to A");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Register + announce on A
    let dest_info = daemon_a
        .register_destination("smoke_test", &["target"])
        .await
        .expect("Failed to register");
    let dest_hash = parse_dest_hash(&dest_info.hash);

    daemon_a
        .announce_destination(&dest_info.hash, b"smoke")
        .await
        .expect("Failed to announce");

    // B should learn the path
    let found = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_b,
        &dest_hash,
        &daemon_a,
        &dest_info.hash,
        b"smoke",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Daemon B should see daemon A's announce");
}

/// Test: Rust-side path recovery when a relay drops LRPROOF and a link times out.
///
/// Phase 1, Baseline: Rust node connects to Py-Dest through Relay, sends data.
/// Phase 2, Failure: Relay drops LRPROOF, Rust node attempts new link, times out (~30s),
///           `expire_path()` fires, `has_path()` becomes false.
/// Phase 3, Recovery: Stop dropping, re-announce, new link succeeds.
#[tokio::test]
async fn test_rust_node_path_recovery_on_link_timeout() {
    // Phase 1 baseline: verify link works through Relay
    // Step 1: Start Python daemons (relay + destination)
    let py_relay = TestDaemon::start().await.expect("Failed to start Py-Relay");
    let py_dest = TestDaemon::start().await.expect("Failed to start Py-Dest");

    // Step 2: Relay connects to Py-Dest
    py_relay
        .add_client_interface("127.0.0.1", py_dest.rns_port(), Some("LinkTo_Dest"))
        .await
        .expect("Failed to connect Relay to Dest");

    // Wait for Relay ↔ Dest TCP to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 3: Build + start Rust node (non-transport, client to relay)
    let _storage =
        crate::common::temp_storage("test_rust_node_path_recovery_on_link_timeout", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(py_relay.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust node");

    let mut event_rx = rust_node
        .take_event_receiver()
        .expect("Event receiver should be available");

    rust_node.start().await.expect("Failed to start Rust node");

    // Wait for Rust ↔ Relay TCP to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 4: Register + announce destination on Py-Dest
    let dest_info = py_dest
        .register_destination("path_recovery_test", &["target"])
        .await
        .expect("Failed to register destination on Py-Dest");

    let dest_hash = parse_dest_hash(&dest_info.hash);
    let signing_key = extract_signing_key(&dest_info.public_key);

    py_dest
        .announce_destination(&dest_info.hash, b"recovery-target")
        .await
        .expect("Failed to announce on Py-Dest");

    // Step 5: Wait for announce propagation: Dest → Relay → Rust node
    assert!(
        crate::common::wait_for_path_reannounce_on_daemon(
            &py_relay,
            &dest_hash,
            &py_dest,
            &dest_info.hash,
            b"recovery-target",
            Duration::from_secs(10),
        )
        .await,
        "Relay should learn path to Py-Dest"
    );

    assert!(
        crate::common::wait_for_path_reannounce(
            || rust_node.has_path(&dest_hash),
            &py_dest,
            &dest_info.hash,
            b"recovery-target",
            Duration::from_secs(20),
        )
        .await,
        "Rust node should learn path to Py-Dest through Relay within 20s"
    );

    // Step 6: Rust node connects to Py-Dest through Relay
    let mut stream1 = rust_node
        .connect(&dest_hash, &signing_key)
        .await
        .expect("First connect() should succeed");

    assert!(
        wait_for_link_established(&mut event_rx, stream1.link_id(), Duration::from_secs(15)).await,
        "Link through Relay should establish within 15s"
    );

    // Step 7: Send data and verify Py-Dest receives it
    stream1
        .try_send(b"phase1-hello")
        .await
        .expect("Failed to send on first link");

    let received = collect_messages(&py_dest, "phase1-hello", 1, Duration::from_secs(10)).await;
    assert!(
        received.contains("phase1-hello"),
        "Py-Dest should receive data through Relay"
    );

    // Step 8: Close link cleanly
    let _ = stream1.close().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 2: Drop LRPROOF, trigger timeout recovery
    // Step 9: Path from Phase 1 should still be valid
    assert!(
        rust_node.has_path(&dest_hash),
        "Path should still be valid after clean link close"
    );

    // Step 10: Enable LRPROOF dropping on Relay
    py_relay
        .enable_lrproof_drop()
        .await
        .expect("Failed to enable LRPROOF dropping");

    // Step 11: Rust node attempts new link.
    // Link request (HEADER_2) reaches Relay → forwarded to Dest.
    // Dest generates proof → Relay drops it (LRPROOF, context 0xFF).
    // No proof reaches Rust node → link stays pending → times out after ~30s.
    let stream2 = rust_node
        .connect(&dest_hash, &signing_key)
        .await
        .expect("connect() should return immediately (async link request)");

    // Step 12: Wait for link timeout via LinkClosed event.
    // Retry budget: max(2, hops) retries × establishment_timeout_ms
    // At 2 hops with UNKNOWN_BITRATE_ASSUMPTION_BPS=300:
    // 3 attempts × ~31s base = ~94s, plus the #129 establishment jitter of
    // up to 25% per attempt = ~118s worst case. 150s gives comfortable margin.
    assert!(
        wait_for_link_closed_event(&mut event_rx, stream2.link_id(), Duration::from_secs(150))
            .await,
        "Should receive LinkClosed event — link timeout should fire within 150s"
    );

    // Step 13: KEY ASSERTION, path should be gone.
    // TCP connection is alive → handle_interface_down() never fires.
    // ONLY expire_path() at node/mod.rs:988 can cause this.
    assert!(
        !rust_node.has_path(&dest_hash),
        "Path should be expired by timeout handler (expire_path at node/mod.rs:988) \
         — TCP is alive, so handle_interface_down never fires"
    );

    // Step 14: Verify relay actually dropped at least one LRPROOF
    let drops = py_relay
        .get_lrproof_drops()
        .await
        .expect("Failed to get LRPROOF drops");
    assert!(
        !drops.is_empty(),
        "Relay should have dropped at least one LRPROOF packet, got 0 drops"
    );

    // Phase 3: Recovery, stop dropping, re-announce, new link
    // Step 15: Disable LRPROOF dropping on Relay
    py_relay
        .disable_lrproof_drop()
        .await
        .expect("Failed to disable LRPROOF dropping");

    // Step 16: Soft check, request_path() from the timeout handler may have
    // already restored the path (relay has a cached path to Py-Dest).
    // Poll for ~10s but don't hard-assert, timing-dependent.
    let path_restored_by_request =
        wait_for_path_on_node(&rust_node, &dest_hash, Duration::from_secs(10)).await;
    eprintln!(
        "Path restored by request_path() alone: {}",
        path_restored_by_request
    );

    // Step 17: If path NOT restored, re-announce from Py-Dest
    if !path_restored_by_request {
        py_dest
            .announce_destination(&dest_info.hash, b"recovery-target-v2")
            .await
            .expect("Failed to re-announce on Py-Dest");

        // Step 18: Wait for path on Rust node
        assert!(
            crate::common::wait_for_path_reannounce(
                || rust_node.has_path(&dest_hash),
                &py_dest,
                &dest_info.hash,
                b"recovery-target-v2",
                Duration::from_secs(20),
            )
            .await,
            "Rust node should learn new path to Py-Dest after re-announce"
        );
    }

    // Step 19: Rust node connects, proof arrives this time
    let stream3 = rust_node
        .connect(&dest_hash, &signing_key)
        .await
        .expect("Recovery connect() should succeed");

    assert!(
        wait_for_link_established(&mut event_rx, stream3.link_id(), Duration::from_secs(15)).await,
        "Recovery link should establish within 15s"
    );

    // Step 20: Send data and verify Py-Dest receives it
    stream3
        .try_send(b"phase3-recovered")
        .await
        .expect("Failed to send on recovered link");

    let recovered =
        collect_messages(&py_dest, "phase3-recovered", 1, Duration::from_secs(10)).await;
    assert!(
        recovered.contains("phase3-recovered"),
        "Py-Dest should receive data after recovery"
    );

    // Clean up
    rust_node.stop().await.expect("Failed to stop Rust node");
}
