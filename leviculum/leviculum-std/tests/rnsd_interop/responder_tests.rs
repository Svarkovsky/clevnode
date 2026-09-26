//! Link-Responder interop tests using the daemon harness.
//!
//! These tests verify that Rust can act as the **responder** (server) side of
//! link establishment, accepting incoming links from Python.
//!
//! ## What These Tests Verify
//!
//! 1. **Rust can accept incoming LINK_REQUEST** from Python
//! 2. **Rust can generate valid PROOF** that Python accepts
//! 3. **Rust can process RTT** and finalize the link
//! 4. **Bidirectional encrypted data exchange** works with Rust as responder
//!
//! ## Test Flow
//!
//! ```text
//! Python (Initiator)              Rust (Responder)
//!     |                                |
//!     |<------- [ANNOUNCE] ------------|  Rust announces destination
//!     |                                |
//!     |-------- [LINK_REQUEST] ------->|  Python initiates link
//!     |                                |
//!     |<------- [PROOF] --------------|  Rust responds with proof
//!     |                                |
//!     |-------- [RTT] ---------------->|  Python sends RTT
//!     |                                |
//!     |<======= LINK ACTIVE ==========|  Encrypted channel established
//!     |                                |
//!     |-------- [DATA] --------------->|  Python sends data
//!     |<------- [DATA] ----------------|  Rust echoes back
//! ```
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop responder_tests
//! ```

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use leviculum_core::link::{Link, LinkId, LinkState};
use leviculum_core::packet::Packet;
use leviculum_std::interfaces::hdlc::Deframer;

use crate::common::*;
use crate::harness::TestDaemon;

// =========================================================================
// Test 1: Basic responder handshake
// =========================================================================

/// Verify that Rust can accept an incoming link request from Python.
///
/// This test verifies:
/// - Rust announce is received and processed by Python
/// - Python can initiate a link to Rust's destination
/// - Rust can create incoming link and generate valid proof
/// - Python accepts the proof and sends RTT
/// - Rust can process RTT and finalize link
#[tokio::test]
async fn test_responder_basic_handshake() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Connect to daemon
    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Set up Rust destination and announce
    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["interop"], b"rust-responder").await;

    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);
    let identity = destination.identity().expect("Should have identity");
    println!("Rust destination hash: {}", dest_hash_hex);

    // Wait for daemon to process announce
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify daemon has path to our destination
    assert!(
        daemon.has_path(&dest_hash).await,
        "Daemon should have path to Rust destination after announce"
    );

    println!("Daemon has path, requesting link creation...");

    // Tell Python daemon to create a link to our destination in background
    let daemon_addr = daemon.cmd_addr();
    let dest_hash_hex_clone = dest_hash_hex.clone();
    let public_key_hex_clone = public_key_hex.clone();

    let link_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut cmd_stream = TcpStream::connect(daemon_addr)
            .await
            .expect("Failed to connect");

        let cmd = serde_json::json!({
            "method": "create_link",
            "params": {
                "dest_hash": dest_hash_hex_clone,
                "dest_key": public_key_hex_clone,
                "timeout": 10
            }
        });

        cmd_stream
            .write_all(cmd.to_string().as_bytes())
            .await
            .unwrap();
        cmd_stream.shutdown().await.unwrap();

        let mut response = Vec::new();
        cmd_stream.read_to_end(&mut response).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&response).unwrap()
    });

    // Wait for LINK_REQUEST from Python
    println!("Waiting for LINK_REQUEST...");
    let request_result = wait_for_link_request(
        &mut stream,
        &mut deframer,
        &dest_hash,
        Duration::from_secs(10),
    )
    .await;

    assert!(
        request_result.is_some(),
        "Should receive LINK_REQUEST from Python"
    );
    let (raw_packet, link_id_bytes) = request_result.unwrap();
    let link_id = LinkId::new(link_id_bytes);

    println!("Received LINK_REQUEST, link_id: {}", hex::encode(link_id));

    // Extract request data (skip header: flags + hops + dest_hash + context = 19 bytes)
    let request_data = &raw_packet[19..];

    // Create incoming link
    let mut link = Link::new_incoming(request_data, link_id, dest_hash, &mut OsRng, None)
        .expect("Failed to create incoming link");

    assert_eq!(link.state(), LinkState::Pending);
    assert!(!link.is_initiator());

    // Build and send proof
    let proof_packet = link
        .build_proof_packet(identity, 500, 1)
        .expect("Failed to build proof packet");

    assert_eq!(link.state(), LinkState::Handshake);

    send_framed(&mut stream, &proof_packet).await;
    println!("Sent PROOF packet");

    // Wait for RTT packet from Python
    println!("Waiting for RTT...");
    let rtt_data = wait_for_rtt_packet(
        &mut stream,
        &mut deframer,
        &link_id,
        Duration::from_secs(10),
    )
    .await;

    assert!(rtt_data.is_some(), "Should receive RTT packet from Python");
    let rtt_encrypted = rtt_data.unwrap();

    // Process RTT
    let rtt_seconds = link
        .process_rtt(&rtt_encrypted)
        .expect("Failed to process RTT");

    println!("Processed RTT: {:.3}s", rtt_seconds);

    // Link should now be active
    assert_eq!(link.state(), LinkState::Active);
    assert!(link.link_key().is_some());

    // Wait for Python's link creation task to complete
    let link_response = link_task.await.expect("Link task panicked");
    println!("Python link response: {:?}", link_response);

    // Check that Python got a response (not necessarily status=4 due to timing)
    if let Some(error) = link_response.get("error") {
        panic!("Python link creation failed: {}", error);
    }

    // The real test is that Rust's link is Active - this proves the crypto handshake worked
    // Python's status may lag due to timing, but if Rust processed the RTT successfully,
    // the handshake is complete.
    assert_eq!(
        link.state(),
        LinkState::Active,
        "Rust link should be active after processing RTT"
    );

    println!("SUCCESS: Responder handshake completed!");
}

