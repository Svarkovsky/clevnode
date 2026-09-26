//! Responder Node interop tests: Rust as link responder via high-level API
//!
//! These tests verify the full responder path with auto-accepted incoming links
//! (Python parity), proving that:
//!
//! 1. Rust can register a destination, announce it, and have Python learn the path
//! 2. Python can create a link to the Rust destination through a relay
//! 3. `ReticulumNode::link_handle()` yields a working `LinkHandle` for the
//!    auto-accepted link after its `LinkEstablished` event
//! 4. `MessageReceived` events are routed to the `LinkHandle` (fixes silent drop)
//! 5. Bidirectional data exchange works (Python→Rust via raw data, Rust→Python via Channel)
//!
//! ## Topology
//!
//! ```text
//! Py-Initiator → Py-Relay (transport) → Rust-Node (responder, non-transport)
//! ```
//!
//! ## Running
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop responder_node_tests -- --nocapture
//! ```

use std::time::Duration;

use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationType, Direction};
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{create_link_raw, wait_for_data_event, wait_for_responder_established_link};
use crate::harness::TestDaemon;

/// Test: Rust node as responder accepting incoming links via the high-level API.
///
/// Topology: Py-Initiator → Py-Relay (transport) → Rust-Node (responder)
#[tokio::test]
async fn test_rust_node_as_responder() {
    // Phase 1: Setup
    // Start Python relay (transport node) and initiator
    let py_relay = TestDaemon::start().await.expect("Failed to start Py-Relay");
    let py_initiator = TestDaemon::start()
        .await
        .expect("Failed to start Py-Initiator");

    // Connect initiator → relay
    py_initiator
        .add_client_interface("127.0.0.1", py_relay.rns_port(), Some("ToRelay"))
        .await
        .expect("Failed to connect Initiator to Relay");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Build Rust node (non-transport) connected to relay
    let _storage = crate::common::temp_storage("test_rust_node_as_responder", "node");
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

    // Phase 2: Register + Announce
    // Create a Rust identity and destination
    let rust_identity = Identity::generate(&mut rand_core::OsRng);
    let public_key_hex = hex::encode(rust_identity.public_key_bytes());
    let mut dest = Destination::new(
        Some(rust_identity),
        Direction::In,
        DestinationType::Single,
        "respondertest",
        &["echo"],
    )
    .expect("Failed to create destination");
    dest.set_accepts_links(true);

    let dest_hash = *dest.hash();
    let dest_hash_hex = hex::encode(dest_hash.as_bytes());
    eprintln!("Rust destination hash: {}", dest_hash_hex);

    // Register and announce
    rust_node.register_destination(dest);
    rust_node
        .announce_destination(&dest_hash, Some(b"rust-responder"))
        .await
        .expect("Failed to announce destination");

    // Wait for the announce to propagate through relay to initiator, re-driving
    // the one-shot announce on a fixed cadence so a single lost announce under
    // load does not fail the test (Codeberg #105).
    let initiator_has_path = crate::common::wait_for_node_reannounce_on_daemon(
        &py_initiator,
        &dest_hash,
        &rust_node,
        b"rust-responder",
        Duration::from_secs(20),
    )
    .await;
    assert!(
        initiator_has_path,
        "Py-Initiator should learn path to Rust destination through Relay"
    );

    // Phase 3: Incoming Link
    // Python create_link blocks until the link is ACTIVE or times out.
    // We must accept the link on the Rust side concurrently, so
    // spawn create_link as a background task using a raw JSON-RPC call.
    let create_link_handle = {
        let cmd_addr = py_initiator.cmd_addr();
        let dh = dest_hash_hex.clone();
        let pk = public_key_hex.clone();
        tokio::spawn(async move { create_link_raw(cmd_addr, &dh, &pk, 30).await })
    };

    // Rust auto-accepts and proves the incoming link; wait for the responder-side
    // LinkEstablished, then mint a writable handle.
    let req_link_id = wait_for_responder_established_link(&mut event_rx, Duration::from_secs(15))
        .await
        .expect("Rust should establish incoming link within 15s");
    eprintln!("Rust established incoming link {:?}", req_link_id);
    let mut stream = rust_node.link_handle(&req_link_id);

    // Join the create_link background task, it should have succeeded by now
    let link_hash = create_link_handle
        .await
        .expect("create_link task panicked")
        .expect("Python create_link should succeed");
    eprintln!("Python link established: {}", link_hash);

    // Phase 4: Bidirectional Data
    // Python → Rust (raw data via send_on_link, produces LinkDataReceived)
    py_initiator
        .send_on_link(&link_hash, b"hello-from-python-initiator")
        .await
        .expect("Python send_on_link should succeed");

    let data = wait_for_data_event(&mut event_rx, &req_link_id, Duration::from_secs(10))
        .await
        .expect("Should receive LinkDataReceived from Python within 10s");
    assert_eq!(
        data, b"hello-from-python-initiator",
        "Rust should receive exact data from Python"
    );
    eprintln!("Rust received Python→Rust data: OK");

    // Rust → Python (via Channel / send(), produces MessageReceived on receiver)
    // This is the key test for the MessageReceived routing fix: LinkHandle::try_send()
    // uses Channel internally, which produces MessageReceived events, not LinkDataReceived.
    stream
        .try_send(b"hello-from-rust-responder")
        .await
        .expect("Rust stream.try_send() should succeed");

    // Poll Python for received packets
    let py_recv_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut rust_to_py_received = false;
    while tokio::time::Instant::now() < py_recv_deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(packets) = py_initiator.get_received_packets().await {
            for p in &packets {
                let s = String::from_utf8_lossy(&p.data);
                if s.contains("hello-from-rust-responder") {
                    rust_to_py_received = true;
                    break;
                }
            }
        }
        if rust_to_py_received {
            break;
        }
    }
    assert!(
        rust_to_py_received,
        "Python should receive data sent from Rust via Channel (proves MessageReceived fix)"
    );
    eprintln!("Python received Rust→Python data: OK");

    // Cleanup
    stream.close().await.expect("Failed to close stream");
    rust_node.stop().await.expect("Failed to stop Rust node");
    eprintln!("test_rust_node_as_responder: PASSED");
}
