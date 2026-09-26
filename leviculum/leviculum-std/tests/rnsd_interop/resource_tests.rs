//! # RNS.Resource Interop Tests: Rust ↔ Python
//!
//! ## Metadata Encoding: Rust ↔ Python
//!
//! Python's Resource constructor (Resource.py:258-264) calls
//! `umsgpack.packb(metadata)` BEFORE prepending the 3-byte BE uint24 length.
//! Wire format: `[3-byte len][msgpack-encoded metadata][data]`.
//!
//! Python's assemble() (Resource.py:687-720) strips the 3-byte prefix,
//! saves packed bytes, then calls `umsgpack.unpackb()` to decode.
//!
//! Rust's OutgoingResource::new() takes metadata as `&[u8]` documented as
//! "msgpack-encoded by caller". Rust's IncomingResource::assemble() strips
//! the 3-byte prefix and returns raw (msgpack-encoded) bytes.
//!
//! Therefore:
//! - Rust→Python: caller must msgpack-encode metadata before send_resource()
//! - Python→Rust: ResourceCompleted.metadata contains msgpack-encoded bytes

use std::time::Duration;

use leviculum_std::EventReceiver;

use leviculum_core::identity::Identity;
use leviculum_core::link::LinkId;
use leviculum_core::resource::ResourceStrategy;
use leviculum_core::{Destination, DestinationType, Direction};
use leviculum_std::driver::{ReticulumNode, ReticulumNodeBuilder};

use crate::common::{
    create_link_raw, wait_for_resource_completed, wait_for_resource_sender_completed,
    wait_for_responder_established_link,
};
use crate::harness::TestDaemon;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Msgpack-encode a byte slice as a bin value.
/// Returns the raw msgpack bytes (bin8/bin16/bin32 format).
fn msgpack_encode_bin(data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &rmpv::Value::Binary(data.to_vec()))
        .expect("msgpack encode should not fail");
    buf
}

/// Decode msgpack bytes and extract the inner Binary value.
fn msgpack_decode_bin(data: &[u8]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(data);
    let value = rmpv::decode::read_value(&mut cursor).expect("msgpack decode should not fail");
    match value {
        rmpv::Value::Binary(b) => b,
        other => panic!("expected msgpack Binary, got: {:?}", other),
    }
}

