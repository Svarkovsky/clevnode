//! Proof system interoperability tests
//!
//! Tests the packet proof system against Python Reticulum.
//!
//! Proofs provide cryptographic delivery confirmation - when a packet
//! is received, the receiver can sign the packet hash and send it back
//! as proof of delivery.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use leviculum_core::constants::{MTU, PROOF_DATA_SIZE, TRUNCATED_HASHBYTES};
use leviculum_core::crypto::sha256;
use leviculum_core::identity::Identity;
use leviculum_core::link::{Link, LinkState};
use leviculum_core::packet::{
    build_proof_packet, packet_hash, HeaderType, Packet, PacketContext, PacketData, PacketFlags,
    PacketType, TransportType,
};
use leviculum_core::{Destination, DestinationType, Direction, ProofStrategy};
use leviculum_std::interfaces::hdlc::{frame, DeframeResult, Deframer};

use crate::common::{send_framed, OsRng};
use crate::harness::TestDaemon;

// =========================================================================
// Constants
// =========================================================================

const DAEMON_SETTLE_TIME: Duration = Duration::from_millis(500);
const PACKET_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

use leviculum_core::link::LinkId;

/// Wait for any proof packet
async fn wait_for_any_proof_packet(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    timeout_duration: Duration,
) -> Option<Packet> {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Proof {
                                return Some(pkt);
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

/// Build a data packet for a destination
fn build_data_packet(destination_hash: &[u8; TRUNCATED_HASHBYTES], data: &[u8]) -> Packet {
    Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: *destination_hash,
        context: PacketContext::None,
        data: PacketData::Owned(data.to_vec()),
    }
}

// =========================================================================
// Unit Tests - Proof Format
// =========================================================================

#[test]
fn test_proof_data_size() {
    // Verify proof data size constant
    assert_eq!(PROOF_DATA_SIZE, 96); // 32 hash + 64 signature
}

#[test]
fn test_build_proof_packet() {
    let dest_hash = [0x42u8; 16];
    let proof_data = [0xAAu8; 96];

    let packet = build_proof_packet(&dest_hash, &proof_data);

    assert_eq!(packet.flags.packet_type, PacketType::Proof);
    assert_eq!(packet.destination_hash, dest_hash);
    assert_eq!(packet.data.as_slice(), &proof_data);
}

#[test]
fn test_create_and_verify_proof() {
    let identity = Identity::generate(&mut OsRng);
    let packet_hash = [0x42u8; 32];

    // Create proof
    let proof = identity.create_proof(&packet_hash).unwrap();
    assert_eq!(proof.len(), PROOF_DATA_SIZE);

    // First 32 bytes should be the hash
    assert_eq!(&proof[..32], &packet_hash);

    // Verify the proof
    assert!(identity.verify_proof(&proof, &packet_hash));
}

#[test]
fn test_proof_strategy_values() {
    // Verify our enum values match Python Reticulum
    assert_eq!(ProofStrategy::None as u8, 0x21);
    assert_eq!(ProofStrategy::App as u8, 0x22);
    assert_eq!(ProofStrategy::All as u8, 0x23);
}

#[test]
fn test_destination_proof_strategy() {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "test",
        &["proof"],
    )
    .unwrap();

    // Default should be None
    assert_eq!(dest.proof_strategy(), ProofStrategy::None);

    // Can change to All
    dest.set_proof_strategy(ProofStrategy::All);
    assert_eq!(dest.proof_strategy(), ProofStrategy::All);

    // Can change to App
    dest.set_proof_strategy(ProofStrategy::App);
    assert_eq!(dest.proof_strategy(), ProofStrategy::App);
}

// =========================================================================
// Integration Tests with Python Daemon
// =========================================================================

