//! NodeCore link interop tests using the daemon harness.
//!
//! These tests verify that the [`NodeCore`] link API correctly interoperates
//! with Python Reticulum. Tests cover both initiator and responder roles, Rust-to-Rust
//! communication via Python relay, error handling, and stress scenarios.
//!
//! ## What These Tests Verify
//!
//! 1. **NodeCore as Initiator** - Rust initiates links to Python destinations
//! 2. **NodeCore as Responder** - Python initiates links to Rust destinations
//! 3. **Rust-to-Rust via Relay** - Two Rust NodeCores communicate through daemon
//! 4. **Error Handling** - Timeouts, invalid proofs, invalid states
//! 5. **Stress Tests** - Multiple links, rapid data exchange, large payloads
//!
//! ## Running These Tests
//!
//! ```sh
//! # Run all link tests
//! cargo test --package leviculum-std --test rnsd_interop link_manager_tests
//!
//! # Run specific test
//! cargo test --package leviculum-std --test rnsd_interop test_manager_initiator_basic_handshake
//!
//! # Run with verbose output
//! cargo test --package leviculum-std --test rnsd_interop link_manager_tests -- --nocapture
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rand_core::OsRng;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::identity::Identity;
use leviculum_core::link::{LinkCloseReason, LinkId, LinkState};
use leviculum_core::node::{NodeCoreBuilder, NodeEvent};
use leviculum_core::packet::{Packet, PacketType};
use leviculum_core::traits::Clock;
use leviculum_core::transport::{Action, InterfaceId};
use leviculum_core::DestinationHash;
use leviculum_core::MemoryStorage;
use leviculum_core::{Destination, DestinationType, Direction};
use leviculum_std::interfaces::hdlc::{DeframeResult, Deframer};

use crate::common::{
    connect_to_daemon, receive_raw_proof_for_link, send_framed, setup_rust_destination,
    wait_for_any_announce_with_route_info, wait_for_data_packet, wait_for_link_data_packet,
    wait_for_link_request, wait_for_rtt_packet, TestClock,
};
use crate::harness::{DestinationInfo, HarnessError, TestDaemon};

// =========================================================================
// Test context helpers
// =========================================================================

/// Thread-safe mock clock that can be advanced for timeout testing.
///
/// Uses `Arc<AtomicU64>` so a handle can be kept outside `NodeCore` (which
/// takes ownership of the clock). Call `handle()` before passing the clock
/// to `NodeCoreBuilder::build()` to retain access.
struct SharedMockClock(Arc<AtomicU64>);

impl SharedMockClock {
    fn new(initial_ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(initial_ms)))
    }

    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::Relaxed);
    }

    fn handle(&self) -> SharedMockClock {
        SharedMockClock(Arc::clone(&self.0))
    }
}

impl Clock for SharedMockClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

// =========================================================================
// Helper functions
// =========================================================================

/// Extract all packet data from TickOutput actions.
fn extract_action_packets(output: &leviculum_core::transport::TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data.clone(),
        })
        .collect()
}

/// Send all actions from a TickOutput over the wire.
async fn dispatch_actions(stream: &mut TcpStream, output: &leviculum_core::transport::TickOutput) {
    for action in &output.actions {
        match action {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => {
                send_framed(stream, data).await;
            }
        }
    }
}

/// Check whether any event in a TickOutput is `LinkEstablished`.
fn has_link_established(events: &[NodeEvent], link_id: &LinkId, expected_initiator: bool) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            NodeEvent::LinkEstablished { link_id: id, is_initiator, .. }
                if *id == *link_id && *is_initiator == expected_initiator
        )
    })
}

/// Check whether a node considers a link active.
fn is_active<R: rand_core::CryptoRngCore, C: Clock, S: leviculum_core::traits::Storage>(
    node: &leviculum_core::node::NodeCore<R, C, S>,
    link_id: &LinkId,
) -> bool {
    node.link(link_id)
        .map(|l| l.state() == LinkState::Active)
        .unwrap_or(false)
}

/// Establish link with Rust as initiator using NodeCore.
///
/// Returns (link_id, destination_info) on success.
async fn establish_initiator_link(
    node: &mut leviculum_core::node::NodeCore<OsRng, TestClock, MemoryStorage>,
    daemon: &TestDaemon,
    stream: &mut TcpStream,
    deframer: &mut Deframer,
) -> Result<(LinkId, DestinationInfo), HarnessError> {
    // Register a destination in daemon
    let dest_info = daemon.register_destination("linktest", &["echo"]).await?;

    // Extract signing key
    let pub_key_bytes =
        hex::decode(&dest_info.public_key).map_err(|e| HarnessError::ParseError(e.to_string()))?;
    let signing_key_bytes: [u8; 32] = pub_key_bytes[32..64]
        .try_into()
        .map_err(|_| HarnessError::ParseError("Invalid signing key".to_string()))?;

    // Parse destination hash
    let dest_hash_bytes =
        hex::decode(&dest_info.hash).map_err(|e| HarnessError::ParseError(e.to_string()))?;
    let dest_hash: [u8; TRUNCATED_HASHBYTES] = dest_hash_bytes
        .try_into()
        .map_err(|_| HarnessError::ParseError("Invalid hash length".to_string()))?;

    // Initiate link via node
    let (link_id, _, output) = node.connect(DestinationHash::new(dest_hash), &signing_key_bytes);

    // Send link request
    dispatch_actions(stream, &output).await;

    // Wait for proof (raw bytes for handle_packet)
    let proof_raw = receive_raw_proof_for_link(stream, deframer, &link_id, Duration::from_secs(10))
        .await
        .ok_or_else(|| HarnessError::CommandFailed("No proof received".to_string()))?;

    // Process proof via node
    let output = node.handle_packet(InterfaceId(0), &proof_raw);

    // Check for LinkEstablished event
    if !has_link_established(&output.events, &link_id, true) {
        return Err(HarnessError::CommandFailed(
            "LinkEstablished event not received".to_string(),
        ));
    }

    // Dispatch RTT packet (included in actions from proof handling)
    dispatch_actions(stream, &output).await;

    // Wait for daemon to process RTT
    tokio::time::sleep(Duration::from_millis(200)).await;

    Ok((link_id, dest_info))
}

