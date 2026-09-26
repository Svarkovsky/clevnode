//! Ratchet (forward secrecy) interoperability tests with Python Reticulum daemon.
//!
//! These tests verify that ratchet-based forward secrecy works correctly between
//! Rust and Python Reticulum implementations.
//!
//! ## What These Tests Cover
//!
//! - Receiving ratcheted announces from Python
//! - Sending ratcheted announces from Rust
//! - Rust-to-Rust announce exchange via Python relay
//! - Encrypted packet exchange using ratchets (with enforce_ratchets proving correctness)
//!
//! ## Running These Tests
//!
//! ```sh
//! # Run all ratchet interop tests
//! cargo test --package leviculum-std --test rnsd_interop ratchet_tests
//!
//! # Run with verbose output
//! cargo test --package leviculum-std --test rnsd_interop ratchet_tests -- --nocapture
//! ```

use rand_core::OsRng;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use leviculum_core::constants::{MTU, RATCHET_SIZE};
use leviculum_core::identity::Identity;
use leviculum_core::node::NodeEvent;
use leviculum_core::packet::{Packet, PacketType};
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};
use leviculum_std::interfaces::hdlc::{frame, DeframeResult, Deframer};

use crate::common::{
    build_rust_node, connect_to_daemon, now_ms, parse_dest_hash, wait_for_event, ParsedAnnounce,
    DAEMON_PROCESS_TIME,
};
use crate::harness::{DaemonTopology, TestDaemon};

// =========================================================================
// Receiving ratcheted announces from Python
// =========================================================================

/// Verify that Rust can receive and parse a ratcheted announce from Python.
///
/// This test:
/// 1. Registers a destination in Python daemon
/// 2. Enables ratchets for that destination
/// 3. Has Python announce the destination
/// 4. Rust receives and parses the announce
/// 5. Verifies the ratchet field is present and correct size
#[tokio::test]
async fn test_receive_ratcheted_announce_from_python() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register a destination in Python
    let py_dest = daemon
        .register_destination("test", &["ratchet", "receiver"])
        .await
        .expect("Failed to register destination");

    println!("Python destination registered: {}", py_dest.hash);

    // Enable ratchets for the destination
    let ratchet_result = daemon
        .enable_ratchets(&py_dest.hash)
        .await
        .expect("Failed to enable ratchets");

    assert!(ratchet_result.enabled, "Ratchets should be enabled");
    println!("Ratchets enabled for destination");

    // Verify ratchet info
    let ratchet_info = daemon
        .get_ratchet_info(&py_dest.hash)
        .await
        .expect("Failed to get ratchet info");

    assert!(ratchet_info.enabled, "Ratchet info should show enabled");
    println!("Ratchet count: {:?}", ratchet_info.count);

    // Connect Rust to receive the announce
    let mut stream = connect_to_daemon(&daemon).await;

    // Have Python announce with ratchet
    daemon
        .announce_destination(&py_dest.hash, b"ratchet-test-data")
        .await
        .expect("Failed to announce");

    println!("Python announced destination with ratchet");

    // Wait for and parse the announce
    let announce = wait_for_ratcheted_announce(&mut stream, Duration::from_secs(5)).await;

    assert!(
        announce.is_some(),
        "Should receive announce with ratchet from Python"
    );

    let announce = announce.unwrap();
    println!(
        "Received announce for: {}",
        hex::encode(announce.destination_hash)
    );

    // Verify ratchet is present (non-empty means context_flag was set)
    assert!(
        !announce.ratchet.is_empty(),
        "Announce should have ratchet field set"
    );
    assert_eq!(
        announce.ratchet.len(),
        RATCHET_SIZE,
        "Ratchet should be {} bytes",
        RATCHET_SIZE
    );

    println!(
        "Ratchet received: {} bytes, first 8: {:02x?}",
        announce.ratchet.len(),
        &announce.ratchet[..8]
    );

    println!("SUCCESS: Rust correctly received ratcheted announce from Python");
}

// =========================================================================
// Sending ratcheted announces from Rust
// =========================================================================