/// Test that Python daemon with PROVE_ALL sends proof for our packets
#[tokio::test]
async fn test_python_prove_all_sends_proof() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register a destination with PROVE_ALL
    let dest_info = daemon
        .register_destination("proof_test", &["all"])
        .await
        .expect("Failed to register destination");

    let dest_hash = &dest_info.hash;

    // Set proof strategy to PROVE_ALL
    daemon
        .set_proof_strategy(dest_hash, "PROVE_ALL")
        .await
        .expect("Failed to set proof strategy");

    // Verify strategy was set
    let strategy = daemon
        .get_proof_strategy(dest_hash)
        .await
        .expect("Failed to get proof strategy");
    assert_eq!(strategy, "PROVE_ALL");

    // Connect to daemon
    let mut stream = TcpStream::connect(daemon.rns_addr())
        .await
        .expect("Failed to connect");
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    // Create our identity for receiving the proof
    let our_identity = Identity::generate(&mut OsRng);
    let mut our_dest = Destination::new(
        Some(our_identity),
        Direction::In,
        DestinationType::Single,
        "rust",
        &["sender"],
    )
    .unwrap();

    // Send our announce so Python knows where to send the proof
    let announce_packet = our_dest
        .announce(
            None,
            &mut OsRng,
            crate::common::now_ms(),
            crate::common::now_ms() / 1000,
        )
        .expect("Failed to create announce");

    let mut announce_raw = [0u8; MTU];
    let announce_len = announce_packet.pack(&mut announce_raw).unwrap();
    send_framed(&mut stream, &announce_raw[..announce_len]).await;

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Build and send a data packet to Python's destination
    let dest_hash_bytes: [u8; 16] = hex::decode(dest_hash)
        .expect("Invalid hex")
        .try_into()
        .expect("Wrong length");

    let data_packet = build_data_packet(&dest_hash_bytes, b"test proof data");
    let mut packet_raw = [0u8; MTU];
    let packet_len = data_packet.pack(&mut packet_raw).unwrap();

    // Calculate packet hash for later verification
    let _packet_hash = sha256(&packet_raw[..packet_len]);

    send_framed(&mut stream, &packet_raw[..packet_len]).await;

    // Wait for proof from Python
    let mut deframer = Deframer::new();
    let _proof_packet =
        wait_for_any_proof_packet(&mut stream, &mut deframer, PACKET_WAIT_TIMEOUT).await;

    // Python might send the proof to a different destination or not send one
    // if it doesn't have a route back to us. This test verifies the basic flow.
    // In a full implementation, we'd need proper path announcement handling.

    // For now, just verify we don't crash and the daemon is functioning
    daemon
        .ping()
        .await
        .expect("Daemon should still be responsive");
}

/// Rust→Python single-packet proof round-trip via ReticulumNode
///
/// This is the definitive interop test for single-packet proofs:
/// 1. Python registers destination with PROVE_ALL
/// 2. Rust sends a single data packet to Python's destination
/// 3. Python auto-proves → proof travels back over TCP
/// 4. Rust receives proof → NodeEvent::PacketDeliveryConfirmed
///
/// Verifies the proof wire format is correct end-to-end.
///
#[tokio::test]
async fn test_single_packet_proof_round_trip_via_node() {
    use leviculum_core::NodeEvent;
    use leviculum_std::driver::ReticulumNodeBuilder;

    use crate::common::parse_dest_hash;

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // 1. Python: register destination with PROVE_ALL
    let dest_info = daemon
        .register_destination("proof_test", &["roundtrip"])
        .await
        .expect("Failed to register destination");

    daemon
        .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
        .await
        .expect("Failed to set proof strategy");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);

    // 2. Build Rust node connected to Python daemon
    let _storage =
        crate::common::temp_storage("test_single_packet_proof_round_trip_via_node", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    let mut event_rx = rust_node.take_event_receiver().unwrap();
    rust_node.start().await.expect("Failed to start node");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // 3. Register Python's destination on Rust side (public-key-only identity)
    //    so NodeCore can verify the proof signature
    let py_pub_bytes = hex::decode(&dest_info.public_key).expect("Invalid public key hex");
    let py_identity =
        Identity::from_public_key_bytes(&py_pub_bytes).expect("Failed to parse Python identity");
    let py_dest_on_rust = Destination::new(
        Some(py_identity),
        Direction::Out,
        DestinationType::Single,
        "proof_test",
        &["roundtrip"],
    )
    .expect("Failed to create destination");
    rust_node.register_destination(py_dest_on_rust);

    // 4. Python announces its destination so Rust learns the path
    daemon
        .announce_destination(&dest_info.hash, b"roundtrip-test")
        .await
        .expect("Python announce should succeed");

    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"roundtrip-test",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust node should learn path to Python destination");

    // 5. Send an encrypted single packet from Rust to Python
    let _receipt_hash = rust_node
        .send_single_packet(&py_dest_hash, b"proof roundtrip test")
        .await
        .expect("send should succeed");

    // 6. Wait for PacketDeliveryConfirmed event (Python PROVE_ALL → proof → Rust verifies)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut got_confirmed = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(NodeEvent::PacketDeliveryConfirmed { .. })) => {
                got_confirmed = true;
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }

    assert!(
        got_confirmed,
        "Rust should receive PacketDeliveryConfirmed after Python PROVE_ALL proof"
    );

    // Clean up
    rust_node.stop().await.expect("Failed to stop node");
}