/// Establish link with Rust as responder using NodeCore.
///
/// Returns link_id on success.
async fn establish_responder_link(
    node: &mut leviculum_core::node::NodeCore<OsRng, TestClock, MemoryStorage>,
    _daemon: &TestDaemon,
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    dest_hash: DestinationHash,
) -> Result<LinkId, HarnessError> {
    // Wait for link request
    let (raw_packet, link_id_bytes) =
        wait_for_link_request(stream, deframer, &dest_hash, Duration::from_secs(10))
            .await
            .ok_or_else(|| HarnessError::CommandFailed("No link request received".to_string()))?;
    let link_id = LinkId::new(link_id_bytes);

    // Process the packet via node. Auto-accept (Python parity): handle_packet
    // builds and routes the establishment proof inline, so the proof action is in
    // this output.
    let output = node.handle_packet(InterfaceId(0), &raw_packet);

    // The responder must have auto-proved the incoming link.
    if output.actions.is_empty() {
        return Err(HarnessError::CommandFailed(
            "responder did not auto-prove the incoming link".to_string(),
        ));
    }

    // Send proof (auto-sent inline by handle_packet)
    dispatch_actions(stream, &output).await;

    // Wait for RTT
    let rtt_data = wait_for_rtt_packet(stream, deframer, &link_id, Duration::from_secs(10))
        .await
        .ok_or_else(|| HarnessError::CommandFailed("No RTT received".to_string()))?;

    // Build raw RTT packet for handle_packet
    let mut rtt_raw = Vec::new();
    rtt_raw.push(0x0C); // flags for Data packet to Link
    rtt_raw.push(0x00); // hops
    rtt_raw.extend_from_slice(link_id.as_bytes());
    rtt_raw.push(leviculum_core::packet::PacketContext::Lrrtt.to_byte());
    rtt_raw.extend_from_slice(&rtt_data);

    // Process RTT via node
    let output = node.handle_packet(InterfaceId(0), &rtt_raw);

    // Check for LinkEstablished event
    if !has_link_established(&output.events, &link_id, false) {
        return Err(HarnessError::CommandFailed(
            "LinkEstablished event not received".to_string(),
        ));
    }

    Ok(link_id)
}

// =========================================================================
// 1. NodeCore as Initiator (Rust -> Python)
// =========================================================================

/// Test basic handshake with Rust as initiator using NodeCore.
///
/// Verifies:
/// - node.connect() creates pending link
/// - Processing proof triggers LinkEstablished { is_initiator: true }
/// - RTT packet is dispatched automatically via actions
#[tokio::test]
async fn test_manager_initiator_basic_handshake() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // Establish link
    let (link_id, _dest_info) =
        establish_initiator_link(&mut node, &daemon, &mut stream, &mut deframer)
            .await
            .expect("Failed to establish link");

    // Verify link is active
    assert!(
        is_active(&node, &link_id),
        "Link should be active after handshake"
    );
    assert_eq!(node.active_link_count(), 1);
    assert_eq!(node.pending_link_count(), 0);

    println!("SUCCESS: NodeCore initiator basic handshake");
}

/// Test data exchange with NodeCore as initiator.
///
/// Verifies:
/// - node.send_on_link() encrypts and returns packet in actions
/// - Daemon receives and decrypts data
/// - Daemon can send data back
/// - LinkDataReceived/MessageReceived event is emitted
#[tokio::test]
async fn test_manager_initiator_data_exchange() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    let (link_id, _dest_info) =
        establish_initiator_link(&mut node, &daemon, &mut stream, &mut deframer)
            .await
            .expect("Failed to establish link");

    // Send data via node
    let test_data = b"Hello from NodeCore!";
    let output = node
        .send_on_link(&link_id, test_data)
        .expect("Failed to send data");

    dispatch_actions(&mut stream, &output).await;
    println!("Sent: {:?}", String::from_utf8_lossy(test_data));

    // Wait for daemon to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify daemon received the data
    let received = daemon
        .get_received_packets()
        .await
        .expect("Failed to get received packets");

    let found = received.iter().any(|p| p.data == test_data);
    assert!(
        found,
        "Daemon should have received our message. Got: {:?}",
        received
            .iter()
            .map(|p| String::from_utf8_lossy(&p.data))
            .collect::<Vec<_>>()
    );

    println!("SUCCESS: NodeCore initiator data exchange");
}

/// Test sequential links: establish -> exchange -> close -> establish new.
#[tokio::test]
async fn test_manager_initiator_sequential_links() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // First link
    let mut stream1 = connect_to_daemon(&daemon).await;
    let mut deframer1 = Deframer::new();

    let (link_id1, _) = establish_initiator_link(&mut node, &daemon, &mut stream1, &mut deframer1)
        .await
        .expect("Failed to establish first link");

    // Send data on first link
    let data1 = b"First link data";
    let output = node.send_on_link(&link_id1, data1).unwrap();
    dispatch_actions(&mut stream1, &output).await;

    // Close first link
    let output = node.close_link(&link_id1);
    assert!(!is_active(&node, &link_id1));

    // Drain close event
    assert!(output.events.iter().any(
        |e| matches!(e, NodeEvent::LinkClosed { link_id, reason, .. }
        if *link_id == link_id1 && *reason == LinkCloseReason::Normal)
    ));

    // Second link (new connection)
    let mut stream2 = connect_to_daemon(&daemon).await;
    let mut deframer2 = Deframer::new();

    let (link_id2, _) = establish_initiator_link(&mut node, &daemon, &mut stream2, &mut deframer2)
        .await
        .expect("Failed to establish second link");

    assert_ne!(link_id1, link_id2);
    assert!(is_active(&node, &link_id2));

    // Send data on second link
    let data2 = b"Second link data";
    let output = node.send_on_link(&link_id2, data2).unwrap();
    dispatch_actions(&mut stream2, &output).await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify daemon received second message
    let received = daemon.get_received_packets().await.unwrap();
    let found = received.iter().any(|p| p.data == data2);
    assert!(found, "Daemon should have received second link's data");

    println!("SUCCESS: NodeCore sequential links");
}

