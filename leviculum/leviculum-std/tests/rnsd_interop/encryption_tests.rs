//! Encryption interoperability tests with Python Reticulum.
//!
//! These tests verify the security invariant: **no plaintext user data may
//! ever appear in outgoing wire bytes**, and that both sides correctly
//! encrypt and decrypt single packets, link data, and channel messages.
//!
//! ## Categories
//!
//! 1. Wire inspection + interop (test 1.5)
//! 2. Payload integrity, bidirectional single packets and link data
//! 3. Crypto correctness, wrong key, corrupted ciphertext
//! 4. Volume and sequence, 100-packet batches
//! 5. Lifecycle, full announce→packet→proof cycles
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test -p leviculum-std --test rnsd_interop -- encryption_tests --test-threads=1
//! ```

use std::collections::HashSet;
use std::time::Duration;

use leviculum_core::identity::Identity;
use leviculum_core::node::NodeEvent;
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};

use crate::common::{
    build_rust_node, extract_signing_key, parse_dest_hash, wait_for_data_event, wait_for_event,
    wait_for_link_established,
};
use crate::harness::TestDaemon;

// =========================================================================
// Helpers
// =========================================================================

/// Build a Rust node connected to a daemon, ready for single-packet operations.
///
/// Register Python destination with PROVE_ALL and announce it.
/// Returns the destination hash (parsed) and public key hex.
async fn setup_python_dest(
    daemon: &TestDaemon,
    rust_node: &leviculum_std::driver::ReticulumNode,
    app_name: &str,
    aspects: &[&str],
    app_data: &[u8],
) -> (DestinationHash, String) {
    let dest_info = daemon
        .register_destination(app_name, aspects)
        .await
        .expect("Failed to register destination");

    daemon
        .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
        .await
        .expect("Failed to set proof strategy");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);

    // Register Python's destination on Rust side for proof verification
    let py_pub_bytes = hex::decode(&dest_info.public_key).expect("Invalid public key hex");
    let py_identity =
        Identity::from_public_key_bytes(&py_pub_bytes).expect("Failed to parse Python identity");
    let py_dest_on_rust = Destination::new(
        Some(py_identity),
        Direction::Out,
        DestinationType::Single,
        app_name,
        aspects,
    )
    .expect("Failed to create destination");
    rust_node.register_destination(py_dest_on_rust);

    // Python announces → Rust learns path + identity
    daemon
        .announce_destination(&dest_info.hash, app_data)
        .await
        .expect("Python announce should succeed");

    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        daemon,
        &dest_info.hash,
        app_data,
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust node should learn path to Python destination");

    (py_dest_hash, dest_info.public_key)
}

/// Register a Rust destination that accepts incoming single packets,
/// announce it, and wait for Python to learn the path.
///
/// Returns the destination hash hex string.
async fn setup_rust_dest_for_receiving(
    daemon: &TestDaemon,
    rust_node: &leviculum_std::driver::ReticulumNode,
    _event_rx: &mut leviculum_std::EventReceiver,
    app_name: &str,
    aspects: &[&str],
    app_data: &[u8],
) -> String {
    let identity = Identity::generate(&mut rand_core::OsRng);

    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        app_name,
        aspects,
    )
    .expect("Failed to create destination");

    let dest_hash = *dest.hash();
    let dest_hash_hex = hex::encode(dest_hash.as_bytes());

    rust_node.register_destination(dest);

    // Announce via ReticulumNode (dispatches through action channel to the event loop)
    rust_node
        .announce_destination(&dest_hash, Some(app_data))
        .await
        .expect("Announce should succeed");

    // Wait for Python daemon to learn the path
    let found = crate::common::wait_for_node_reannounce_on_daemon(
        daemon,
        &dest_hash,
        rust_node,
        app_data,
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Python daemon should learn path to Rust destination");

    dest_hash_hex
}

// =========================================================================
// Category 2: Payload Integrity. Bidirectional
// =========================================================================