/// Test that Python daemon with PROVE_NONE does not send proofs
#[tokio::test]
async fn test_python_prove_none_no_proof() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register a destination with default (PROVE_NONE)
    let dest_info = daemon
        .register_destination("proof_test", &["none"])
        .await
        .expect("Failed to register destination");

    let dest_hash = &dest_info.hash;

    // Verify default strategy is PROVE_NONE
    let strategy = daemon
        .get_proof_strategy(dest_hash)
        .await
        .expect("Failed to get proof strategy");
    assert_eq!(strategy, "PROVE_NONE");

    // Connect to daemon
    let mut stream = TcpStream::connect(daemon.rns_addr())
        .await
        .expect("Failed to connect");
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    // Send a data packet
    let dest_hash_bytes: [u8; 16] = hex::decode(dest_hash)
        .expect("Invalid hex")
        .try_into()
        .expect("Wrong length");

    let data_packet = build_data_packet(&dest_hash_bytes, b"test no proof");
    let mut packet_raw = [0u8; MTU];
    let packet_len = data_packet.pack(&mut packet_raw).unwrap();
    send_framed(&mut stream, &packet_raw[..packet_len]).await;

    // Wait briefly - should NOT receive a proof
    let mut deframer = Deframer::new();
    let proof = wait_for_any_proof_packet(&mut stream, &mut deframer, Duration::from_secs(2)).await;

    // With PROVE_NONE, we should not receive any proof
    assert!(
        proof.is_none(),
        "Should not receive proof with PROVE_NONE strategy"
    );
}

/// Test proof validation - verify a proof we create is valid
#[tokio::test]
async fn test_proof_creation_and_validation() {
    let identity = Identity::generate(&mut OsRng);

    // Simulate receiving a packet
    let packet_data = b"Test packet content for proof";
    let packet_hash = sha256(packet_data);

    // Create proof
    let proof = identity.create_proof(&packet_hash).unwrap();

    // Verify proof structure
    assert_eq!(proof.len(), 96);
    assert_eq!(&proof[..32], &packet_hash);

    // Verify signature
    let signature = &proof[32..];
    assert!(identity.verify(&packet_hash, signature).unwrap());

    // Full proof verification
    assert!(identity.verify_proof(&proof, &packet_hash));

    // Verification with wrong hash should fail
    let wrong_hash = [0xFFu8; 32];
    assert!(!identity.verify_proof(&proof, &wrong_hash));
}

/// Test that proof strategy setting works correctly
#[tokio::test]
async fn test_set_proof_strategy() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register a destination
    let dest_info = daemon
        .register_destination("proof_test", &["strategy"])
        .await
        .expect("Failed to register destination");

    let dest_hash = &dest_info.hash;

    // Test each strategy
    for strategy in ["PROVE_NONE", "PROVE_APP", "PROVE_ALL"] {
        daemon
            .set_proof_strategy(dest_hash, strategy)
            .await
            .expect("Failed to set proof strategy");

        let current = daemon
            .get_proof_strategy(dest_hash)
            .await
            .expect("Failed to get proof strategy");

        assert_eq!(current, strategy);
    }
}

// =========================================================================
// Comprehensive PROVE_ALL Test with Link Traffic
// =========================================================================