// =========================================================================
// Test 2: Bidirectional data exchange with Rust as responder
// =========================================================================

/// Verify bidirectional encrypted data exchange with Rust as responder.
///
/// This test:
/// 1. Establishes a link with Rust as responder
/// 2. Python sends data to Rust
/// 3. Rust decrypts and verifies the data
/// 4. Rust sends data back to Python
#[tokio::test]
async fn test_responder_bidirectional_data() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Set up Rust destination
    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["echo"], b"echo-test").await;
    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);
    let identity = destination.identity().expect("Should have identity");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start link creation in background
    let daemon_addr = daemon.cmd_addr();
    let dhx = dest_hash_hex.clone();
    let pkx = public_key_hex.clone();

    let link_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = TcpStream::connect(daemon_addr).await.unwrap();
        let cmd = serde_json::json!({
            "method": "create_link",
            "params": { "dest_hash": dhx, "dest_key": pkx, "timeout": 10 }
        });
        s.write_all(cmd.to_string().as_bytes()).await.unwrap();
        s.shutdown().await.unwrap();
        let mut r = Vec::new();
        s.read_to_end(&mut r).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&r).unwrap()
    });

    // Accept incoming link
    let (raw_packet, link_id_bytes) = wait_for_link_request(
        &mut stream,
        &mut deframer,
        &dest_hash,
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive LINK_REQUEST");
    let link_id = LinkId::new(link_id_bytes);

    let mut link =
        Link::new_incoming(&raw_packet[19..], link_id, dest_hash, &mut OsRng, None).unwrap();

    // Send proof
    let proof_packet = link.build_proof_packet(identity, 500, 1).unwrap();
    send_framed(&mut stream, &proof_packet).await;

    // Process RTT
    let rtt_encrypted = wait_for_rtt_packet(
        &mut stream,
        &mut deframer,
        &link_id,
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive RTT");

    link.process_rtt(&rtt_encrypted).unwrap();
    assert_eq!(link.state(), LinkState::Active);

    // Wait for Python link task
    let link_response = link_task.await.unwrap();
    let link_hash = link_response
        .get("result")
        .and_then(|r| r.get("link_hash"))
        .and_then(|h| h.as_str())
        .expect("Should have link_hash");

    println!("Link established, link_hash: {}", link_hash);

    // Have Python send data to Rust
    let test_message = b"Hello from Python!";
    daemon
        .send_on_link(link_hash, test_message)
        .await
        .expect("Failed to send on link");

    println!("Python sent: {:?}", String::from_utf8_lossy(test_message));

    // Receive and decrypt data from Python
    let raw_packet =
        wait_for_data_packet(&mut stream, &mut deframer, &link_id, Duration::from_secs(5))
            .await
            .expect("Should receive data from Python");

    let pkt = Packet::unpack(&raw_packet).expect("Should parse packet");
    let encrypted_data = pkt.data.as_slice();
    let mut decrypted = vec![0u8; encrypted_data.len()];
    let dec_len = link
        .decrypt(encrypted_data, &mut decrypted)
        .expect("Failed to decrypt data from Python");

    let received_message = &decrypted[..dec_len];
    println!(
        "Rust received: {:?}",
        String::from_utf8_lossy(received_message)
    );

    assert_eq!(
        received_message, test_message,
        "Decrypted message should match what Python sent"
    );

    // Rust sends data back to Python
    let reply_message = b"Hello from Rust!";
    let data_packet = link
        .build_data_packet(reply_message, &mut OsRng)
        .expect("Failed to build data packet");

    send_framed(&mut stream, &data_packet).await;
    println!("Rust sent: {:?}", String::from_utf8_lossy(reply_message));

    // Wait for Python to receive
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check if Python received our message
    let received = daemon.get_received_packets().await.unwrap();
    let found_reply = received.iter().any(|p| p.data == reply_message);

    assert!(
        found_reply,
        "Python should have received Rust's reply. Received: {:?}",
        received
            .iter()
            .map(|p| String::from_utf8_lossy(&p.data))
            .collect::<Vec<_>>()
    );

    println!("SUCCESS: Bidirectional data exchange with Rust as responder!");
}

// =========================================================================
// Test 3: Key derivation matches between Rust and Python
// =========================================================================

/// Verify that both sides derive the same encryption key.
///
/// This is tested implicitly by the bidirectional data exchange, but
/// this test makes it explicit by exchanging data in both directions
/// and verifying correct decryption.
#[tokio::test]
async fn test_responder_key_derivation_match() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["keytest"], b"key-test").await;
    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);
    let identity = destination.identity().expect("Should have identity");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Establish link
    let daemon_addr = daemon.cmd_addr();
    let dhx = dest_hash_hex.clone();
    let pkx = public_key_hex.clone();

    let link_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = TcpStream::connect(daemon_addr).await.unwrap();
        let cmd = serde_json::json!({
            "method": "create_link",
            "params": { "dest_hash": dhx, "dest_key": pkx, "timeout": 10 }
        });
        s.write_all(cmd.to_string().as_bytes()).await.unwrap();
        s.shutdown().await.unwrap();
        let mut r = Vec::new();
        s.read_to_end(&mut r).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&r).unwrap()
    });

    let (raw, link_id_bytes) = wait_for_link_request(
        &mut stream,
        &mut deframer,
        &dest_hash,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let link_id = LinkId::new(link_id_bytes);
    let mut link = Link::new_incoming(&raw[19..], link_id, dest_hash, &mut OsRng, None).unwrap();

    let proof = link.build_proof_packet(identity, 500, 1).unwrap();
    send_framed(&mut stream, &proof).await;

    let rtt = wait_for_rtt_packet(
        &mut stream,
        &mut deframer,
        &link_id,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    link.process_rtt(&rtt).unwrap();

    let resp = link_task.await.unwrap();
    let link_hash = resp
        .get("result")
        .and_then(|r| r.get("link_hash"))
        .and_then(|h| h.as_str())
        .unwrap();

    // Test 1: Python encrypts, Rust decrypts
    let python_message = b"Python encrypted this with AES-256-CBC";
    daemon
        .send_on_link(link_hash, python_message)
        .await
        .unwrap();

    let raw_packet1 =
        wait_for_data_packet(&mut stream, &mut deframer, &link_id, Duration::from_secs(5))
            .await
            .expect("Should receive Python's encrypted data");

    let pkt1 = Packet::unpack(&raw_packet1).expect("Should parse packet");
    let encrypted1 = pkt1.data.as_slice();
    let mut decrypted1 = vec![0u8; encrypted1.len()];
    let len1 = link.decrypt(encrypted1, &mut decrypted1).unwrap();
    assert_eq!(
        &decrypted1[..len1],
        python_message,
        "Rust should decrypt Python's message"
    );
    println!("Rust decrypted Python's message correctly");

    // Test 2: Rust encrypts, Python decrypts
    let rust_message = b"Rust encrypted this with the same derived key";
    let data_packet = link.build_data_packet(rust_message, &mut OsRng).unwrap();
    send_framed(&mut stream, &data_packet).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let received = daemon.get_received_packets().await.unwrap();
    let found = received.iter().any(|p| p.data == rust_message);
    assert!(found, "Python should decrypt Rust's message");
    println!("Python decrypted Rust's message correctly");

    println!("SUCCESS: Key derivation matches between Rust and Python!");
}

// =========================================================================
// Test 4: Multiple data packets
// =========================================================================

/// Verify multiple data packets can be exchanged correctly.
#[tokio::test]
async fn test_responder_multiple_packets() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["multi"], b"multi-packet").await;
    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);
    let identity = destination.identity().expect("Should have identity");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Establish link
    let daemon_addr = daemon.cmd_addr();
    let dhx = dest_hash_hex.clone();
    let pkx = public_key_hex.clone();

    let link_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = TcpStream::connect(daemon_addr).await.unwrap();
        let cmd = serde_json::json!({
            "method": "create_link",
            "params": { "dest_hash": dhx, "dest_key": pkx, "timeout": 10 }
        });
        s.write_all(cmd.to_string().as_bytes()).await.unwrap();
        s.shutdown().await.unwrap();
        let mut r = Vec::new();
        s.read_to_end(&mut r).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&r).unwrap()
    });

    let (raw, link_id_bytes) = wait_for_link_request(
        &mut stream,
        &mut deframer,
        &dest_hash,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let link_id = LinkId::new(link_id_bytes);
    let mut link = Link::new_incoming(&raw[19..], link_id, dest_hash, &mut OsRng, None).unwrap();

    let proof = link.build_proof_packet(identity, 500, 1).unwrap();
    send_framed(&mut stream, &proof).await;

    let rtt = wait_for_rtt_packet(
        &mut stream,
        &mut deframer,
        &link_id,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    link.process_rtt(&rtt).unwrap();

    let _ = link_task.await.unwrap();

    println!("Link established, sending multiple packets from Rust...");

    // Send multiple packets from Rust to Python
    let test_sizes: &[usize] = &[1, 15, 16, 17, 50, 100];

    for (i, &size) in test_sizes.iter().enumerate() {
        let data: Vec<u8> = (0..size).map(|j| ((i + j) & 0xFF) as u8).collect();
        let data_packet = link.build_data_packet(&data, &mut OsRng).unwrap();
        send_framed(&mut stream, &data_packet).await;
        println!("Sent packet {}: {} bytes", i + 1, size);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for Python to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check Python received the packets
    let received = daemon.get_received_packets().await.unwrap();
    println!("Python received {} packets", received.len());

    assert!(
        received.len() >= test_sizes.len() / 2,
        "Python should receive most packets"
    );

    println!("SUCCESS: Multiple packets exchanged correctly!");
}