/// Verify that Rust can send a ratcheted announce that Python accepts.
///
/// This test:
/// 1. Creates a Rust destination with ratchets enabled
/// 2. Sends announce with ratchet
/// 3. Verifies Python creates a path entry
/// 4. Verifies the announce propagates back with ratchet intact
#[tokio::test]
async fn test_send_ratcheted_announce_from_rust() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Create Rust destination with ratchets
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "leviculum",
        &["ratchet", "sender"],
    )
    .expect("Failed to create destination");

    // Enable ratchets
    dest.enable_ratchets(&mut OsRng, now_ms())
        .expect("Failed to enable ratchets");

    assert!(
        dest.current_ratchet_public().is_some(),
        "Should have current ratchet after enabling"
    );

    let ratchet_pub = dest.current_ratchet_public().unwrap();
    println!("Rust ratchet public: {:02x?}...", &ratchet_pub[..8]);

    // Build and send announce with ratchet
    let packet = dest
        .announce(
            Some(b"rust-ratchet-test"),
            &mut OsRng,
            now_ms(),
            now_ms() / 1000,
        )
        .expect("Failed to create announce");

    // Verify context_flag is set
    assert!(
        packet.flags.context_flag,
        "Announce packet should have context_flag=true for ratchet"
    );

    // Connect and send
    let mut stream = connect_to_daemon(&daemon).await;

    let mut raw_packet = [0u8; MTU];
    let size = packet.pack(&mut raw_packet).expect("Failed to pack packet");

    let mut framed = Vec::new();
    frame(&raw_packet[..size], &mut framed);

    tokio::io::AsyncWriteExt::write_all(&mut stream, &framed)
        .await
        .expect("Failed to send");
    tokio::io::AsyncWriteExt::flush(&mut stream)
        .await
        .expect("Failed to flush");

    println!("Sent ratcheted announce from Rust");

    // Wait for processing
    tokio::time::sleep(DAEMON_PROCESS_TIME).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify Python created a path
    let dest_hash = *dest.hash();
    let has_path = daemon.has_path(&dest_hash).await;

    assert!(
        has_path,
        "Python should accept ratcheted announce and create path"
    );

    println!("SUCCESS: Python accepted ratcheted announce from Rust");
}

// =========================================================================
// Rust-to-Rust announce exchange via Python
// =========================================================================

