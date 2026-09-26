//! Test: Full link lifecycle through a Python relay.
//!
//! Exercises three fixes from v0.5.5 end-to-end:
//!
//! - **Fix 1**: Link-addressed Data packet delivery on non-transport nodes
//! - **Fix 2**: Channel ACK wiring (proof → receipt → mark_delivered)
//! - **Fix 3**: Graceful close exposed through LinkHandle
//!
//! ## Topology
//!
//! ```text
//! Rust-Node (non-transport) → Py-Relay (transport) → Py-Dest
//! ```
//!
//! ## Phases
//!
//! 1. Setup + link establishment
//! 2. Bidirectional data (proves Fix 1)
//! 3. Channel ACK / delivery confirmation (proves Fix 2)
//! 4. Graceful close from Rust side (proves Fix 3)
//! 5. Remote close from Python side (proves Fix 1 for LINKCLOSE)
//!
//! ## Running
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop lifecycle_tests -- --nocapture
//! ```

use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{
    collect_messages, extract_signing_key, parse_dest_hash, wait_for_data_event,
    wait_for_delivery_confirmations, wait_for_link_closed_event, wait_for_link_established,
    wait_for_link_on_daemon,
};
use crate::harness::TestDaemon;

// Test
#[tokio::test]
async fn test_full_link_lifecycle_through_relay() {
    // Phase 1: Setup + Link Establishment
    // Step 1: Start Python daemons (relay + destination)
    let py_relay = TestDaemon::start().await.expect("Failed to start Py-Relay");
    let py_dest = TestDaemon::start().await.expect("Failed to start Py-Dest");

    // Step 2: Relay connects to Dest
    py_relay
        .add_client_interface("127.0.0.1", py_dest.rns_port(), Some("LinkTo_Dest"))
        .await
        .expect("Failed to connect Relay to Dest");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 3: Build Rust node (non-transport, client to relay)
    let _storage = crate::common::temp_storage("test_full_link_lifecycle_through_relay", "node");
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
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 4: Register + announce destination on Py-Dest
    let dest_info = py_dest
        .register_destination("lifecycle_test", &["target"])
        .await
        .expect("Failed to register destination on Py-Dest");

    let dest_hash = parse_dest_hash(&dest_info.hash);
    let signing_key = extract_signing_key(&dest_info.public_key);

    py_dest
        .announce_destination(&dest_info.hash, b"lifecycle-target")
        .await
        .expect("Failed to announce on Py-Dest");

    // Step 5: Wait for path propagation: Dest → Relay → Rust
    assert!(
        crate::common::wait_for_path_reannounce(
            || rust_node.has_path(&dest_hash),
            &py_dest,
            &dest_info.hash,
            b"lifecycle-target",
            Duration::from_secs(20),
        )
        .await,
        "Rust node should learn path to Py-Dest through Relay"
    );

    // Step 6: Rust node connects to Py-Dest through Relay
    let mut stream = rust_node
        .connect(&dest_hash, &signing_key)
        .await
        .expect("connect() should succeed");

    let link_hash_hex = hex::encode(stream.link_id().as_bytes());
    eprintln!("Link established with hash: {}", link_hash_hex);

    // Step 7: Wait for LinkEstablished
    assert!(
        wait_for_link_established(&mut event_rx, stream.link_id(), Duration::from_secs(15)).await,
        "Link through Relay should establish within 15s"
    );

    // Wait for link to appear on Python side
    assert!(
        wait_for_link_on_daemon(&py_dest, &link_hash_hex, Duration::from_secs(10)).await,
        "Py-Dest should show the link in its link table"
    );

    // Phase 2: Bidirectional Data (proves Fix 1)
    // Step 8: Rust → Python
    stream
        .try_send(b"hello-from-rust")
        .await
        .expect("Failed to send Rust→Python");

    let rust_to_py =
        collect_messages(&py_dest, "hello-from-rust", 1, Duration::from_secs(10)).await;
    assert!(
        rust_to_py.contains("hello-from-rust"),
        "Py-Dest should receive data from Rust through Relay"
    );

    // Step 9: Python → Rust
    // Before Fix 1, this would hang forever: Transport::handle_data() silently
    // dropped link-addressed Data packets on non-transport nodes.
    py_dest
        .send_on_link(&link_hash_hex, b"hello-from-python")
        .await
        .expect("Failed to send Python→Rust");

    let data = wait_for_data_event(&mut event_rx, stream.link_id(), Duration::from_secs(10))
        .await
        .expect("Should receive data from Python within 10s — Fix 1 (link-addressed Data delivery) not working");
    assert_eq!(
        data, b"hello-from-python",
        "Rust should receive exact data from Python"
    );

    // Phase 3: Channel ACK / Delivery Confirmation (proves Fix 2)
    // Step 10: Send 5 messages from Rust, each going through Channel as RawBytesMessage.
    // Space sends to avoid Busy (channel window starts small after handshake).
    for i in 0..5 {
        let msg = format!("lifecycle-ack-{}", i);
        loop {
            match stream.try_send(msg.as_bytes()).await {
                Ok(()) => break,
                Err(leviculum_std::Error::Send(
                    leviculum_core::SendError::Busy | leviculum_core::SendError::PacingDelay { .. },
                )) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => panic!("Failed to send message {}: {}", i, e),
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Step 11: Wait for delivery confirmations
    // Before Fix 2, zero LinkDeliveryConfirmed events would arrive because:
    // No proof was generated for CHANNEL packets (Fix 2a)
    // No receipt was registered (Fix 2b)
    // mark_delivered was not wired (Fix 2c)
    let confirmations = wait_for_delivery_confirmations(&mut event_rx, 3, Duration::from_secs(15));
    let confirmations = confirmations.await;
    eprintln!("Delivery confirmations received: {}/5", confirmations);
    assert!(
        confirmations >= 3,
        "Should receive at least 3 LinkDeliveryConfirmed events, got {}",
        confirmations
    );

    // Verify Python actually received the messages
    let ack_messages =
        collect_messages(&py_dest, "lifecycle-ack-", 5, Duration::from_secs(10)).await;
    assert!(
        ack_messages.len() >= 3,
        "Py-Dest should receive at least 3 of 5 messages, got {}",
        ack_messages.len()
    );

    // Phase 4: Graceful Close from Rust (proves Fix 3)
    // Step 12: Verify link is active on Python side
    let link_status = py_dest
        .get_link_status(&link_hash_hex)
        .await
        .expect("Failed to get link status");
    eprintln!("Python link state before close: {:?}", link_status.state);

    // Step 13: Graceful close, sends LINKCLOSE packet
    // Before Fix 3, close() only set a local flag. No LINKCLOSE was sent.
    stream.close().await.expect("Failed to close stream");

    // Step 14: Python should see the link close within 10s
    // (before Fix 3, Python would only see it after ~12 min keepalive timeout)
    let close_result = py_dest
        .wait_for_link_state(&link_hash_hex, "CLOSED", 10)
        .await
        .expect("Failed to wait for link state");

    assert_eq!(
        close_result.status, "reached",
        "Python should see link close within 10s (got status: {}, state: {:?})",
        close_result.status, close_result.state
    );

    // Phase 5: Remote Close from Python (proves Fix 1 for LINKCLOSE)
    // Step 15: Establish a new link
    tokio::time::sleep(Duration::from_millis(500)).await;

    let stream2 = rust_node
        .connect(&dest_hash, &signing_key)
        .await
        .expect("Second connect() should succeed");

    let link_hash2_hex = hex::encode(stream2.link_id().as_bytes());
    eprintln!("Second link established with hash: {}", link_hash2_hex);

    assert!(
        wait_for_link_established(&mut event_rx, stream2.link_id(), Duration::from_secs(15)).await,
        "Second link should establish within 15s"
    );

    // Wait for link to appear on Python side
    assert!(
        wait_for_link_on_daemon(&py_dest, &link_hash2_hex, Duration::from_secs(10)).await,
        "Py-Dest should show the second link"
    );

    // Step 16: Verify the new link works (one message each direction)
    stream2
        .try_send(b"link2-rust-to-py")
        .await
        .expect("Failed to send on second link");

    let link2_r2p =
        collect_messages(&py_dest, "link2-rust-to-py", 1, Duration::from_secs(10)).await;
    assert!(
        link2_r2p.contains("link2-rust-to-py"),
        "Second link should carry data Rust→Python"
    );

    py_dest
        .send_on_link(&link_hash2_hex, b"link2-py-to-rust")
        .await
        .expect("Failed to send Python→Rust on second link");

    let data2 = wait_for_data_event(&mut event_rx, stream2.link_id(), Duration::from_secs(10))
        .await
        .expect("Should receive data from Python on second link within 10s");
    assert_eq!(
        data2, b"link2-py-to-rust",
        "Second link should carry data Python→Rust"
    );

    // Step 17: Python closes the link
    // Before Fix 1, the LINKCLOSE (a link-addressed Data packet with context=Linkclose)
    // was silently dropped by Transport::handle_data() on the non-transport Rust node.
    // The link would stay open forever (until ~12 min keepalive timeout).
    py_dest
        .close_link(&link_hash2_hex)
        .await
        .expect("Failed to close link from Python");

    // Step 18: Should receive LinkClosed event within 5s
    assert!(
        wait_for_link_closed_event(&mut event_rx, stream2.link_id(), Duration::from_secs(5)).await,
        "Should receive LinkClosed event after Python closes link — Fix 1 not delivering LINKCLOSE packets"
    );

    // Clean up
    rust_node.stop().await.expect("Failed to stop Rust node");
}