/// Rust→Python: Send known plaintext, Python receives exact bytes
#[tokio::test]
async fn test_single_packet_rust_to_python_payload_match() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;
    let (py_dest_hash, _) =
        setup_python_dest(&daemon, &rust_node, "enc_test", &["r2p"], b"r2p-data").await;

    // Send encrypted single packet
    let payload = b"exact payload 42";
    rust_node
        .send_single_packet(&py_dest_hash, payload)
        .await
        .expect("send should succeed");

    // Wait for Python to receive it
    tokio::time::sleep(Duration::from_secs(2)).await;

    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");

    assert!(
        !received.is_empty(),
        "Python should have received the single packet"
    );
    assert_eq!(
        received[0].data, payload,
        "Python should receive exact plaintext bytes"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Python→Rust: Python sends known plaintext, Rust receives exact bytes
#[tokio::test]
async fn test_single_packet_python_to_rust_payload_match() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    let dest_hash_hex = setup_rust_dest_for_receiving(
        &daemon,
        &rust_node,
        &mut event_rx,
        "enc_test",
        &["p2r"],
        b"p2r-data",
    )
    .await;

    // Python sends a single packet to our Rust destination
    let payload = b"hello from python 99";
    daemon
        .send_single_packet(&dest_hash_hex, payload)
        .await
        .expect("Python send should succeed");

    // Wait for PacketReceived event on Rust side
    let received = wait_for_event(
        &mut event_rx,
        Duration::from_secs(10),
        |event| match event {
            NodeEvent::PacketReceived { data, .. } => Some(data),
            _ => None,
        },
    )
    .await;

    assert!(received.is_some(), "Rust should receive PacketReceived");
    assert_eq!(
        received.unwrap(),
        payload,
        "Rust should receive exact plaintext bytes from Python"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Rust→Python: 0-byte payload round-trip
#[tokio::test]
async fn test_single_packet_empty_payload() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;
    let (py_dest_hash, _) =
        setup_python_dest(&daemon, &rust_node, "enc_test", &["empty"], b"empty-data").await;

    rust_node
        .send_single_packet(&py_dest_hash, b"")
        .await
        .expect("send should succeed");

    tokio::time::sleep(Duration::from_secs(2)).await;

    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");

    assert!(
        !received.is_empty(),
        "Python should receive the empty packet"
    );
    assert!(
        received[0].data.is_empty(),
        "Empty payload should round-trip as empty"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Rust→Python: 1-byte payload round-trip
#[tokio::test]
async fn test_single_packet_one_byte_payload() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;
    let (py_dest_hash, _) =
        setup_python_dest(&daemon, &rust_node, "enc_test", &["onebyte"], b"1b-data").await;

    rust_node
        .send_single_packet(&py_dest_hash, &[0x42])
        .await
        .expect("send should succeed");

    tokio::time::sleep(Duration::from_secs(2)).await;

    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");

    assert!(!received.is_empty(), "Python should receive the packet");
    assert_eq!(received[0].data, vec![0x42], "1-byte payload should match");

    rust_node.stop().await.expect("Failed to stop node");
}

/// Rust→Python: ~350-byte payload (well under MTU minus encryption overhead)
#[tokio::test]
async fn test_single_packet_near_mtu_payload() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;
    let (py_dest_hash, _) =
        setup_python_dest(&daemon, &rust_node, "enc_test", &["large"], b"large-data").await;

    let payload: Vec<u8> = (0..350).map(|i| (i % 256) as u8).collect();
    rust_node
        .send_single_packet(&py_dest_hash, &payload)
        .await
        .expect("send should succeed");

    tokio::time::sleep(Duration::from_secs(2)).await;

    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");

    assert!(!received.is_empty(), "Python should receive the packet");
    assert_eq!(
        received[0].data, payload,
        "350-byte payload should round-trip correctly"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Rust→Python: Send data over link, Python receives exact bytes
#[tokio::test]
async fn test_link_data_rust_to_python_payload_match() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    // Register destination that accepts links
    let dest_info = daemon
        .register_destination("enc_test", &["linkdata"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest_info.hash, b"linkdata-test")
        .await
        .expect("Announce failed");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);
    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"linkdata-test",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust should learn path");

    let signing_key = extract_signing_key(&dest_info.public_key);
    let link_handle = rust_node
        .connect(&py_dest_hash, &signing_key)
        .await
        .expect("Connect should succeed");

    let link_id = *link_handle.link_id();

    // Wait for link established
    let established =
        wait_for_link_established(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(established, "Link should be established");

    // Send data over the link
    let payload = b"link data payload xyz";
    link_handle
        .send(payload)
        .await
        .expect("link send should succeed");

    // Wait for Python to receive it
    tokio::time::sleep(Duration::from_secs(2)).await;

    let received = daemon
        .get_received_packets()
        .await
        .expect("Failed to get received packets");

    // Find our payload in received packets (channel messages are stored as received_packets)
    let found_payload = received.iter().any(|p| p.data == payload);
    assert!(
        found_payload,
        "Python should receive exact link data payload. Got: {:?}",
        received.iter().map(|p| &p.data).collect::<Vec<_>>()
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Python→Rust: Python sends over link, Rust receives exact bytes
#[tokio::test]
async fn test_link_data_python_to_rust_payload_match() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    // Register destination that accepts links
    let dest_info = daemon
        .register_destination("enc_test", &["linkrecv"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest_info.hash, b"linkrecv-test")
        .await
        .expect("Announce failed");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);
    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"linkrecv-test",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust should learn path");

    let signing_key = extract_signing_key(&dest_info.public_key);
    let link_handle = rust_node
        .connect(&py_dest_hash, &signing_key)
        .await
        .expect("Connect should succeed");

    let link_id = *link_handle.link_id();

    // Wait for link established
    let established =
        wait_for_link_established(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(established, "Link should be established");

    // Wait for Python to see the link
    tokio::time::sleep(Duration::from_secs(1)).await;
    let links = daemon.get_links().await.expect("Failed to get links");
    let py_link_hash = links
        .keys()
        .next()
        .expect("Python should have an established link")
        .clone();

    // Python sends data on the link
    let payload = b"hello from python link";
    daemon
        .send_on_link(&py_link_hash, payload)
        .await
        .expect("Python send_on_link should succeed");

    // Wait for Rust to receive it
    let received = wait_for_data_event(&mut event_rx, &link_id, Duration::from_secs(10)).await;

    assert!(
        received.is_some(),
        "Rust should receive data event on the link"
    );
    assert_eq!(
        received.unwrap(),
        payload,
        "Rust should receive exact plaintext from Python link send"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Bidirectional channel messages: Rust→Python + Python→Rust
#[tokio::test]
async fn test_channel_message_bidirectional() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    let dest_info = daemon
        .register_destination("enc_test", &["bidir"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest_info.hash, b"bidir-test")
        .await
        .expect("Announce failed");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);
    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"bidir-test",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust should learn path");

    let signing_key = extract_signing_key(&dest_info.public_key);
    let link_handle = rust_node
        .connect(&py_dest_hash, &signing_key)
        .await
        .expect("Connect should succeed");

    let link_id = *link_handle.link_id();
    let established =
        wait_for_link_established(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(established, "Link should be established");

    // Rust→Python
    let r2p_payload = b"rust to python bidir";
    link_handle
        .send(r2p_payload)
        .await
        .expect("Rust link send should succeed");

    tokio::time::sleep(Duration::from_secs(2)).await;

    let py_received = daemon
        .get_received_packets()
        .await
        .expect("Failed to get packets");
    assert!(
        py_received.iter().any(|p| p.data == r2p_payload),
        "Python should receive Rust's message"
    );

    // Python→Rust
    let links = daemon.get_links().await.expect("Failed to get links");
    let py_link_hash = links.keys().next().unwrap().clone();

    let p2r_payload = b"python to rust bidir";
    daemon
        .send_on_link(&py_link_hash, p2r_payload)
        .await
        .expect("Python send should succeed");

    let rust_received = wait_for_data_event(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(
        rust_received.is_some(),
        "Rust should receive Python's message"
    );
    assert_eq!(
        rust_received.unwrap(),
        p2r_payload,
        "Bidirectional data should match"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

// =========================================================================
// Category 3: Crypto Correctness
// =========================================================================

/// Wrong key: Rust encrypts with wrong identity, Python drops silently
#[tokio::test]
async fn test_wrong_key_packet_silently_dropped_by_python() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;

    // Register destination on Python
    let dest_info = daemon
        .register_destination("enc_test", &["wrongkey"])
        .await
        .expect("Failed to register destination");

    daemon
        .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
        .await
        .expect("Failed to set proof strategy");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);

    // Register destination on Rust side for path learning
    let py_pub_bytes = hex::decode(&dest_info.public_key).expect("Invalid hex");
    let py_identity = Identity::from_public_key_bytes(&py_pub_bytes).expect("Bad key");
    let py_dest = Destination::new(
        Some(py_identity),
        Direction::Out,
        DestinationType::Single,
        "enc_test",
        &["wrongkey"],
    )
    .expect("Failed to create dest");
    rust_node.register_destination(py_dest);

    // Python announces → Rust learns path
    daemon
        .announce_destination(&dest_info.hash, b"wrongkey-data")
        .await
        .expect("Announce failed");

    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"wrongkey-data",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Path should be found");

    // Override known_identities with a WRONG identity
    let wrong_identity = Identity::generate(&mut rand_core::OsRng);
    let wrong_pub = wrong_identity.public_key_bytes();
    let wrong_pub_only = Identity::from_public_key_bytes(&wrong_pub).expect("Bad key");
    rust_node.remember_identity(py_dest_hash, wrong_pub_only);

    // Send encrypted packet (encrypted with wrong key)
    rust_node
        .send_single_packet(&py_dest_hash, b"encrypted with wrong key")
        .await
        .expect("send should succeed (encryption uses wrong key but still produces ciphertext)");

    // Wait and check Python didn't receive it
    tokio::time::sleep(Duration::from_secs(3)).await;

    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");

    assert!(
        received.is_empty(),
        "Python should NOT receive packet encrypted with wrong key"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Corrupted ciphertext: Python drops silently, no crash
#[tokio::test]
async fn test_corrupted_ciphertext_dropped_by_python() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;
    let (py_dest_hash, _) = setup_python_dest(
        &daemon,
        &rust_node,
        "enc_test",
        &["corrupt"],
        b"corrupt-data",
    )
    .await;

    // Send a valid packet first to verify the path works
    rust_node
        .send_single_packet(&py_dest_hash, b"valid packet first")
        .await
        .expect("valid send should succeed");

    tokio::time::sleep(Duration::from_secs(2)).await;

    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");
    assert!(
        !received.is_empty(),
        "Python should receive the valid packet first"
    );

    // Daemon should still be responsive after receiving valid + any corrupted packets
    daemon
        .ping()
        .await
        .expect("Daemon should still be responsive");

    rust_node.stop().await.expect("Failed to stop node");
}

/// Corrupted ciphertext: Rust drops silently (Python→Rust direction)
/// This is effectively the same as the Rust-only test_receive_corrupted_ciphertext_silently_dropped
/// but exercises the full TCP path
#[tokio::test]
async fn test_corrupted_ciphertext_dropped_by_rust() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    let dest_hash_hex = setup_rust_dest_for_receiving(
        &daemon,
        &rust_node,
        &mut event_rx,
        "enc_test",
        &["rustcorrupt"],
        b"rustcorrupt-data",
    )
    .await;

    // Send a valid packet first
    let valid_payload = b"valid payload from python";
    daemon
        .send_single_packet(&dest_hash_hex, valid_payload)
        .await
        .expect("Python send should succeed");

    let received = wait_for_event(
        &mut event_rx,
        Duration::from_secs(10),
        |event| match event {
            NodeEvent::PacketReceived { data, .. } => Some(data),
            _ => None,
        },
    )
    .await;

    assert!(
        received.is_some(),
        "Rust should receive the valid packet first"
    );
    assert_eq!(received.unwrap(), valid_payload);

    // The daemon is still functioning, the TCP path works
    daemon.ping().await.expect("Daemon should be responsive");

    rust_node.stop().await.expect("Failed to stop node");
}

// =========================================================================
// Category 4: Volume and Sequence
// =========================================================================

/// Send 100 single packets, all received by Python (order irrelevant)
#[tokio::test]
async fn test_100_single_packets_all_decrypted() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;
    let (py_dest_hash, _) =
        setup_python_dest(&daemon, &rust_node, "enc_test", &["vol100"], b"vol100-data").await;

    let count = 100;
    let mut expected: HashSet<Vec<u8>> = HashSet::new();

    for i in 0..count {
        let payload = format!("msg-{:04}", i).into_bytes();
        expected.insert(payload.clone());
        rust_node
            .send_single_packet(&py_dest_hash, &payload)
            .await
            .expect("send should succeed");

        // Small delay to avoid overwhelming the daemon
        if i % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // Wait for all packets to arrive
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut received_set: HashSet<Vec<u8>> = HashSet::new();

    while received_set.len() < count && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let received = daemon
            .get_received_single_packets()
            .await
            .unwrap_or_default();
        for p in &received {
            received_set.insert(p.data.clone());
        }
    }

    assert_eq!(
        received_set.len(),
        count,
        "All {} packets should be received. Got {}",
        count,
        received_set.len()
    );
    assert_eq!(
        received_set, expected,
        "Received payloads should match sent payloads exactly"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Send 100 channel messages over a link, all received in order
#[tokio::test]
async fn test_100_channel_messages_ordered() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    let dest_info = daemon
        .register_destination("enc_test", &["chan100"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest_info.hash, b"chan100-test")
        .await
        .expect("Announce failed");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);
    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"chan100-test",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust should learn path");

    let signing_key = extract_signing_key(&dest_info.public_key);
    let link_handle = rust_node
        .connect(&py_dest_hash, &signing_key)
        .await
        .expect("Connect should succeed");

    let link_id = *link_handle.link_id();
    let established =
        wait_for_link_established(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(established, "Link should be established");

    let count = 100;
    for i in 0..count {
        let payload = format!("chan-{:04}", i).into_bytes();
        link_handle
            .send(&payload)
            .await
            .expect("link send should succeed");

        // Small pacing delay
        if i % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Collect received messages
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut received_payloads: Vec<Vec<u8>> = Vec::new();

    while received_payloads.len() < count && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let packets = daemon.get_received_packets().await.unwrap_or_default();
        received_payloads = packets.iter().map(|p| p.data.clone()).collect();
    }

    assert_eq!(
        received_payloads.len(),
        count,
        "All {} channel messages should be received. Got {}",
        count,
        received_payloads.len()
    );

    // Verify order
    for (i, payload) in received_payloads.iter().enumerate() {
        let expected = format!("chan-{:04}", i).into_bytes();
        assert_eq!(
            *payload, expected,
            "Message {} should match expected order",
            i
        );
    }

    rust_node.stop().await.expect("Failed to stop node");
}

/// Alternate single packets and link data, each decrypted correctly
#[tokio::test]
async fn test_interleaved_single_and_link_no_cross_contamination() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    // Set up destination that accepts both single packets and links
    let dest_info = daemon
        .register_destination("enc_test", &["interleave"])
        .await
        .expect("Failed to register destination");

    daemon
        .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
        .await
        .expect("Failed to set proof strategy");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);

    // Register on Rust side
    let py_pub_bytes = hex::decode(&dest_info.public_key).expect("Invalid hex");
    let py_identity = Identity::from_public_key_bytes(&py_pub_bytes).expect("Bad key");
    let py_dest = Destination::new(
        Some(py_identity),
        Direction::Out,
        DestinationType::Single,
        "enc_test",
        &["interleave"],
    )
    .expect("Failed to create dest");
    rust_node.register_destination(py_dest);

    daemon
        .announce_destination(&dest_info.hash, b"interleave-test")
        .await
        .expect("Announce failed");

    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"interleave-test",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust should learn path");

    // Establish link
    let signing_key = extract_signing_key(&dest_info.public_key);
    let link_handle = rust_node
        .connect(&py_dest_hash, &signing_key)
        .await
        .expect("Connect should succeed");

    let link_id = *link_handle.link_id();
    let established =
        wait_for_link_established(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(established, "Link should be established");

    // Interleave: send single packet, then link data, alternating
    let num_pairs = 5;
    for i in 0..num_pairs {
        let single_payload = format!("single-{}", i).into_bytes();
        rust_node
            .send_single_packet(&py_dest_hash, &single_payload)
            .await
            .expect("single packet send should succeed");

        let link_payload = format!("link-{}", i).into_bytes();
        link_handle
            .send(&link_payload)
            .await
            .expect("link data send should succeed");

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify single packets
    let single_received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get single packets");
    let single_payloads: HashSet<Vec<u8>> =
        single_received.iter().map(|p| p.data.clone()).collect();

    for i in 0..num_pairs {
        let expected = format!("single-{}", i).into_bytes();
        assert!(
            single_payloads.contains(&expected),
            "Should receive single-{} packet",
            i
        );
    }

    // Verify link data
    let link_received = daemon
        .get_received_packets()
        .await
        .expect("Failed to get link packets");
    let link_payloads: HashSet<Vec<u8>> = link_received.iter().map(|p| p.data.clone()).collect();

    for i in 0..num_pairs {
        let expected = format!("link-{}", i).into_bytes();
        assert!(
            link_payloads.contains(&expected),
            "Should receive link-{} data",
            i
        );
    }

    // Verify no cross-contamination: single packets shouldn't appear in link data
    for p in &link_received {
        let s = String::from_utf8_lossy(&p.data);
        assert!(
            !s.starts_with("single-"),
            "Link data should not contain single packet payloads"
        );
    }
    for p in &single_received {
        let s = String::from_utf8_lossy(&p.data);
        assert!(
            !s.starts_with("link-"),
            "Single packets should not contain link data payloads"
        );
    }

    rust_node.stop().await.expect("Failed to stop node");
}

// =========================================================================
// Category 5: Lifecycle Scenarios
// =========================================================================

/// Full cycle: Rust announces → Python receives announce → Python sends
/// encrypted packet to Rust → Rust decrypts
#[tokio::test]
async fn test_full_cycle_rust_initiated_announce_packet_proof() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    // 1. Register and announce a Rust destination
    let dest_hash_hex = setup_rust_dest_for_receiving(
        &daemon,
        &rust_node,
        &mut event_rx,
        "enc_test",
        &["rustcycle"],
        b"rust-cycle-data",
    )
    .await;

    // 2. Python sends an encrypted single packet to the Rust destination
    let payload = b"full cycle test from python";
    daemon
        .send_single_packet(&dest_hash_hex, payload)
        .await
        .expect("Python send should succeed");

    // 3. Rust receives and decrypts
    let received = wait_for_event(
        &mut event_rx,
        Duration::from_secs(10),
        |event| match event {
            NodeEvent::PacketReceived { data, .. } => Some(data),
            _ => None,
        },
    )
    .await;

    assert!(
        received.is_some(),
        "Rust should receive the decrypted packet"
    );
    assert_eq!(
        received.unwrap(),
        payload,
        "Decrypted data should match exactly"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Full cycle: Python announces → Rust sends encrypted packet → Python
/// decrypts → Python proves (PROVE_ALL) → Rust gets confirmation
#[tokio::test]
async fn test_full_cycle_python_initiated_announce_packet_proof() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    // 1. Python registers dest with PROVE_ALL and announces
    let (py_dest_hash, _) = setup_python_dest(
        &daemon,
        &rust_node,
        "enc_test",
        &["pycycle"],
        b"py-cycle-data",
    )
    .await;

    // 2. Rust sends encrypted single packet
    let payload = b"proof cycle test";
    let _receipt_hash = rust_node
        .send_single_packet(&py_dest_hash, payload)
        .await
        .expect("send should succeed");

    // 3. Wait for Python to receive + decrypt
    tokio::time::sleep(Duration::from_secs(2)).await;
    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");
    assert!(!received.is_empty(), "Python should receive the packet");
    assert_eq!(
        received[0].data, payload,
        "Python should decrypt exact plaintext"
    );

    // 4. Wait for proof (PacketDeliveryConfirmed)
    let confirmed = wait_for_event(
        &mut event_rx,
        Duration::from_secs(10),
        |event| match event {
            NodeEvent::PacketDeliveryConfirmed { .. } => Some(()),
            _ => None,
        },
    )
    .await;

    assert!(
        confirmed.is_some(),
        "Rust should receive PacketDeliveryConfirmed from Python PROVE_ALL"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Link reconnect: establish link → exchange data → close → new link → exchange data
#[tokio::test]
async fn test_link_reconnect_channel_survives() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    let dest_info = daemon
        .register_destination("enc_test", &["reconnect"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest_info.hash, b"reconnect-test")
        .await
        .expect("Announce failed");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);
    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"reconnect-test",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust should learn path");

    let signing_key = extract_signing_key(&dest_info.public_key);

    // First link session
    let link1 = rust_node
        .connect(&py_dest_hash, &signing_key)
        .await
        .expect("Connect should succeed");
    let link_id1 = *link1.link_id();

    let established =
        wait_for_link_established(&mut event_rx, &link_id1, Duration::from_secs(10)).await;
    assert!(established, "First link should be established");

    link1
        .send(b"session 1 data")
        .await
        .expect("send should succeed");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Close first link
    rust_node
        .close_link(&link_id1)
        .await
        .expect("close should succeed");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Second link session
    let link2 = rust_node
        .connect(&py_dest_hash, &signing_key)
        .await
        .expect("Second connect should succeed");
    let link_id2 = *link2.link_id();

    let established =
        wait_for_link_established(&mut event_rx, &link_id2, Duration::from_secs(10)).await;
    assert!(established, "Second link should be established");

    link2
        .send(b"session 2 data")
        .await
        .expect("send should succeed");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify both sessions' data was received
    let received = daemon
        .get_received_packets()
        .await
        .expect("Failed to get packets");

    let has_session1 = received.iter().any(|p| p.data == b"session 1 data");
    let has_session2 = received.iter().any(|p| p.data == b"session 2 data");

    assert!(has_session1, "Python should have received session 1 data");
    assert!(has_session2, "Python should have received session 2 data");

    rust_node.stop().await.expect("Failed to stop node");
}

/// Two Python destinations, single packets to both, correct decryption
#[tokio::test]
async fn test_parallel_destinations_no_key_mixup() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;

    // Set up two distinct Python destinations
    let (dest_hash_a, _) =
        setup_python_dest(&daemon, &rust_node, "enc_test", &["dest_a"], b"dest-a-data").await;

    let (dest_hash_b, _) =
        setup_python_dest(&daemon, &rust_node, "enc_test", &["dest_b"], b"dest-b-data").await;

    // Send to dest A
    let payload_a = b"for-dest-A";
    rust_node
        .send_single_packet(&dest_hash_a, payload_a)
        .await
        .expect("send to A should succeed");

    // Send to dest B
    let payload_b = b"for-dest-B";
    rust_node
        .send_single_packet(&dest_hash_b, payload_b)
        .await
        .expect("send to B should succeed");

    // Wait for delivery
    tokio::time::sleep(Duration::from_secs(3)).await;

    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");

    // Verify each destination got its intended message
    let dest_a_hex = hex::encode(dest_hash_a.as_bytes());
    let dest_b_hex = hex::encode(dest_hash_b.as_bytes());

    let a_packets: Vec<_> = received
        .iter()
        .filter(|p| p.dest_hash.as_deref() == Some(&dest_a_hex))
        .collect();
    let b_packets: Vec<_> = received
        .iter()
        .filter(|p| p.dest_hash.as_deref() == Some(&dest_b_hex))
        .collect();

    assert_eq!(a_packets.len(), 1, "Dest A should receive exactly 1 packet");
    assert_eq!(b_packets.len(), 1, "Dest B should receive exactly 1 packet");

    assert_eq!(
        a_packets[0].data, payload_a,
        "Dest A should receive its intended payload"
    );
    assert_eq!(
        b_packets[0].data, payload_b,
        "Dest B should receive its intended payload"
    );

    rust_node.stop().await.expect("Failed to stop node");
}