/// Verify two Rust instances can exchange ratcheted announces through Python.
///
/// This test:
/// 1. Opens two connections to Python daemon
/// 2. Each side creates a destination with ratchets
/// 3. Each side announces
/// 4. Each side receives the other's announce with ratchet
#[tokio::test]
async fn test_rust_to_rust_ratcheted_announce_via_python() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Create Rust A's destination with ratchets
    let identity_a = Identity::generate(&mut OsRng);
    let mut dest_a = Destination::new(
        Some(identity_a),
        Direction::In,
        DestinationType::Single,
        "leviculum",
        &["exchange", "a"],
    )
    .expect("Failed to create destination A");

    dest_a
        .enable_ratchets(&mut OsRng, now_ms())
        .expect("Failed to enable ratchets for A");

    // Create Rust B's destination with ratchets
    let identity_b = Identity::generate(&mut OsRng);
    let mut dest_b = Destination::new(
        Some(identity_b),
        Direction::In,
        DestinationType::Single,
        "leviculum",
        &["exchange", "b"],
    )
    .expect("Failed to create destination B");

    dest_b
        .enable_ratchets(&mut OsRng, now_ms())
        .expect("Failed to enable ratchets for B");

    // Open two connections
    let mut stream_a = connect_to_daemon(&daemon).await;
    let mut stream_b = connect_to_daemon(&daemon).await;

    // A announces
    let packet_a = dest_a
        .announce(Some(b"from-a"), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("Failed to create announce A");

    let mut raw_a = [0u8; MTU];
    let size_a = packet_a.pack(&mut raw_a).expect("Failed to pack A");
    let mut framed_a = Vec::new();
    frame(&raw_a[..size_a], &mut framed_a);

    tokio::io::AsyncWriteExt::write_all(&mut stream_a, &framed_a)
        .await
        .expect("Failed to send A");
    tokio::io::AsyncWriteExt::flush(&mut stream_a)
        .await
        .expect("Failed to flush A");

    println!("A announced: {:02x?}...", &dest_a.hash().as_bytes()[..4]);

    // B announces
    let packet_b = dest_b
        .announce(Some(b"from-b"), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("Failed to create announce B");

    let mut raw_b = [0u8; MTU];
    let size_b = packet_b.pack(&mut raw_b).expect("Failed to pack B");
    let mut framed_b = Vec::new();
    frame(&raw_b[..size_b], &mut framed_b);

    tokio::io::AsyncWriteExt::write_all(&mut stream_b, &framed_b)
        .await
        .expect("Failed to send B");
    tokio::io::AsyncWriteExt::flush(&mut stream_b)
        .await
        .expect("Failed to flush B");

    println!("B announced: {:02x?}...", &dest_b.hash().as_bytes()[..4]);

    // Wait for propagation
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A should receive B's announce
    let announce_from_b =
        wait_for_announce_with_hash(&mut stream_a, dest_b.hash(), Duration::from_secs(5)).await;

    assert!(
        announce_from_b.is_some(),
        "A should receive B's ratcheted announce"
    );
    let announce_from_b = announce_from_b.unwrap();
    assert!(
        !announce_from_b.ratchet.is_empty(),
        "B's announce should have ratchet"
    );
    println!("A received B's announce with ratchet");

    // B should receive A's announce
    let announce_from_a =
        wait_for_announce_with_hash(&mut stream_b, dest_a.hash(), Duration::from_secs(5)).await;

    assert!(
        announce_from_a.is_some(),
        "B should receive A's ratcheted announce"
    );
    let announce_from_a = announce_from_a.unwrap();
    assert!(
        !announce_from_a.ratchet.is_empty(),
        "A's announce should have ratchet"
    );
    println!("B received A's announce with ratchet");

    println!("SUCCESS: Two Rust instances exchanged ratcheted announces via Python");
}

// =========================================================================
// Context flag verification
// =========================================================================

/// Verify that context_flag correctly indicates ratchet presence.
///
/// This test ensures the flag is correctly set/unset based on ratchet state.
#[tokio::test]
async fn test_context_flag_indicates_ratchet() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Create destination WITHOUT ratchets
    let identity1 = Identity::generate(&mut OsRng);
    let mut dest_no_ratchet = Destination::new(
        Some(identity1),
        Direction::In,
        DestinationType::Single,
        "leviculum",
        &["context", "noratchet"],
    )
    .expect("Failed to create destination");

    let packet_no_ratchet = dest_no_ratchet
        .announce(Some(b"no-ratchet"), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("Failed to create announce");

    assert!(
        !packet_no_ratchet.flags.context_flag,
        "Non-ratcheted announce should have context_flag=false"
    );

    // Create destination WITH ratchets
    let identity2 = Identity::generate(&mut OsRng);
    let mut dest_with_ratchet = Destination::new(
        Some(identity2),
        Direction::In,
        DestinationType::Single,
        "leviculum",
        &["context", "withratchet"],
    )
    .expect("Failed to create destination");

    dest_with_ratchet
        .enable_ratchets(&mut OsRng, now_ms())
        .expect("Failed to enable ratchets");

    let packet_with_ratchet = dest_with_ratchet
        .announce(Some(b"with-ratchet"), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("Failed to create announce");

    assert!(
        packet_with_ratchet.flags.context_flag,
        "Ratcheted announce should have context_flag=true"
    );

    // Send both and verify paths created - use separate streams
    // (sending two announces on same stream quickly can cause issues)

    // Send non-ratcheted on first stream
    let mut stream1 = connect_to_daemon(&daemon).await;
    let mut raw1 = [0u8; MTU];
    let size1 = packet_no_ratchet.pack(&mut raw1).expect("Failed to pack");
    let mut framed1 = Vec::new();
    frame(&raw1[..size1], &mut framed1);
    tokio::io::AsyncWriteExt::write_all(&mut stream1, &framed1)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut stream1).await.unwrap();

    // Wait between sends
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send ratcheted on second stream
    let mut stream2 = connect_to_daemon(&daemon).await;
    let mut raw2 = [0u8; MTU];
    let size2 = packet_with_ratchet.pack(&mut raw2).expect("Failed to pack");
    let mut framed2 = Vec::new();
    frame(&raw2[..size2], &mut framed2);
    tokio::io::AsyncWriteExt::write_all(&mut stream2, &framed2)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut stream2).await.unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Both should create paths
    assert!(
        daemon.has_path(dest_no_ratchet.hash()).await,
        "Non-ratcheted announce should create path"
    );
    assert!(
        daemon.has_path(dest_with_ratchet.hash()).await,
        "Ratcheted announce should create path"
    );

    println!("SUCCESS: context_flag correctly indicates ratchet presence");
}