/// Test concurrent links to different destinations.
#[tokio::test]
async fn test_manager_initiator_concurrent_links() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register multiple destinations
    let dest1 = daemon
        .register_destination("concurrent", &["alpha"])
        .await
        .expect("Failed to register dest1");
    let dest2 = daemon
        .register_destination("concurrent", &["beta"])
        .await
        .expect("Failed to register dest2");
    let dest3 = daemon
        .register_destination("concurrent", &["gamma"])
        .await
        .expect("Failed to register dest3");

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // Helper to initiate link
    let initiate_link =
        |node: &mut leviculum_core::node::NodeCore<OsRng, TestClock, MemoryStorage>,
         dest_info: &DestinationInfo|
         -> (LinkId, Vec<Vec<u8>>) {
            let pub_key_bytes = hex::decode(&dest_info.public_key).unwrap();
            let signing_key: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();
            let dest_hash: [u8; 16] = hex::decode(&dest_info.hash).unwrap().try_into().unwrap();
            let (link_id, _, output) = node.connect(DestinationHash::new(dest_hash), &signing_key);
            let packets = extract_action_packets(&output);
            (link_id, packets)
        };

    // Initiate all three links
    let (link_id1, packets1) = initiate_link(&mut node, &dest1);
    let (link_id2, packets2) = initiate_link(&mut node, &dest2);
    let (link_id3, packets3) = initiate_link(&mut node, &dest3);

    // Connect and send all requests
    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    for pkt in &packets1 {
        send_framed(&mut stream, pkt).await;
    }
    for pkt in &packets2 {
        send_framed(&mut stream, pkt).await;
    }
    for pkt in &packets3 {
        send_framed(&mut stream, pkt).await;
    }

    // Receive and process proofs
    let mut established = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut buf = [0u8; 2048];

    while established < 3 && tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Proof {
                                let output = node.handle_packet(InterfaceId(0), &data);
                                // Dispatch RTT actions
                                dispatch_actions(&mut stream, &output).await;
                                // Count established events
                                for event in &output.events {
                                    if matches!(event, NodeEvent::LinkEstablished { .. }) {
                                        established += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    println!("Established {} links", established);
    assert_eq!(
        established, 3,
        "All 3 links must be established. Got {}",
        established
    );

    // Verify all links are active
    assert!(is_active(&node, &link_id1));
    assert!(is_active(&node, &link_id2));
    assert!(is_active(&node, &link_id3));

    println!("All 3 concurrent links established");
}

// =========================================================================
// 2. NodeCore as Responder (Python -> Rust)
// =========================================================================

/// Test accepting an incoming link request.
///
/// Verifies:
/// - LinkRequest event is emitted when LINK_REQUEST arrives
/// - With auto-accept (Stage 1), handle_packet() builds and routes the
///   establishment proof inline (the proof action is in its output)
/// - After RTT, LinkEstablished { is_initiator: false } is emitted
#[tokio::test]
async fn test_manager_responder_auto_accept() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Set up Rust destination
    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["responder"], b"responder-test").await;

    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);

    // Register destination in node
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut dest = destination;
    dest.set_accepts_links(true);
    node.register_destination(dest);

    // Wait for daemon to process announce
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify daemon has path
    assert!(
        daemon.has_path(&dest_hash).await,
        "Daemon should have path to Rust destination"
    );

    // Have Python create link to our destination
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

    // --- Establish link as responder, inlined to assert on the proof ---

    // Wait for link request
    let (raw_request, link_id_bytes) = wait_for_link_request(
        &mut stream,
        &mut deframer,
        &dest_hash,
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive link request");
    let link_id = LinkId::new(link_id_bytes);

    // Auto-accept (Python parity): handle_packet builds + routes the proof inline,
    // so its output carries the establishment proof action.
    let output = node.handle_packet(InterfaceId(0), &raw_request);

    // The establishment proof must be present in handle_packet's output.
    let proof_present = extract_action_packets(&output).iter().any(|data| {
        Packet::unpack(data)
            .map(|pkt| pkt.flags.packet_type == PacketType::Proof)
            .unwrap_or(false)
    });
    assert!(
        proof_present,
        "handle_packet should auto-produce the establishment proof in its actions"
    );

    // Send the auto-produced proof on the wire.
    dispatch_actions(&mut stream, &output).await;

    // Wait for RTT and process it to complete establishment.
    let rtt_data = wait_for_rtt_packet(
        &mut stream,
        &mut deframer,
        &link_id,
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive RTT");

    let mut rtt_raw = Vec::new();
    rtt_raw.push(0x0C);
    rtt_raw.push(0x00);
    rtt_raw.extend_from_slice(link_id.as_bytes());
    rtt_raw.push(leviculum_core::packet::PacketContext::Lrrtt.to_byte());
    rtt_raw.extend_from_slice(&rtt_data);

    let output = node.handle_packet(InterfaceId(0), &rtt_raw);
    assert!(
        has_link_established(&output.events, &link_id, false),
        "Should emit LinkEstablished {{ is_initiator: false }}. Events: {:?}",
        output.events
    );

    // Wait for Python task
    let link_response = link_task.await.expect("Link task panicked");
    println!("Python link response: {:?}", link_response);

    // Verify link is active
    assert!(
        is_active(&node, &link_id),
        "Link should be active after handshake"
    );

    // Verify link properties
    let link = node.link(&link_id).expect("Link should exist");
    assert!(!link.is_initiator(), "We should be responder");

    println!("SUCCESS: NodeCore responder accept link");
}

/// Test rejecting an incoming link request.
#[tokio::test]
async fn test_manager_responder_reject_link() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Set up Rust destination
    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["reject"], b"reject-test").await;

    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);

    // Register destination in node
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut dest = destination;
    dest.set_accepts_links(true);
    node.register_destination(dest);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Have Python create link
    let daemon_addr = daemon.cmd_addr();
    let dhx = dest_hash_hex.clone();
    let pkx = public_key_hex.clone();

    let _link_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = TcpStream::connect(daemon_addr).await.unwrap();
        let cmd = serde_json::json!({
            "method": "create_link",
            "params": { "dest_hash": dhx, "dest_key": pkx, "timeout": 5 }
        });
        s.write_all(cmd.to_string().as_bytes()).await.unwrap();
        s.shutdown().await.unwrap();
        let mut r = Vec::new();
        s.read_to_end(&mut r).await.unwrap();
        // Expect failure since we reject
        serde_json::from_slice::<serde_json::Value>(&r).unwrap()
    });

    // Wait for link request
    let result = wait_for_link_request(
        &mut stream,
        &mut deframer,
        &dest_hash,
        Duration::from_secs(10),
    )
    .await;

    assert!(result.is_some(), "Should receive link request");
    let (raw_packet, link_id_bytes) = result.unwrap();
    let link_id = LinkId::new(link_id_bytes);

    // Process the packet. Auto-accept (Python parity): handle_packet auto-proves
    // the incoming link inline.
    let output = node.handle_packet(InterfaceId(0), &raw_packet);
    assert!(
        !output.actions.is_empty(),
        "responder should auto-prove the incoming link"
    );

    // Reject the link
    node.reject_link(&link_id);

    // Verify link is removed
    assert!(node.link(&link_id).is_none());
    assert_eq!(node.pending_link_count(), 0);

    println!("SUCCESS: NodeCore responder reject link");
}