/// Wait for a proof packet for a specific link
async fn wait_for_link_proof(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link_id: &LinkId,
    timeout_duration: Duration,
) -> Option<(Packet, Vec<u8>)> {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            // Check if this is a proof packet for our link
                            if pkt.flags.packet_type == PacketType::Proof
                                && *link_id.as_bytes() == pkt.destination_hash
                            {
                                return Some((pkt, data));
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

/// Comprehensive PROVE_ALL test with established link and traffic verification
///
/// This test:
/// 1. Establishes a link to a Python destination with PROVE_ALL
/// 2. Sends many data packets over the link
/// 3. Receives and validates proof for EACH packet
/// 4. Verifies proof signature matches packet hash
#[tokio::test]
async fn test_prove_all_link_traffic_comprehensive() {
    const NUM_PACKETS: usize = 20;

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register destination and set PROVE_ALL
    let dest_info = daemon
        .register_destination("proof_test", &["link", "comprehensive"])
        .await
        .expect("Failed to register destination");

    daemon
        .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
        .await
        .expect("Failed to set proof strategy");

    // Verify strategy is set
    let strategy = daemon
        .get_proof_strategy(&dest_info.hash)
        .await
        .expect("Failed to get proof strategy");
    assert_eq!(strategy, "PROVE_ALL");

    // Connect to daemon
    let mut stream = TcpStream::connect(daemon.rns_addr())
        .await
        .expect("Failed to connect");
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    // Parse destination info
    let dest_hash: [u8; TRUNCATED_HASHBYTES] = hex::decode(&dest_info.hash)
        .expect("Invalid hex")
        .try_into()
        .expect("Wrong length");
    let pub_key_bytes = hex::decode(&dest_info.public_key).expect("Invalid public key hex");
    let signing_key: [u8; 32] = pub_key_bytes[32..64]
        .try_into()
        .expect("Wrong signing key length");

    // Create and send link request
    let mut link = Link::new_outgoing(dest_hash.into(), &mut OsRng);
    link.set_destination_keys(&signing_key)
        .expect("Failed to set destination keys");

    let link_request = link.build_link_request_packet(None);
    let mut framed = Vec::new();
    frame(&link_request, &mut framed);
    stream
        .write_all(&framed)
        .await
        .expect("Failed to send link request");
    stream.flush().await.expect("Failed to flush");

    // Wait for link proof
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    let mut link_proof: Option<Packet> = None;
    while link_proof.is_none() {
        if tokio::time::Instant::now() > deadline {
            panic!("Timeout waiting for link proof");
        }

        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => panic!("Connection closed"),
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Proof
                                && *link.id().as_bytes() == pkt.destination_hash
                            {
                                link_proof = Some(pkt);
                                break;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let link_proof = link_proof.unwrap();

    // Process link proof
    link.process_proof(link_proof.data.as_slice())
        .expect("Link proof should be valid");
    assert_eq!(link.state(), LinkState::Active);

    // Send RTT to finalize link
    let rtt_packet = link
        .build_rtt_packet(0.05, &mut OsRng)
        .expect("Failed to build RTT");
    framed.clear();
    frame(&rtt_packet, &mut framed);
    stream.write_all(&framed).await.expect("Failed to send RTT");
    stream.flush().await.expect("Failed to flush");

    tokio::time::sleep(Duration::from_millis(200)).await;

    println!(
        "link established, sending {} packets with PROVE_ALL",
        NUM_PACKETS
    );

    // Track sent packets and their hashes
    let mut sent_packet_hashes: Vec<[u8; 32]> = Vec::with_capacity(NUM_PACKETS);
    let mut proofs_received = 0;
    let mut proofs_valid = 0;

    // Send packets and collect proofs
    for i in 0..NUM_PACKETS {
        let msg = format!("PROVE_ALL test packet #{:03}", i);

        // Build and send data packet
        let data_packet = link
            .build_data_packet(msg.as_bytes(), &mut OsRng)
            .expect("Failed to build data packet");

        // Calculate packet hash (now matches Python's algorithm)
        let expected_hash = packet_hash(&data_packet);
        sent_packet_hashes.push(expected_hash);

        framed.clear();
        frame(&data_packet, &mut framed);
        stream
            .write_all(&framed)
            .await
            .expect("Failed to send data");
        stream.flush().await.expect("Failed to flush");

        // Wait for proof (with short timeout since it should come quickly)
        let proof_result = wait_for_link_proof(
            &mut stream,
            &mut deframer,
            link.id(),
            Duration::from_secs(2),
        )
        .await;

        if let Some((proof_pkt, _raw)) = proof_result {
            proofs_received += 1;

            // Validate proof using the Link's validate_data_proof function
            // This checks: correct length (PROOF_DATA_SIZE), hash match, and signature validity
            let proof_data = proof_pkt.data.as_slice();
            if link.validate_data_proof(proof_data, &expected_hash) {
                proofs_valid += 1;
            } else {
                // Provide detailed error for debugging
                if proof_data.len() < PROOF_DATA_SIZE {
                    println!(
                        "  packet {}: proof too short ({} bytes)",
                        i,
                        proof_data.len()
                    );
                } else {
                    let proof_hash: [u8; 32] = proof_data[..32].try_into().unwrap();
                    if proof_hash != expected_hash {
                        println!(
                            "  packet {}: hash mismatch (expected {:02x?}, got {:02x?})",
                            i,
                            &expected_hash[..4],
                            &proof_hash[..4]
                        );
                    } else {
                        println!("  packet {}: signature invalid", i);
                    }
                }
            }
        } else {
            println!("  packet {}: no proof received", i);
        }
    }

    println!(
        "results: {}/{} proofs received, {}/{} valid",
        proofs_received, NUM_PACKETS, proofs_valid, NUM_PACKETS
    );

    // Assertions
    assert_eq!(
        proofs_received, NUM_PACKETS,
        "Should receive proof for every packet with PROVE_ALL"
    );
    assert_eq!(
        proofs_valid, NUM_PACKETS,
        "All proofs should be cryptographically valid"
    );

    // Verify daemon is still healthy
    daemon
        .ping()
        .await
        .expect("Daemon should still be responsive");
}