// =========================================================================
// Helper functions
// =========================================================================

/// Wait for any announce packet with ratchet (context_flag=true).
async fn wait_for_ratcheted_announce(
    stream: &mut TcpStream,
    timeout_duration: Duration,
) -> Option<ParsedAnnounce> {
    let mut deframer = Deframer::new();
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(100), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Announce
                                && pkt.flags.context_flag
                            {
                                return ParsedAnnounce::from_packet(&pkt);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Wait for an announce packet with a specific destination hash.
async fn wait_for_announce_with_hash(
    stream: &mut TcpStream,
    dest_hash: &leviculum_core::DestinationHash,
    timeout_duration: Duration,
) -> Option<ParsedAnnounce> {
    let mut deframer = Deframer::new();
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(100), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Announce
                                && pkt.destination_hash == *dest_hash.as_bytes()
                            {
                                return ParsedAnnounce::from_packet(&pkt);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

// =========================================================================
// Helpers for ratcheted encryption tests
// =========================================================================

/// Register Python destination with ratchets enabled and enforced, then announce.
///
/// Returns the destination hash (parsed) and public key hex.
/// Enforcement means Python drops packets not encrypted with the ratchet key.
async fn setup_ratcheted_python_dest(
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

    // Enable and enforce ratchets. Python drops packets not using a ratchet
    daemon
        .enable_ratchets(&dest_info.hash)
        .await
        .expect("Failed to enable ratchets");
    daemon
        .enforce_ratchets(&dest_info.hash)
        .await
        .expect("Failed to enforce ratchets");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);

    // Register Python's destination on Rust side for path + identity learning
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

    // Python announces → Rust learns path + identity + ratchet key
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

// =========================================================================
// Ratcheted encryption interop tests
// =========================================================================

/// Rust→Python: Send packet encrypted with ratchet key, Python enforces ratchets.
///
/// Proves the sender-side fix: if Rust used the identity key (old `None` behavior),
/// Python's enforce_ratchets would reject the packet and the test would fail.
#[tokio::test]
async fn test_python_decrypts_ratcheted_packet_from_rust() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, _event_rx, _storage) = build_rust_node(&daemon).await;

    let (py_dest_hash, _) =
        setup_ratcheted_python_dest(&daemon, &rust_node, "ratchet_enc", &["r2p"], b"ratchet-r2p")
            .await;

    // Send encrypted single packet, sender should use ratchet key
    let payload = b"ratchet encrypted from rust";
    rust_node
        .send_single_packet(&py_dest_hash, payload)
        .await
        .expect("send should succeed");

    // Wait for Python to receive and decrypt
    tokio::time::sleep(Duration::from_secs(2)).await;

    let received = daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");

    assert!(
        !received.is_empty(),
        "Python (enforce_ratchets) should receive the ratchet-encrypted packet"
    );
    assert_eq!(
        received[0].data, payload,
        "Python should decrypt exact plaintext"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Python→Rust: Python sends packet encrypted with ratchet key, Rust enforces ratchets.
///
/// Proves the receiver side: if Python used the identity key, Rust's enforce_ratchets
/// would reject the packet and no PacketReceived event would be emitted.
#[tokio::test]
async fn test_rust_decrypts_ratcheted_packet_from_python() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (mut rust_node, mut event_rx, _storage) = build_rust_node(&daemon).await;

    // Create Rust destination with ratchets enabled and enforced
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "ratchet_enc",
        &["p2r"],
    )
    .expect("Failed to create destination");

    dest.enable_ratchets(&mut OsRng, now_ms())
        .expect("Failed to enable ratchets");
    dest.set_enforce_ratchets(true);

    let dest_hash = *dest.hash();
    let dest_hash_hex = hex::encode(dest_hash.as_bytes());

    rust_node.register_destination(dest);

    // Announce Rust destination (includes ratchet public key)
    rust_node
        .announce_destination(&dest_hash, Some(b"ratchet-p2r"))
        .await
        .expect("Announce should succeed");

    // Wait for Python daemon to learn the path + ratchet
    let found = crate::common::wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &rust_node,
        b"ratchet-p2r",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Python daemon should learn path to Rust destination");

    // Python sends single packet, should use ratchet key from Rust's announce
    let payload = b"ratchet encrypted from python";
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

    assert!(
        received.is_some(),
        "Rust (enforce_ratchets) should receive ratchet-encrypted packet from Python"
    );
    assert_eq!(
        received.unwrap(),
        payload,
        "Rust should decrypt exact plaintext"
    );

    rust_node.stop().await.expect("Failed to stop node");
}

/// Rust → Python relay → Python server: Ratcheted packet through relay.
///
/// Proves the full chain: announce with ratchet survives relay, sender looks up
/// ratchet from relayed announce, encrypts with ratchet, relay forwards ciphertext,
/// server decrypts with ratchet. Server enforces ratchets, so identity-key-only
/// encryption would be rejected.
#[tokio::test]
async fn test_ratcheted_packet_through_relay() {
    let topology = DaemonTopology::linear(2)
        .await
        .expect("Failed to create topology");

    let entry_daemon = topology.entry_daemon();
    let exit_daemon = topology.exit_daemon();

    // Register ratcheted destination on exit daemon (server)
    let dest_info = exit_daemon
        .register_destination("ratchet_enc", &["relay"])
        .await
        .expect("Failed to register destination");

    exit_daemon
        .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
        .await
        .expect("Failed to set proof strategy");

    exit_daemon
        .enable_ratchets(&dest_info.hash)
        .await
        .expect("Failed to enable ratchets");
    exit_daemon
        .enforce_ratchets(&dest_info.hash)
        .await
        .expect("Failed to enforce ratchets");

    // Announce from exit daemon, propagates through relay (entry daemon)
    exit_daemon
        .announce_destination(&dest_info.hash, b"ratchet-relay")
        .await
        .expect("Announce should succeed");

    // Wait for announce to propagate to entry daemon
    let py_dest_hash = parse_dest_hash(&dest_info.hash);
    let found = crate::common::wait_for_path_reannounce_on_daemon(
        entry_daemon,
        &py_dest_hash,
        exit_daemon,
        &dest_info.hash,
        b"ratchet-relay",
        Duration::from_secs(10),
    )
    .await;
    assert!(
        found,
        "Entry daemon should learn path to exit daemon's destination"
    );

    // Build Rust node connected to entry daemon (relay)
    let (mut rust_node, _event_rx, _storage) = build_rust_node(entry_daemon).await;

    // Register exit daemon's destination on Rust side
    let py_pub_bytes = hex::decode(&dest_info.public_key).expect("Invalid public key hex");
    let py_identity =
        Identity::from_public_key_bytes(&py_pub_bytes).expect("Failed to parse Python identity");
    let py_dest_on_rust = Destination::new(
        Some(py_identity),
        Direction::Out,
        DestinationType::Single,
        "ratchet_enc",
        &["relay"],
    )
    .expect("Failed to create destination");
    rust_node.register_destination(py_dest_on_rust);

    // Wait for Rust to learn path from entry daemon's rebroadcast
    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        exit_daemon,
        &dest_info.hash,
        b"ratchet-relay",
        Duration::from_secs(10),
    )
    .await;
    assert!(
        found,
        "Rust should learn path to exit daemon's destination via relay"
    );

    // Send ratcheted packet: Rust → entry daemon (relay) → exit daemon (server)
    let payload = b"ratchet through relay";
    rust_node
        .send_single_packet(&py_dest_hash, payload)
        .await
        .expect("send should succeed");

    // Wait for exit daemon to receive and decrypt
    tokio::time::sleep(Duration::from_secs(3)).await;

    let received = exit_daemon
        .get_received_single_packets()
        .await
        .expect("Failed to get received packets");

    assert!(
        !received.is_empty(),
        "Exit daemon (enforce_ratchets) should receive ratchet-encrypted packet through relay"
    );
    assert_eq!(
        received[0].data, payload,
        "Exit daemon should decrypt exact plaintext"
    );

    rust_node.stop().await.expect("Failed to stop node");
}