/// Test bidirectional data exchange with Rust as responder.
#[tokio::test]
async fn test_manager_responder_data_exchange() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Set up Rust destination
    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["dataexchange"], b"data-test").await;

    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut dest = destination;
    dest.set_accepts_links(true);
    node.register_destination(dest);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Have Python create link
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

    // Establish link
    let link_id =
        establish_responder_link(&mut node, &daemon, &mut stream, &mut deframer, dest_hash)
            .await
            .expect("Failed to establish link");

    // Get link hash from Python
    let link_response = link_task.await.unwrap();
    let python_link_hash = link_response
        .get("result")
        .and_then(|r| r.get("link_hash"))
        .and_then(|h| h.as_str())
        .expect("Should have link_hash");

    // Python sends data to Rust
    let python_message = b"Hello from Python!";
    daemon
        .send_on_link(python_link_hash, python_message)
        .await
        .expect("Failed to send from Python");

    // Wait for and process data packet
    let data_raw =
        wait_for_data_packet(&mut stream, &mut deframer, &link_id, Duration::from_secs(5))
            .await
            .expect("Should receive data from Python");

    let output = node.handle_packet(InterfaceId(0), &data_raw);

    // Check for LinkDataReceived or MessageReceived event
    let received_data = output.events.iter().find_map(|e| match e {
        NodeEvent::LinkDataReceived { link_id: id, data } if *id == link_id => Some(data.clone()),
        NodeEvent::MessageReceived {
            link_id: id, data, ..
        } if *id == link_id => Some(data.clone()),
        _ => None,
    });

    assert_eq!(
        received_data.as_deref(),
        Some(python_message.as_slice()),
        "Should receive correct data from Python"
    );

    // Rust sends data to Python
    let rust_message = b"Hello from Rust!";
    let output = node
        .send_on_link(&link_id, rust_message)
        .expect("Failed to send data");
    dispatch_actions(&mut stream, &output).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify Python received
    let received = daemon.get_received_packets().await.unwrap();
    let found = received.iter().any(|p| p.data == rust_message);
    assert!(found, "Python should receive Rust's message");

    println!("SUCCESS: NodeCore responder data exchange");
}

/// Test accepting multiple sequential incoming links.
#[tokio::test]
async fn test_manager_responder_multiple_incoming() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Set up Rust destination
    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["multi"], b"multi-test").await;

    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut dest = destination;
    dest.set_accepts_links(true);
    node.register_destination(dest);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Accept 3 sequential links
    for i in 0..3 {
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

        let link_id =
            establish_responder_link(&mut node, &daemon, &mut stream, &mut deframer, dest_hash)
                .await
                .unwrap_or_else(|_| panic!("Failed to establish link {}", i + 1));

        let _ = link_task.await;

        // Close the link before next iteration
        let _output = node.close_link(&link_id);

        println!("Established and closed link {}", i + 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    println!("SUCCESS: NodeCore responder multiple incoming");
}

// =========================================================================
// 3. Rust-to-Rust via Python Relay
// =========================================================================

/// Test two Rust NodeCores communicating through daemon relay.
///
/// Setup:
/// - Node A: Creates identity, destination, announces via daemon
/// - Node B: Receives A's announce over the wire, extracts signing key and transport_id
/// - Node B: Initiates link with HEADER_2 transport headers
/// - Daemon forwards link request to A based on transport_id
/// - Both nodes exchange data bidirectionally
///
/// This tests proper mesh routing: B discovers A through the announce packet
/// forwarded by the daemon, and uses the transport_id from that announce to
/// route the link request back through the daemon.
#[tokio::test]
async fn test_rust_to_rust_via_daemon() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // --- Node B connects first to receive announces ---
    let mut stream_b = connect_to_daemon(&daemon).await;
    let mut deframer_b = Deframer::new();
    let mut node_b = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // --- Node A (Responder) ---
    let mut stream_a = connect_to_daemon(&daemon).await;
    let mut deframer_a = Deframer::new();

    let (destination_a, _public_key_hex_a) =
        setup_rust_destination(&mut stream_a, "r2r", &["test"], b"rust-A").await;

    let dest_hash_a = *destination_a.hash();

    let mut node_a = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut dest_a = destination_a;
    dest_a.set_accepts_links(true);
    node_a.register_destination(dest_a);

    // Wait for daemon to process announce
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify daemon has path to A
    assert!(
        daemon.has_path(&dest_hash_a).await,
        "Daemon should have path to A after announce"
    );

    // --- B receives A's announce over the wire with full routing info ---
    let announce_info = wait_for_any_announce_with_route_info(
        &mut stream_b,
        &mut deframer_b,
        Duration::from_secs(5),
    )
    .await
    .expect("B should receive A's announce from daemon");

    // Verify the announce is for A's destination
    assert_eq!(
        announce_info.packet.destination_hash, dest_hash_a,
        "Announce should be for A's destination"
    );

    // Extract routing info from the announce
    let signing_key_a = announce_info
        .signing_key()
        .expect("Failed to extract signing key from announce");

    // Feed announce to B's transport so it learns the path
    let _ = node_b.handle_packet(InterfaceId(0), &announce_info.raw_data);

    // B initiates link to A - connect() uses the path learned from announce
    let (link_id_b, _, output) = node_b.connect(dest_hash_a, &signing_key_a);

    // If transport_id is set, we should be using HEADER_2
    let packets = extract_action_packets(&output);
    if announce_info.transport_id.is_some() && !packets.is_empty() {
        let using_header_2 = packets[0][0] & 0x40 != 0;
        assert!(
            using_header_2,
            "Should use HEADER_2 when transport_id is set"
        );
    }

    // Send link request via B's stream
    dispatch_actions(&mut stream_b, &output).await;

    // A receives link request
    let (raw_request, link_id_a_bytes) = wait_for_link_request(
        &mut stream_a,
        &mut deframer_a,
        &dest_hash_a,
        Duration::from_secs(10),
    )
    .await
    .expect("A should receive link request");
    let link_id_a = LinkId::new(link_id_a_bytes);

    // Verify link IDs match
    assert_eq!(
        link_id_a, link_id_b,
        "Link IDs should match after transport unwrapping"
    );

    // Auto-accept (Stage 1): handle_packet builds + routes the proof inline.
    let output = node_a.handle_packet(InterfaceId(0), &raw_request);

    // Send proof via A's stream (auto-sent inline by handle_packet)
    dispatch_actions(&mut stream_a, &output).await;

    // B receives proof
    let proof_raw = receive_raw_proof_for_link(
        &mut stream_b,
        &mut deframer_b,
        &link_id_b,
        Duration::from_secs(10),
    )
    .await
    .expect("B should receive proof");

    let output = node_b.handle_packet(InterfaceId(0), &proof_raw);

    // B should have LinkEstablished
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkEstablished {
            is_initiator: true,
            ..
        }
    )));

    // Dispatch RTT from B
    dispatch_actions(&mut stream_b, &output).await;

    // A receives RTT
    let rtt_data = wait_for_rtt_packet(
        &mut stream_a,
        &mut deframer_a,
        &link_id_a,
        Duration::from_secs(10),
    )
    .await
    .expect("A should receive RTT");

    // Build RTT packet for processing
    let mut rtt_raw = Vec::new();
    rtt_raw.push(0x0C);
    rtt_raw.push(0x00);
    rtt_raw.extend_from_slice(link_id_a.as_bytes());
    rtt_raw.push(leviculum_core::packet::PacketContext::Lrrtt.to_byte());
    rtt_raw.extend_from_slice(&rtt_data);

    let output = node_a.handle_packet(InterfaceId(0), &rtt_raw);

    // A should have LinkEstablished
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkEstablished {
            is_initiator: false,
            ..
        }
    )));

    // Both links should be active
    assert!(is_active(&node_a, &link_id_a));
    assert!(is_active(&node_b, &link_id_b));

    // Let the daemon finalize its link table after handshake
    tokio::time::sleep(Duration::from_millis(500)).await;

    // --- Bidirectional data exchange ---

    // B sends to A (send_on_link uses Channel, so context=Channel)
    let msg_b_to_a = b"Hello from B to A!";
    let output = node_b.send_on_link(&link_id_b, msg_b_to_a).unwrap();
    dispatch_actions(&mut stream_b, &output).await;

    // A receives any Data packet for this link (Channel context)
    let data_raw_a = wait_for_link_data_packet(
        &mut stream_a,
        &mut deframer_a,
        &link_id_a,
        Duration::from_secs(10),
    )
    .await
    .expect("A should receive data from B");

    let output = node_a.handle_packet(InterfaceId(0), &data_raw_a);
    // Dispatch any response actions (proofs, acks)
    dispatch_actions(&mut stream_a, &output).await;

    let received_a = output.events.iter().find_map(|e| match e {
        NodeEvent::LinkDataReceived { data, .. } => Some(data.clone()),
        NodeEvent::MessageReceived { data, .. } => Some(data.clone()),
        _ => None,
    });

    assert_eq!(received_a.as_deref(), Some(msg_b_to_a.as_slice()));

    // Small pause before reverse direction
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A sends to B
    let msg_a_to_b = b"Hello from A to B!";
    let output = node_a.send_on_link(&link_id_a, msg_a_to_b).unwrap();
    dispatch_actions(&mut stream_a, &output).await;

    // B receives (Channel context)
    let data_raw_b = wait_for_link_data_packet(
        &mut stream_b,
        &mut deframer_b,
        &link_id_b,
        Duration::from_secs(10),
    )
    .await
    .expect("B should receive data from A");

    let output = node_b.handle_packet(InterfaceId(0), &data_raw_b);
    dispatch_actions(&mut stream_b, &output).await;

    let received_b = output.events.iter().find_map(|e| match e {
        NodeEvent::LinkDataReceived { data, .. } => Some(data.clone()),
        NodeEvent::MessageReceived { data, .. } => Some(data.clone()),
        _ => None,
    });

    assert_eq!(received_b.as_deref(), Some(msg_a_to_b.as_slice()));

    println!("SUCCESS: Rust-to-Rust via daemon relay");
}