/// Set up topology and establish a link from Python to Rust.
///
/// Topology: `Py-Initiator → Py-Relay (transport) → Rust-Node (responder)`
///
/// If `accept_resources` is true, calls `set_resource_strategy("accept_all")`
/// on the Python initiator BEFORE creating the link, so that the
/// `_on_link_established` callback configures ACCEPT_ALL on the new link.
///
/// Returns `(rust_node, event_rx, py_initiator, py_relay, link_id, py_link_hash, dest_hash_hex, _storage)`.
async fn setup_link(
    accept_resources: bool,
) -> (
    ReticulumNode,
    EventReceiver,
    TestDaemon,
    TestDaemon,
    LinkId,
    String,
    String,
    tempfile::TempDir,
) {
    crate::common::init_tracing();

    // Start Python relay and initiator
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

    // Build Rust node connected to relay
    let _storage = crate::common::temp_storage("setup_link", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(py_relay.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust node");

    let event_rx = rust_node
        .take_event_receiver()
        .expect("Event receiver should be available");

    rust_node.start().await.expect("Failed to start Rust node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create Rust destination that accepts links
    let rust_identity = Identity::generate(&mut rand_core::OsRng);
    let public_key_hex = hex::encode(rust_identity.public_key_bytes());
    let mut dest = Destination::new(
        Some(rust_identity),
        Direction::In,
        DestinationType::Single,
        "resourcetest",
        &["interop"],
    )
    .expect("Failed to create destination");
    dest.set_accepts_links(true);

    let dest_hash = *dest.hash();
    let dest_hash_hex = hex::encode(dest_hash.as_bytes());
    eprintln!("Rust destination hash: {dest_hash_hex}");

    // Register and announce
    rust_node.register_destination(dest);
    rust_node
        .announce_destination(&dest_hash, Some(b"resource-test"))
        .await
        .expect("Failed to announce destination");

    // Wait for path to propagate to initiator, re-driving the one-shot announce
    // on a fixed cadence so a single lost announce under load does not fail the
    // test (Codeberg #105).
    let has_path = crate::common::wait_for_node_reannounce_on_daemon(
        &py_initiator,
        &dest_hash,
        &rust_node,
        b"resource-test",
        Duration::from_secs(20),
    )
    .await;
    assert!(
        has_path,
        "Py-Initiator should learn path to Rust destination"
    );

    // If accepting resources, set strategy BEFORE creating the link.
    // No await between set_resource_strategy and create_link.
    if accept_resources {
        let strategy_result = py_initiator
            .set_resource_strategy(&dest_hash_hex, "accept_all")
            .await
            .expect("set_resource_strategy should succeed");
        assert_eq!(
            strategy_result.as_str(),
            Some("ok"),
            "set_resource_strategy should return ok"
        );
    }

    // Spawn create_link as background task
    let create_link_handle = {
        let cmd_addr = py_initiator.cmd_addr();
        let dh = dest_hash_hex.clone();
        let pk = public_key_hex.clone();
        tokio::spawn(async move { create_link_raw(cmd_addr, &dh, &pk, 30).await })
    };

    // Rust auto-accepts and proves the incoming link; wait for it to establish.
    let mut event_rx = event_rx;
    let req_link_id = wait_for_responder_established_link(&mut event_rx, Duration::from_secs(15))
        .await
        .expect("Rust should establish incoming link within 15s");
    let _stream = rust_node.link_handle(&req_link_id);

    // Join background task to get Python's link hash
    let py_link_hash = create_link_handle
        .await
        .expect("create_link task panicked")
        .expect("Python create_link should succeed");
    eprintln!("Link established: Rust={req_link_id:?}, Python={py_link_hash}");

    (
        rust_node,
        event_rx,
        py_initiator,
        py_relay,
        req_link_id,
        py_link_hash,
        dest_hash_hex,
        _storage,
    )
}

/// Poll `get_received_resources` until at least one resource with status "complete"
/// appears, or until timeout. Returns the list of completed resources.
async fn wait_for_python_resource(
    daemon: &TestDaemon,
    timeout: Duration,
) -> Vec<crate::harness::ReceivedResource> {
    wait_for_python_resources(daemon, 1, timeout).await
}

/// Poll `get_received_resources` until at least `min_count` resources with
/// status "complete" appear, or until timeout. Returns the completed list.
async fn wait_for_python_resources(
    daemon: &TestDaemon,
    min_count: usize,
    timeout: Duration,
) -> Vec<crate::harness::ReceivedResource> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(resources) = daemon.get_received_resources().await {
            let complete: Vec<_> = resources
                .into_iter()
                .filter(|r| r.status == "complete")
                .collect();
            if complete.len() >= min_count {
                return complete;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    vec![]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// TEST 1: Rust sends a small resource (512 bytes), Python receives it.
#[tokio::test]
async fn test_rust_sends_resource_python_receives() {
    let (
        rust_node,
        mut event_rx,
        py_initiator,
        _py_relay,
        link_id,
        _py_link_hash,
        _dest_hash,
        _storage,
    ) = setup_link(true).await;

    let data = vec![0x42u8; 512];
    rust_node
        .send_resource(&link_id, &data, None, true)
        .await
        .expect("send_resource should succeed");

    // Wait for sender-side completion
    assert!(
        wait_for_resource_sender_completed(&mut event_rx, &link_id, Duration::from_secs(30)).await,
        "Rust sender should get ResourceCompleted"
    );

    // Wait for Python to receive the resource
    let received = wait_for_python_resource(&py_initiator, Duration::from_secs(30)).await;
    assert!(
        !received.is_empty(),
        "Python should receive at least one resource"
    );
    assert_eq!(received[0].data, data, "Data should match");
    assert!(received[0].metadata.is_none(), "Metadata should be None");
}

/// TEST 1b (Codeberg #165): Rust sends the SAME payload twice over one link;
/// a real Python receiver must deliver the exact bytes both times.
///
/// The reference receiver keys its on-disk reassembly file by the
/// advertisement's `o`, appends to it in mode "ab" and unlinks only on
/// success (Resource.py:199/:708/:744). Our generator used to fill a
/// deterministic content hash as `o`, so two transfers of identical content
/// shared one path — a leftover file (crash or raised callback between
/// append and unlink) would be appended to and delivered doubled. `o` is now
/// the salted per-transfer hash: both transfers land in distinct files and
/// each must deliver exactly the sent bytes. The corrupting arrangement
/// itself (a leftover surviving between append and unlink) cannot be staged
/// through a live receiver, so the salted-`o` rule is pinned red-first at
/// core level (`advertisement_fields_follow_reference_generation_rules`);
/// this test is the end-to-end acceptance that the new `o` interoperates.
#[tokio::test]
async fn test_rust_sends_identical_payload_twice_python_receives_both_intact() {
    let (
        rust_node,
        mut event_rx,
        py_initiator,
        _py_relay,
        link_id,
        _py_link_hash,
        _dest_hash,
        _storage,
    ) = setup_link(true).await;

    // Position-dependent bytes so any doubling or truncation changes content.
    let data: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();

    for round in 1..=2u32 {
        rust_node
            .send_resource(&link_id, &data, None, true)
            .await
            .expect("send_resource should succeed");
        assert!(
            wait_for_resource_sender_completed(&mut event_rx, &link_id, Duration::from_secs(30))
                .await,
            "Rust sender should get ResourceCompleted (round {round})"
        );
    }

    let received = wait_for_python_resources(&py_initiator, 2, Duration::from_secs(30)).await;
    assert_eq!(
        received.len(),
        2,
        "Python should receive both transfers as separate resources"
    );
    for (i, resource) in received.iter().enumerate() {
        assert_eq!(
            resource.data, data,
            "transfer {i} must deliver exactly the sent bytes (never doubled)"
        );
    }
}

/// TEST 2: Python sends a small resource (512 bytes), Rust receives it.
#[tokio::test]
async fn test_python_sends_resource_rust_receives() {
    let (
        rust_node,
        mut event_rx,
        py_initiator,
        _py_relay,
        link_id,
        py_link_hash,
        _dest_hash,
        _storage,
    ) = setup_link(false).await;

    // Set Rust side to accept all resources
    rust_node
        .set_resource_strategy(&link_id, ResourceStrategy::AcceptAll)
        .expect("set_resource_strategy should succeed");

    let data = vec![0x42u8; 512];
    py_initiator
        .send_resource(&py_link_hash, &data, None)
        .await
        .expect("Python send_resource should succeed");

    // Wait for Rust receiver-side completion
    let (received_data, received_metadata) =
        wait_for_resource_completed(&mut event_rx, &link_id, Duration::from_secs(30))
            .await
            .expect("Rust should receive ResourceCompleted");

    assert_eq!(received_data, data, "Data should match");
    assert!(received_metadata.is_none(), "Metadata should be None");
}

/// TEST 3: Rust sends resource with metadata, Python receives and decodes it.
#[tokio::test]
async fn test_rust_sends_resource_with_metadata() {
    let (
        rust_node,
        mut event_rx,
        py_initiator,
        _py_relay,
        link_id,
        _py_link_hash,
        _dest_hash,
        _storage,
    ) = setup_link(true).await;

    let data = vec![0x42u8; 256];
    let raw_metadata = b"test-meta-123";

    // Msgpack-encode the metadata (Python does umsgpack.packb() on send)
    let encoded_metadata = msgpack_encode_bin(raw_metadata);

    rust_node
        .send_resource(&link_id, &data, Some(&encoded_metadata), true)
        .await
        .expect("send_resource should succeed");

    // Wait for sender-side completion
    assert!(
        wait_for_resource_sender_completed(&mut event_rx, &link_id, Duration::from_secs(30)).await,
        "Rust sender should get ResourceCompleted"
    );

    // Wait for Python to receive the resource
    let received = wait_for_python_resource(&py_initiator, Duration::from_secs(30)).await;
    assert!(
        !received.is_empty(),
        "Python should receive at least one resource"
    );
    assert_eq!(received[0].data, data, "Data should match");
    // Python unpacks msgpack metadata and returns raw bytes hex
    assert_eq!(
        received[0].metadata.as_deref(),
        Some(raw_metadata.as_slice()),
        "Metadata should match after Python's umsgpack.unpackb()"
    );
}

/// TEST 4: Python sends resource with metadata, Rust receives msgpack-encoded bytes.
#[tokio::test]
async fn test_python_sends_resource_with_metadata() {
    let (
        rust_node,
        mut event_rx,
        py_initiator,
        _py_relay,
        link_id,
        py_link_hash,
        _dest_hash,
        _storage,
    ) = setup_link(false).await;

    rust_node
        .set_resource_strategy(&link_id, ResourceStrategy::AcceptAll)
        .expect("set_resource_strategy should succeed");

    let data = vec![0x42u8; 256];
    let raw_metadata = b"test-meta-456";

    // Python's RNS.Resource(data, link, metadata=bytes.fromhex(...)) will call
    // umsgpack.packb() on the metadata value internally.
    py_initiator
        .send_resource(&py_link_hash, &data, Some(raw_metadata))
        .await
        .expect("Python send_resource should succeed");

    // Wait for Rust receiver-side completion
    let (received_data, received_metadata) =
        wait_for_resource_completed(&mut event_rx, &link_id, Duration::from_secs(30))
            .await
            .expect("Rust should receive ResourceCompleted");

    assert_eq!(received_data, data, "Data should match");

    // Rust receives msgpack-encoded metadata, decode and verify
    let metadata_bytes = received_metadata.expect("Metadata should be present");
    let decoded = msgpack_decode_bin(&metadata_bytes);
    assert_eq!(decoded, raw_metadata, "Decoded metadata should match");
}

/// TEST 5: Rust sends a large resource (300KB), Python receives it.
/// Verifies multi-part transfer with HMU (hashmap update) exchanges.
#[tokio::test]
async fn test_rust_sends_large_resource() {
    let (
        rust_node,
        mut event_rx,
        py_initiator,
        _py_relay,
        link_id,
        _py_link_hash,
        _dest_hash,
        _storage,
    ) = setup_link(true).await;

    let data = vec![0x42u8; 300_000];
    rust_node
        .send_resource(&link_id, &data, None, true)
        .await
        .expect("send_resource should succeed");

    // Large transfer, allow 60s
    assert!(
        wait_for_resource_sender_completed(&mut event_rx, &link_id, Duration::from_secs(60)).await,
        "Rust sender should get ResourceCompleted for large resource"
    );

    let received = wait_for_python_resource(&py_initiator, Duration::from_secs(60)).await;
    assert!(
        !received.is_empty(),
        "Python should receive the large resource"
    );
    assert_eq!(
        received[0].data.len(),
        data.len(),
        "Data length should match"
    );
    assert_eq!(received[0].data, data, "Data content should match");
}

/// TEST 5b: Rust sends a resource LARGER than RESOURCE_MAX_EFFICIENT_SIZE
/// (Codeberg #27), so our sender must split it into multiple segments and a
/// Python `rncp`-style receiver must reassemble the whole file intact.
///
/// This is the real send-direction interop gap: before sender-side
/// segmentation, `lncp` could not send files >1 MiB and Python never received
/// the segments past the first. Varied (position-dependent) data makes any
/// boundary/offset error change the reassembled bytes. Python's Resource
/// machinery reassembles the segments by `original_hash` and fires its
/// resource-concluded callback exactly once with the full file.
#[tokio::test]
async fn test_rust_sends_over_max_efficient_resource_python_receives() {
    use leviculum_core::resource::RESOURCE_MAX_EFFICIENT_SIZE;

    let (
        rust_node,
        mut event_rx,
        py_initiator,
        _py_relay,
        link_id,
        _py_link_hash,
        _dest_hash,
        _storage,
    ) = setup_link(true).await;

    // ~2.2x MAX -> 3 segments. Position-dependent but compressible, so the
    // transfer stays fast while still catching any segment misalignment.
    let size = RESOURCE_MAX_EFFICIENT_SIZE * 2 + RESOURCE_MAX_EFFICIENT_SIZE / 5;
    assert!(size > 1_048_575, "must exceed one segment");
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    rust_node
        .send_resource(&link_id, &data, None, true)
        .await
        .expect("send_resource should succeed");

    // Multi-segment transfer over a real TCP hop: allow generous time.
    assert!(
        wait_for_resource_sender_completed(&mut event_rx, &link_id, Duration::from_secs(120)).await,
        "Rust sender should get ResourceCompleted after all segments"
    );

    let received = wait_for_python_resource(&py_initiator, Duration::from_secs(120)).await;
    assert!(
        !received.is_empty(),
        "Python should receive the reassembled multi-segment resource"
    );
    assert_eq!(
        received[0].data.len(),
        data.len(),
        "reassembled length must match the source file"
    );
    assert_eq!(
        received[0].data, data,
        "reassembled bytes must match the source file exactly"
    );
}

/// TEST 6: Python sends a large resource (51KB of varied data), Rust receives it.
#[tokio::test]
async fn test_python_sends_large_resource_to_rust() {
    let (
        rust_node,
        mut event_rx,
        py_initiator,
        _py_relay,
        link_id,
        py_link_hash,
        _dest_hash,
        _storage,
    ) = setup_link(false).await;

    rust_node
        .set_resource_strategy(&link_id, ResourceStrategy::AcceptAll)
        .expect("set_resource_strategy should succeed");

    // Generate varied data: bytes(range(256)) * 200 = 51200 bytes
    let data: Vec<u8> = (0..200u32).flat_map(|_| 0..=255u8).collect();
    assert_eq!(data.len(), 51200);

    py_initiator
        .send_resource(&py_link_hash, &data, None)
        .await
        .expect("Python send_resource should succeed");

    // Large transfer, allow 60s
    let (received_data, received_metadata) =
        wait_for_resource_completed(&mut event_rx, &link_id, Duration::from_secs(60))
            .await
            .expect("Rust should receive ResourceCompleted for large resource");

    assert_eq!(received_data.len(), data.len(), "Data length should match");
    assert_eq!(received_data, data, "Data content should match");
    assert!(received_metadata.is_none(), "Metadata should be None");
}