/// Test multiple message exchange between two Rust NodeCores.
///
/// Similar to test_rust_to_rust_via_daemon but exchanges many messages to verify
/// reliable bidirectional communication.
#[tokio::test]
async fn test_rust_to_rust_multiple_messages() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // --- B connects first to receive announces ---
    let mut stream_b = connect_to_daemon(&daemon).await;
    let mut deframer_b = Deframer::new();
    let mut node_b = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // --- A (Responder) sets up and announces ---
    let mut stream_a = connect_to_daemon(&daemon).await;
    let mut deframer_a = Deframer::new();

    let (destination_a, _public_key_hex_a) =
        setup_rust_destination(&mut stream_a, "r2rmulti", &["test"], b"rust-A").await;

    let dest_hash_a = *destination_a.hash();

    let mut node_a = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut dest_a = destination_a;
    dest_a.set_accepts_links(true);
    node_a.register_destination(dest_a);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // --- B receives A's announce with routing info ---
    let announce_info = wait_for_any_announce_with_route_info(
        &mut stream_b,
        &mut deframer_b,
        Duration::from_secs(5),
    )
    .await
    .expect("B should receive A's announce from daemon");

    assert_eq!(announce_info.packet.destination_hash, dest_hash_a);

    let signing_key_a = announce_info
        .signing_key()
        .expect("Failed to extract signing key from announce");

    // Feed announce to B's transport so it learns the path
    let _ = node_b.handle_packet(InterfaceId(0), &announce_info.raw_data);

    // Establish link
    let (link_id_b, _, output) = node_b.connect(dest_hash_a, &signing_key_a);
    dispatch_actions(&mut stream_b, &output).await;

    let (raw_request, link_id_a_bytes) = wait_for_link_request(
        &mut stream_a,
        &mut deframer_a,
        &dest_hash_a,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let link_id_a = LinkId::new(link_id_a_bytes);

    // Auto-accept (Stage 1): handle_packet builds + routes the proof inline.
    let output = node_a.handle_packet(InterfaceId(0), &raw_request);
    dispatch_actions(&mut stream_a, &output).await;

    let proof_raw = receive_raw_proof_for_link(
        &mut stream_b,
        &mut deframer_b,
        &link_id_b,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let output = node_b.handle_packet(InterfaceId(0), &proof_raw);
    // Dispatch RTT
    dispatch_actions(&mut stream_b, &output).await;

    let rtt_data = wait_for_rtt_packet(
        &mut stream_a,
        &mut deframer_a,
        &link_id_a,
        Duration::from_secs(10),
    )
    .await
    .unwrap();

    let mut rtt_raw = Vec::new();
    rtt_raw.push(0x0C);
    rtt_raw.push(0x00);
    rtt_raw.extend_from_slice(link_id_a.as_bytes());
    rtt_raw.push(leviculum_core::packet::PacketContext::Lrrtt.to_byte());
    rtt_raw.extend_from_slice(&rtt_data);

    let _output = node_a.handle_packet(InterfaceId(0), &rtt_raw);

    // Let the daemon finalize its link table after handshake
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Exchange messages in both directions.
    // send_on_link() uses Channel (windowed, reliable) so we must handle
    // Busy/PacingDelay by processing incoming packets between sends.
    let total = 5;
    let mut sent_by_b = 0;
    let mut sent_by_a = 0;
    let mut received_by_a = 0;
    let mut received_by_b = 0;
    let mut buf_a = [0u8; 2048];
    let mut buf_b = [0u8; 2048];

    /// Process all pending frames on a stream, feeding them to the node.
    /// Collects response actions into `pending_actions` for later dispatch.
    /// Returns the count of data/message events received.
    fn process_incoming(
        deframer: &mut Deframer,
        raw: &[u8],
        node: &mut leviculum_core::node::NodeCore<OsRng, TestClock, MemoryStorage>,
        pending_actions: &mut Vec<Vec<u8>>,
    ) -> usize {
        let mut count = 0;
        for result in deframer.process(raw) {
            if let DeframeResult::Frame(data) = result {
                let output = node.handle_packet(InterfaceId(0), &data);
                for action in &output.actions {
                    match action {
                        Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => {
                            pending_actions.push(data.clone());
                        }
                    }
                }
                for event in &output.events {
                    if matches!(
                        event,
                        NodeEvent::LinkDataReceived { .. } | NodeEvent::MessageReceived { .. }
                    ) {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    for i in 0..total {
        // B -> A (with retry on Busy)
        let msg = format!("Message {} from B", i);
        for _attempt in 0..5 {
            match node_b.send_on_link(&link_id_b, msg.as_bytes()) {
                Ok(output) => {
                    dispatch_actions(&mut stream_b, &output).await;
                    sent_by_b += 1;
                    break;
                }
                Err(_) => {
                    // Process incoming to free window
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let mut pending = Vec::new();
                    if let Ok(Ok(n)) =
                        timeout(Duration::from_millis(100), stream_b.read(&mut buf_b)).await
                    {
                        if n > 0 {
                            received_by_b += process_incoming(
                                &mut deframer_b,
                                &buf_b[..n],
                                &mut node_b,
                                &mut pending,
                            );
                        }
                    }
                    for pkt in &pending {
                        send_framed(&mut stream_b, pkt).await;
                    }
                    let output = node_b.handle_timeout();
                    dispatch_actions(&mut stream_b, &output).await;
                }
            }
        }

        // A -> B (with retry on Busy)
        let msg = format!("Message {} from A", i);
        for _attempt in 0..5 {
            match node_a.send_on_link(&link_id_a, msg.as_bytes()) {
                Ok(output) => {
                    dispatch_actions(&mut stream_a, &output).await;
                    sent_by_a += 1;
                    break;
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let mut pending = Vec::new();
                    if let Ok(Ok(n)) =
                        timeout(Duration::from_millis(100), stream_a.read(&mut buf_a)).await
                    {
                        if n > 0 {
                            received_by_a += process_incoming(
                                &mut deframer_a,
                                &buf_a[..n],
                                &mut node_a,
                                &mut pending,
                            );
                        }
                    }
                    for pkt in &pending {
                        send_framed(&mut stream_a, pkt).await;
                    }
                    let output = node_a.handle_timeout();
                    dispatch_actions(&mut stream_a, &output).await;
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Process any incoming between rounds
        {
            let mut pending = Vec::new();
            if let Ok(Ok(n)) = timeout(Duration::from_millis(50), stream_a.read(&mut buf_a)).await {
                if n > 0 {
                    received_by_a +=
                        process_incoming(&mut deframer_a, &buf_a[..n], &mut node_a, &mut pending);
                }
            }
            for pkt in &pending {
                send_framed(&mut stream_a, pkt).await;
            }
        }
        {
            let mut pending = Vec::new();
            if let Ok(Ok(n)) = timeout(Duration::from_millis(50), stream_b.read(&mut buf_b)).await {
                if n > 0 {
                    received_by_b +=
                        process_incoming(&mut deframer_b, &buf_b[..n], &mut node_b, &mut pending);
                }
            }
            for pkt in &pending {
                send_framed(&mut stream_b, pkt).await;
            }
        }
    }

    // Drain remaining messages
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && (received_by_a < total || received_by_b < total)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let mut pending = Vec::new();
            if let Ok(Ok(n)) = timeout(Duration::from_millis(50), stream_a.read(&mut buf_a)).await {
                if n > 0 {
                    received_by_a +=
                        process_incoming(&mut deframer_a, &buf_a[..n], &mut node_a, &mut pending);
                }
            }
            for pkt in &pending {
                send_framed(&mut stream_a, pkt).await;
            }
        }
        {
            let mut pending = Vec::new();
            if let Ok(Ok(n)) = timeout(Duration::from_millis(50), stream_b.read(&mut buf_b)).await {
                if n > 0 {
                    received_by_b +=
                        process_incoming(&mut deframer_b, &buf_b[..n], &mut node_b, &mut pending);
                }
            }
            for pkt in &pending {
                send_framed(&mut stream_b, pkt).await;
            }
        }
    }

    println!(
        "A received {} messages (sent by B: {}), B received {} messages (sent by A: {})",
        received_by_a, sent_by_b, received_by_b, sent_by_a
    );
    assert_eq!(
        received_by_a, total,
        "A must receive all {} messages. Got {}",
        total, received_by_a
    );
    assert_eq!(
        received_by_b, total,
        "B must receive all {} messages. Got {}",
        total, received_by_b
    );

    println!("All {} messages exchanged", total * 2);
}

// =========================================================================
// 4. Error Handling
// =========================================================================

/// Test handshake timeout for initiator.
///
/// Link-request retry gives max(LINK_REQUEST_MAX_RETRIES, hops) = max(2, 1) = 2
/// retries, so 3 total attempts. Each attempt resets the timeout to
/// establishment_timeout_ms(). For hops=1, no bitrate the base is
/// 6000 * (1+1) = 12_000ms, plus the per-attempt #129 jitter (0..=25%,
/// re-rolled per retry) — the test reads the live value per attempt.
/// We must exhaust all retries before the link is removed.
#[tokio::test]
async fn test_manager_handshake_timeout() {
    let clock = SharedMockClock::new(1_000_000);
    let clock_handle = clock.handle();

    let mut node = NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults());

    let dest_hash = DestinationHash::new([0x42; 16]);
    let signing_key = [0x33; 32];

    // Initiate link to non-existent destination
    let (link_id, _, _output) = node.connect(dest_hash, &signing_key);

    // Verify pending
    assert_eq!(node.pending_link_count(), 1);
    assert!(node.link(&link_id).is_some());

    // Three attempts, each with its own timeout. The base is 12_000ms but
    // the #129 establishment jitter adds up to 25% and re-rolls per retry,
    // so read the LIVE timeout from the link before each advance (the node's
    // link accessor resolves the re-keyed id via aliases).
    // Attempt 1: times out → retry (remaining: 2 → 1)
    let timeout_ms = node.link(&link_id).unwrap().establishment_timeout_ms();
    clock_handle.advance(timeout_ms + 1);
    let _ = node.handle_timeout();
    assert!(
        node.link(&link_id).is_some(),
        "link should survive first retry"
    );

    // Attempt 2: times out → retry (remaining: 1 → 0)
    let timeout_ms = node.link(&link_id).unwrap().establishment_timeout_ms();
    clock_handle.advance(timeout_ms + 1);
    let _ = node.handle_timeout();
    assert!(
        node.link(&link_id).is_some(),
        "link should survive second retry"
    );

    // Attempt 3: times out → no retries left → link removed
    let timeout_ms = node.link(&link_id).unwrap().establishment_timeout_ms();
    clock_handle.advance(timeout_ms + 1);
    let output = node.handle_timeout();

    // Link should be removed
    assert!(node.link(&link_id).is_none());
    assert_eq!(node.pending_link_count(), 0);

    // Should have LinkClosed with Timeout
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkClosed {
            link_id: id,
            reason: LinkCloseReason::Timeout,
            ..
        } if *id == link_id
    )));

    println!("SUCCESS: NodeCore handshake timeout");
}

/// Test sending on inactive link returns error.
#[tokio::test]
async fn test_manager_send_on_inactive_link() {
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    let dest_hash = DestinationHash::new([0x42; 16]);
    let signing_key = [0x33; 32];

    // Initiate link (pending, not active)
    let (link_id, _, _output) = node.connect(dest_hash, &signing_key);

    // Try to send - should fail
    let result = node.send_on_link(&link_id, b"test data");
    assert!(
        result.is_err(),
        "Should not be able to send on pending link"
    );

    println!("SUCCESS: NodeCore send on inactive link");
}

/// Test operations on unknown link ID.
#[tokio::test]
async fn test_manager_operations_on_unknown_link() {
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    let unknown_link_id = LinkId::new([0xFF; 16]);

    // send_on_link() on unknown link
    let send_result = node.send_on_link(&unknown_link_id, b"test");
    assert!(send_result.is_err());

    // close_link() on unknown link should not panic
    let _output = node.close_link(&unknown_link_id);

    // link() returns None
    assert!(node.link(&unknown_link_id).is_none());

    // is_active returns false
    assert!(!is_active(&node, &unknown_link_id));

    println!("SUCCESS: NodeCore operations on unknown link");
}

/// Test responder timeout when RTT is not received.
#[tokio::test]
async fn test_manager_responder_timeout() {
    let clock = SharedMockClock::new(1_000_000);
    let clock_handle = clock.handle();

    let identity = Identity::generate(&mut OsRng);

    let mut node = NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults());
    let _dest_hash_bytes = [0x42; 16];

    // Create a destination and register it
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "testapp",
        &["timeout"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    // Override the hash by registering with the transport directly
    // Actually, we need to use the destination's own hash. Let's get it.
    let actual_dest_hash = *dest.hash();
    node.register_destination(dest);

    // Simulate receiving a link request
    let initiator_link = leviculum_core::link::Link::new_outgoing(actual_dest_hash, &mut OsRng);
    let request_data = initiator_link.create_link_request();

    // Build raw packet
    let mut raw_packet = Vec::new();
    raw_packet.push(0x02); // flags
    raw_packet.push(0x00); // hops
    raw_packet.extend_from_slice(actual_dest_hash.as_bytes());
    raw_packet.push(0x00); // context
    raw_packet.extend_from_slice(&request_data);

    let link_id = leviculum_core::link::Link::calculate_link_id(&raw_packet);

    let output = node.handle_packet(InterfaceId(0), &raw_packet);

    // Auto-accept (Python parity): handle_packet proves the link inline and moves
    // it to PendingIncoming.
    assert!(
        !output.actions.is_empty(),
        "responder should auto-prove the incoming link"
    );

    // Verify pending incoming
    assert_eq!(node.pending_link_count(), 1);

    // Advance time past responder establishment timeout (don't send RTT).
    // Responder with 0 hops: 6000 * max(1,0) + 360000 = 366000ms.
    clock_handle.advance(366_001);
    let output = node.handle_timeout();

    // Link should be timed out
    assert!(node.link(&link_id).is_none());

    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkClosed {
            reason: LinkCloseReason::Timeout,
            ..
        }
    )));

    println!("SUCCESS: NodeCore responder timeout");
}

// =========================================================================
// 5. Stress Tests
// =========================================================================

/// Test establishing many simultaneous links.
#[tokio::test]
async fn test_manager_many_simultaneous_links() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register 10 destinations
    let mut destinations = Vec::new();
    for i in 0..10 {
        let dest = daemon
            .register_destination("stress", &[&format!("dest{}", i)])
            .await
            .expect("Failed to register destination");
        destinations.push(dest);
    }

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Initiate all links
    let mut link_ids = Vec::new();
    for dest in &destinations {
        let pub_key_bytes = hex::decode(&dest.public_key).unwrap();
        let signing_key: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();
        let dest_hash: [u8; 16] = hex::decode(&dest.hash).unwrap().try_into().unwrap();

        let (link_id, _, output) = node.connect(DestinationHash::new(dest_hash), &signing_key);
        link_ids.push(link_id);
        dispatch_actions(&mut stream, &output).await;
    }

    // Receive all proofs
    let mut established = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut buf = [0u8; 4096];

    while established < 10 && tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Proof {
                                let output = node.handle_packet(InterfaceId(0), &data);
                                // Dispatch RTT actions
                                dispatch_actions(&mut stream, &output).await;
                                for event in &output.events {
                                    if matches!(event, NodeEvent::LinkEstablished { .. }) {
                                        established += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        let output = node.handle_timeout();
        dispatch_actions(&mut stream, &output).await;
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    let active = node.active_link_count();
    println!("Established {} of 10 links", active);
    assert_eq!(
        active, 10,
        "All 10 links must be established. Got {}",
        active
    );

    println!("All 10 simultaneous links established");
}

/// Test rapid data exchange on a single link.
///
/// send_on_link() uses Channel (windowed, reliable delivery).
/// We send in batches, processing incoming proofs between batches to
/// keep the channel window open.
#[tokio::test]
async fn test_manager_rapid_data_exchange() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    let (link_id, _) = establish_initiator_link(&mut node, &daemon, &mut stream, &mut deframer)
        .await
        .expect("Failed to establish link");

    // Send packets in batches, processing incoming between batches
    // to allow the channel window to open
    let total = 20;
    let mut sent = 0;
    let mut buf = [0u8; 2048];

    for i in 0..total {
        let data = format!("Rapid message {}", i);
        match node.send_on_link(&link_id, data.as_bytes()) {
            Ok(output) => {
                dispatch_actions(&mut stream, &output).await;
                sent += 1;
            }
            Err(_) => {
                // Window full or pacing, process incoming and retry
                tokio::time::sleep(Duration::from_millis(100)).await;
                // Read and process any incoming packets (proofs/acks)
                if let Ok(Ok(n)) = timeout(Duration::from_millis(100), stream.read(&mut buf)).await
                {
                    if n > 0 {
                        for result in deframer.process(&buf[..n]) {
                            if let DeframeResult::Frame(data) = result {
                                let output = node.handle_packet(InterfaceId(0), &data);
                                dispatch_actions(&mut stream, &output).await;
                            }
                        }
                    }
                }
                let output = node.handle_timeout();
                dispatch_actions(&mut stream, &output).await;

                // Retry
                let data = format!("Rapid message {}", i);
                if let Ok(output) = node.send_on_link(&link_id, data.as_bytes()) {
                    dispatch_actions(&mut stream, &output).await;
                    sent += 1;
                }
            }
        }
    }

    // Wait for daemon to process
    tokio::time::sleep(Duration::from_secs(2)).await;

    let received = daemon.get_received_packets().await.unwrap();
    println!(
        "Daemon received {} of {} packets (sent {})",
        received.len(),
        total,
        sent
    );
    assert!(
        received.len() >= total - 2,
        "Most packets must be received. Got {}/{}",
        received.len(),
        total,
    );

    println!("Rapid exchange packets verified");
}

/// Test sending large payloads.
#[tokio::test]
async fn test_manager_large_payloads() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    let (link_id, _) = establish_initiator_link(&mut node, &daemon, &mut stream, &mut deframer)
        .await
        .expect("Failed to establish link");

    // Test various payload sizes.
    // send_on_link() uses Channel (windowed, reliable delivery) so we must
    // handle Busy/PacingDelay between sends.
    let sizes = [100, 250, 400];
    let mut buf = [0u8; 2048];

    for size in sizes {
        let data: Vec<u8> = (0..size).map(|i| (i & 0xFF) as u8).collect();
        let mut sent = false;
        for _attempt in 0..5 {
            match node.send_on_link(&link_id, &data) {
                Ok(output) => {
                    dispatch_actions(&mut stream, &output).await;
                    println!("Sent {} byte payload", size);
                    sent = true;
                    break;
                }
                Err(_) => {
                    // Window full or pacing, process incoming and retry
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if let Ok(Ok(n)) =
                        timeout(Duration::from_millis(200), stream.read(&mut buf)).await
                    {
                        if n > 0 {
                            for result in deframer.process(&buf[..n]) {
                                if let DeframeResult::Frame(frame_data) = result {
                                    let output = node.handle_packet(InterfaceId(0), &frame_data);
                                    dispatch_actions(&mut stream, &output).await;
                                }
                            }
                        }
                    }
                    let output = node.handle_timeout();
                    dispatch_actions(&mut stream, &output).await;
                }
            }
        }
        assert!(sent, "Failed to send {} byte packet after retries", size);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tokio::time::sleep(Duration::from_secs(1)).await;

    let received = daemon.get_received_packets().await.unwrap();
    println!("Daemon received {} packets", received.len());

    // Check we received all large payloads
    let received_sizes: Vec<_> = received.iter().map(|p| p.data.len()).collect();
    println!("Received payload sizes: {:?}", received_sizes);

    // All 3 payloads must be received (100, 250, 400 bytes)
    assert_eq!(
        received.len(),
        3,
        "All 3 payloads must be received. Got {}",
        received.len()
    );
    for expected_size in sizes {
        assert!(
            received_sizes.contains(&expected_size),
            "Must receive {} byte payload. Got sizes: {:?}",
            expected_size,
            received_sizes
        );
    }

    println!("All 3 large payloads verified");
}

/// Test interleaved operations across multiple links.
#[tokio::test]
async fn test_manager_interleaved_operations() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register 3 destinations
    let dest1 = daemon
        .register_destination("interleave", &["one"])
        .await
        .unwrap();
    let dest2 = daemon
        .register_destination("interleave", &["two"])
        .await
        .unwrap();
    let dest3 = daemon
        .register_destination("interleave", &["three"])
        .await
        .unwrap();

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Helper to initiate
    let initiate = |node: &mut leviculum_core::node::NodeCore<OsRng, TestClock, MemoryStorage>,
                    dest: &DestinationInfo|
     -> (LinkId, Vec<Vec<u8>>) {
        let pub_key_bytes = hex::decode(&dest.public_key).unwrap();
        let signing_key: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();
        let dest_hash: [u8; 16] = hex::decode(&dest.hash).unwrap().try_into().unwrap();
        let (link_id, _, output) = node.connect(DestinationHash::new(dest_hash), &signing_key);
        let packets = extract_action_packets(&output);
        (link_id, packets)
    };

    // Interleave: initiate 1, initiate 2, wait for proof 1, initiate 3, etc.
    let (link_id1, packets1) = initiate(&mut node, &dest1);
    for pkt in &packets1 {
        send_framed(&mut stream, pkt).await;
    }

    let (link_id2, packets2) = initiate(&mut node, &dest2);
    for pkt in &packets2 {
        send_framed(&mut stream, pkt).await;
    }

    // Receive some proofs
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (link_id3, packets3) = initiate(&mut node, &dest3);
    for pkt in &packets3 {
        send_framed(&mut stream, pkt).await;
    }

    // Process all proofs
    let mut established = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut buf = [0u8; 2048];

    while established < 3 && tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                for result in deframer.process(&buf[..n]) {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Proof {
                                let output = node.handle_packet(InterfaceId(0), &data);
                                dispatch_actions(&mut stream, &output).await;
                                for event in &output.events {
                                    if matches!(event, NodeEvent::LinkEstablished { .. }) {
                                        established += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Send data on whichever links are active, close one
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut sent_count = 0;
    for (i, link_id) in [link_id1, link_id2, link_id3].iter().enumerate() {
        if is_active(&node, link_id) {
            let data = format!("Data on link {}", i + 1);
            if let Ok(output) = node.send_on_link(link_id, data.as_bytes()) {
                dispatch_actions(&mut stream, &output).await;
                sent_count += 1;
            }
        }
    }

    // Close one active link
    if is_active(&node, &link_id2) {
        let _output = node.close_link(&link_id2);
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    println!(
        "Established {}, sent on {}, active now {}",
        established,
        sent_count,
        node.active_link_count()
    );

    assert_eq!(
        established, 3,
        "All 3 links must be established. Got {}",
        established
    );
    assert_eq!(
        sent_count, 3,
        "Must send on all 3 links. Got {}",
        sent_count
    );

    println!("All 3 interleaved links verified");
}
