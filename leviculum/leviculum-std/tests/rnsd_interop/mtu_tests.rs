//! MTU negotiation interop tests.
//!
//! These tests verify that link MTU negotiation works correctly across
//! different interface types (TCP, UDP) and through transport relays.
//!
//! ## Test Groups
//!
//! - **A: TCP**, negotiated MTU over TCP links through a Python daemon
//! - **B: UDP**, negotiated MTU over UDP links (Python-to-Python baseline, Rust-to-Rust, Rust-to-Python, Python-to-Rust)
//! - **C: Boundary**, exact MDU, one-byte, and over-MDU payloads
//! - **D: Multi-hop**. MTU clamping through mixed-interface relays
//!
//! ## Python Daemon MTU Behavior
//!
//! The Python daemon's `optimise_mtu()` sets HW_MTU based on measured bitrate.
//! With the default `BITRATE_GUESS = 10_000_000` (10 Mbps) and the strictly-greater
//! check `> 10_000_000`, the bitrate falls through to `> 5_000_000 → HW_MTU = 8192`.
//! All daemon TCP tests therefore negotiate MTU=8192, not the class-level 262144.
//! (Reticulum 1.5.2 turned those checks into `>=`, which puts the same guess
//! on 16384. We signal the derived value too since Codeberg #355 — the vendored
//! reference here is 1.3.5, so its 8192 wins the per-hop clamp in these tests.)
//!
//! For UDP, Python's UDPInterface has `AUTOCONFIGURE_MTU=False` and `FIXED_MTU=False`,
//! so `Transport.next_hop_interface_hw_mtu()` returns None and the Transport layer
//! clamps link MTU to the base protocol MTU (500). Rust-to-Rust UDP links
//! negotiate the full UDP HW_MTU (1064), but Python interop tests negotiate 500.
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop mtu_tests
//! cargo test --package leviculum-std --test rnsd_interop mtu_tests -- --nocapture
//! ```

use std::time::Duration;

use rand_core::OsRng;
use tokio::net::TcpStream;

use leviculum_core::link::{LinkId, LinkState};
use leviculum_core::node::{NodeCoreBuilder, NodeEvent};
use leviculum_core::traits::{Clock, Storage};
use leviculum_core::transport::{Action, InterfaceId};
use leviculum_core::{DestinationHash, DestinationType, Direction, MemoryStorage};
use leviculum_std::interfaces::hdlc::Deframer;

use crate::common::{
    connect_to_daemon, generate_test_payload, receive_raw_proof_for_link, send_framed,
    setup_rust_destination, verify_test_payload, wait_for_any_announce_with_route_info,
    wait_for_data_event, wait_for_link_data_packet, wait_for_link_established,
    wait_for_link_request, wait_for_responder_established_link, wait_for_rtt_packet, TestClock,
    DAEMON_TCP_LINK_MDU, DAEMON_TCP_MAX_CHANNEL_PAYLOAD, DAEMON_TCP_NEGOTIATED_MTU,
    DIRECT_TCP_LINK_MDU, DIRECT_TCP_MAX_CHANNEL_PAYLOAD, TCP_HW_MTU, UDP_HW_MTU, UDP_LINK_MDU,
    UDP_MAX_CHANNEL_PAYLOAD,
};
use crate::harness::TestDaemon;

// =========================================================================
// Helper functions
// =========================================================================

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

/// Check if a link is active on a NodeCore.
fn is_active<R: rand_core::CryptoRngCore, C: Clock, S: Storage>(
    node: &leviculum_core::node::NodeCore<R, C, S>,
    link_id: &LinkId,
) -> bool {
    node.link(link_id)
        .map(|l| l.state() == LinkState::Active)
        .unwrap_or(false)
}

/// Establish a full Rust-to-Rust link via daemon relay.
///
/// Returns `(link_id_a, link_id_b)`, both should be the same LinkId.
/// `node_a` is responder, `node_b` is initiator.
async fn establish_rust_to_rust_link(
    node_a: &mut leviculum_core::node::NodeCore<OsRng, TestClock, MemoryStorage>,
    node_b: &mut leviculum_core::node::NodeCore<OsRng, TestClock, MemoryStorage>,
    stream_a: &mut TcpStream,
    stream_b: &mut TcpStream,
    deframer_a: &mut Deframer,
    deframer_b: &mut Deframer,
    dest_hash_a: DestinationHash,
) -> (LinkId, LinkId) {
    // B receives A's announce over the wire
    let announce_info =
        wait_for_any_announce_with_route_info(stream_b, deframer_b, Duration::from_secs(5))
            .await
            .expect("B should receive A's announce from daemon");

    assert_eq!(
        announce_info.packet.destination_hash, dest_hash_a,
        "Announce should be for A's destination"
    );

    let signing_key_a = announce_info
        .signing_key()
        .expect("Failed to extract signing key from announce");

    // Feed announce to B's transport so it learns the path
    let _ = node_b.handle_packet(InterfaceId(0), &announce_info.raw_data);

    // B initiates link to A
    let (link_id_b, _, output) = node_b.connect(dest_hash_a, &signing_key_a);
    dispatch_actions(stream_b, &output).await;

    // A receives link request
    let (raw_request, link_id_a_bytes) =
        wait_for_link_request(stream_a, deframer_a, &dest_hash_a, Duration::from_secs(10))
            .await
            .expect("A should receive link request");
    let link_id_a = LinkId::new(link_id_a_bytes);

    assert_eq!(link_id_a, link_id_b, "Link IDs should match");

    // Auto-accept (Stage 1): handle_packet builds + routes the proof inline.
    let output = node_a.handle_packet(InterfaceId(0), &raw_request);

    // Send proof (auto-sent inline by handle_packet)
    dispatch_actions(stream_a, &output).await;

    // B receives proof
    let proof_raw =
        receive_raw_proof_for_link(stream_b, deframer_b, &link_id_b, Duration::from_secs(10))
            .await
            .expect("B should receive proof");

    let output = node_b.handle_packet(InterfaceId(0), &proof_raw);
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkEstablished {
            is_initiator: true,
            ..
        }
    )));

    // Dispatch RTT from B
    dispatch_actions(stream_b, &output).await;

    // A receives RTT
    let rtt_data = wait_for_rtt_packet(stream_a, deframer_a, &link_id_a, Duration::from_secs(10))
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
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkEstablished {
            is_initiator: false,
            ..
        }
    )));

    assert!(is_active(node_a, &link_id_a));
    assert!(is_active(node_b, &link_id_b));

    (link_id_a, link_id_b)
}

// =========================================================================
// Group A: TCP MTU negotiation
// =========================================================================

/// A1: Rust-to-Rust over TCP, verify negotiated MTU and full-MDU data transfer.
///
/// RustA (responder) <-TCP-> PyDaemon (relay) <-TCP-> RustB (initiator)
///
/// The daemon's spawned TCP interfaces have HW_MTU=8192 (see module docs),
/// so the daemon relay clamps the signaling bytes to 8192.
///
/// Verifies:
/// - Both nodes negotiate MTU=8192 (clamped by daemon relay)
/// - MDU=8111 on both sides
/// - Full channel-MDU payload (8105 bytes) transfers correctly
#[tokio::test]
async fn test_mtu_a1_rust_to_rust_tcp_mtu_and_data() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Node B connects first to receive announces
    let mut stream_b = connect_to_daemon(&daemon).await;
    let mut deframer_b = Deframer::new();
    let mut node_b = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_b.set_interface_hw_mtu(0, TCP_HW_MTU);

    // Node A (Responder)
    let mut stream_a = connect_to_daemon(&daemon).await;
    let mut deframer_a = Deframer::new();

    let (destination_a, _) =
        setup_rust_destination(&mut stream_a, "mtu_a1", &["test"], b"rust-A").await;
    let dest_hash_a = *destination_a.hash();

    let mut node_a = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_a.set_interface_hw_mtu(0, TCP_HW_MTU);
    let mut dest_a = destination_a;
    dest_a.set_accepts_links(true);
    node_a.register_destination(dest_a);

    // Wait for daemon to process announce
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Establish link
    let (link_id_a, link_id_b) = establish_rust_to_rust_link(
        &mut node_a,
        &mut node_b,
        &mut stream_a,
        &mut stream_b,
        &mut deframer_a,
        &mut deframer_b,
        dest_hash_a,
    )
    .await;

    // Verify negotiated MTU on both sides (clamped by daemon to 8192)
    let mtu_a = node_a.link(&link_id_a).unwrap().negotiated_mtu();
    let mtu_b = node_b.link(&link_id_b).unwrap().negotiated_mtu();
    assert_eq!(
        mtu_a, DAEMON_TCP_NEGOTIATED_MTU,
        "A should negotiate daemon TCP MTU"
    );
    assert_eq!(
        mtu_b, DAEMON_TCP_NEGOTIATED_MTU,
        "B should negotiate daemon TCP MTU"
    );

    let mdu_a = node_a.link(&link_id_a).unwrap().mdu();
    let mdu_b = node_b.link(&link_id_b).unwrap().mdu();
    assert_eq!(
        mdu_a, DAEMON_TCP_LINK_MDU,
        "A MDU should be {}",
        DAEMON_TCP_LINK_MDU
    );
    assert_eq!(
        mdu_b, DAEMON_TCP_LINK_MDU,
        "B MDU should be {}",
        DAEMON_TCP_LINK_MDU
    );

    // Let daemon finalize link table
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send full channel-MDU payload from B to A
    let payload = generate_test_payload(DAEMON_TCP_MAX_CHANNEL_PAYLOAD);
    let output = node_b
        .send_on_link(&link_id_b, &payload)
        .expect("Failed to send full-MDU payload");
    dispatch_actions(&mut stream_b, &output).await;

    // A receives data
    let data_raw = wait_for_link_data_packet(
        &mut stream_a,
        &mut deframer_a,
        &link_id_a,
        Duration::from_secs(30),
    )
    .await
    .expect("A should receive data from B");

    let output = node_a.handle_packet(InterfaceId(0), &data_raw);
    dispatch_actions(&mut stream_a, &output).await;

    let received = output.events.iter().find_map(|e| match e {
        NodeEvent::LinkDataReceived { data, .. } => Some(data.clone()),
        NodeEvent::MessageReceived { data, .. } => Some(data.clone()),
        _ => None,
    });

    let received_data = received.expect("Should receive data event");
    assert_eq!(
        received_data.len(),
        DAEMON_TCP_MAX_CHANNEL_PAYLOAD,
        "Received payload should be full channel-MDU size"
    );
    assert!(
        verify_test_payload(&received_data),
        "Payload integrity check failed"
    );

    println!(
        "SUCCESS: A1 - Rust-to-Rust TCP MTU={}, MDU={}, payload {} bytes verified",
        DAEMON_TCP_NEGOTIATED_MTU, DAEMON_TCP_LINK_MDU, DAEMON_TCP_MAX_CHANNEL_PAYLOAD
    );
}

/// A2: Rust-to-Python over TCP, verify both sides agree on MTU.
///
/// Rust (initiator) <-TCP-> PyDaemon (responder)
///
/// The daemon's final-hop code clamps the link request's signaled MTU to its
/// interface HW_MTU (8192). Both sides negotiate 8192.
#[tokio::test]
async fn test_mtu_a2_rust_to_python_tcp_mtu() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node.set_interface_hw_mtu(0, TCP_HW_MTU);

    // Register and announce destination on daemon
    let dest_info = daemon
        .register_destination("mtu_a2", &["test"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest_info.hash, b"python-responder")
        .await
        .expect("Failed to announce");

    // Receive announce on Rust side so transport learns the path
    let announce_info =
        wait_for_any_announce_with_route_info(&mut stream, &mut deframer, Duration::from_secs(5))
            .await
            .expect("Should receive daemon's announce");

    let _ = node.handle_packet(InterfaceId(0), &announce_info.raw_data);

    let signing_key = announce_info
        .signing_key()
        .expect("Failed to extract signing key");

    let dest_hash_bytes: [u8; 16] = hex::decode(&dest_info.hash).unwrap().try_into().unwrap();
    let dest_hash = DestinationHash::new(dest_hash_bytes);

    // Initiate link, now has path from announce, so signaling bytes will be included
    let (link_id, _, output) = node.connect(dest_hash, &signing_key);
    dispatch_actions(&mut stream, &output).await;

    // Wait for proof
    let proof_raw = receive_raw_proof_for_link(
        &mut stream,
        &mut deframer,
        &link_id,
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive proof");

    let output = node.handle_packet(InterfaceId(0), &proof_raw);
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkEstablished {
            is_initiator: true,
            ..
        }
    )));
    dispatch_actions(&mut stream, &output).await;

    // Verify Rust-side MTU (clamped by daemon to 8192)
    let link = node.link(&link_id).unwrap();
    assert_eq!(
        link.negotiated_mtu(),
        DAEMON_TCP_NEGOTIATED_MTU,
        "Rust should negotiate daemon TCP MTU"
    );
    assert_eq!(
        link.mdu(),
        DAEMON_TCP_LINK_MDU,
        "Rust MDU should match daemon TCP formula"
    );

    // Wait for daemon to finalize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify Python-side MTU via get_link_status
    let link_hash = hex::encode(link_id.as_bytes());
    let status = daemon
        .get_link_status(&link_hash)
        .await
        .expect("Should get link status");

    assert_eq!(status.status, "found");
    if let Some(mtu) = status.mtu {
        assert_eq!(
            mtu, DAEMON_TCP_NEGOTIATED_MTU,
            "Python should report daemon TCP MTU"
        );
    }

    println!(
        "SUCCESS: A2 - Rust-to-Python TCP MTU={}, Python reports mtu={:?}, mdu={:?}",
        link.negotiated_mtu(),
        status.mtu,
        status.mdu
    );
}

/// A3: Python-to-Rust over TCP, verify responder side MTU.
///
/// PyDaemon (initiator) <-TCP-> Rust (responder)
///
/// `create_link` blocks until the link is fully established, so we must
/// spawn it as a background task while the main thread handles the
/// handshake (receive link request, accept, send proof).
#[tokio::test]
async fn test_mtu_a3_python_to_rust_tcp_mtu() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "mtu_a3", &["test"], b"rust-responder").await;
    let dest_hash = *destination.hash();

    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node.set_interface_hw_mtu(0, TCP_HW_MTU);
    let mut dest = destination;
    dest.set_accepts_links(true);
    node.register_destination(dest);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Spawn create_link in background, it blocks until link is fully established
    let dest_hash_hex = hex::encode(dest_hash.as_bytes());
    let daemon_cmd_addr = daemon.cmd_addr();
    let dest_hash_hex_clone = dest_hash_hex.clone();
    let public_key_hex_clone = public_key_hex.clone();

    let link_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut cmd_stream = TcpStream::connect(daemon_cmd_addr)
            .await
            .expect("Failed to connect to daemon cmd");
        let cmd = serde_json::json!({
            "method": "create_link",
            "params": {
                "dest_hash": dest_hash_hex_clone,
                "dest_key": public_key_hex_clone,
                "timeout": 15
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

    // Wait for link request from Python
    let (raw_request, link_id_bytes) = wait_for_link_request(
        &mut stream,
        &mut deframer,
        &dest_hash,
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive link request");
    let link_id = LinkId::new(link_id_bytes);

    // Auto-accept (Stage 1): handle_packet builds + routes the proof inline.
    let output = node.handle_packet(InterfaceId(0), &raw_request);

    // Send proof (auto-sent inline by handle_packet)
    dispatch_actions(&mut stream, &output).await;

    // Wait for RTT
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
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkEstablished {
            is_initiator: false,
            ..
        }
    )));

    // Wait for the background create_link to complete
    let link_result = link_task.await.expect("link_task panicked");
    if let Some(error) = link_result.get("error") {
        panic!("Python create_link failed: {}", error);
    }

    // Use the link_id we already got from the handshake
    let link_hash = hex::encode(link_id.as_bytes());

    // Verify Rust-side MTU (clamped by daemon to 8192)
    let link = node.link(&link_id).unwrap();
    assert_eq!(
        link.negotiated_mtu(),
        DAEMON_TCP_NEGOTIATED_MTU,
        "Rust responder should negotiate daemon TCP MTU"
    );

    // Verify Python-side MTU
    tokio::time::sleep(Duration::from_millis(500)).await;
    let status = daemon
        .get_link_status(&link_hash)
        .await
        .expect("Should get link status");

    if let Some(mtu) = status.mtu {
        assert_eq!(
            mtu, DAEMON_TCP_NEGOTIATED_MTU,
            "Python initiator should report daemon TCP MTU"
        );
    }

    println!(
        "SUCCESS: A3 - Python-to-Rust TCP MTU={}, Python reports mtu={:?}",
        link.negotiated_mtu(),
        status.mtu
    );
}

/// A4: Bidirectional full-MDU over TCP.
///
/// Same setup as A1, but sends full-MDU payload in both directions.
#[tokio::test]
async fn test_mtu_a4_bidirectional_tcp_full_mdu() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream_b = connect_to_daemon(&daemon).await;
    let mut deframer_b = Deframer::new();
    let mut node_b = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_b.set_interface_hw_mtu(0, TCP_HW_MTU);

    let mut stream_a = connect_to_daemon(&daemon).await;
    let mut deframer_a = Deframer::new();

    let (destination_a, _) =
        setup_rust_destination(&mut stream_a, "mtu_a4", &["test"], b"rust-A").await;
    let dest_hash_a = *destination_a.hash();

    let mut node_a = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_a.set_interface_hw_mtu(0, TCP_HW_MTU);
    let mut dest_a = destination_a;
    dest_a.set_accepts_links(true);
    node_a.register_destination(dest_a);

    tokio::time::sleep(Duration::from_millis(500)).await;

    let (link_id_a, link_id_b) = establish_rust_to_rust_link(
        &mut node_a,
        &mut node_b,
        &mut stream_a,
        &mut stream_b,
        &mut deframer_a,
        &mut deframer_b,
        dest_hash_a,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // B -> A: full channel-MDU
    let payload_b = generate_test_payload(DAEMON_TCP_MAX_CHANNEL_PAYLOAD);
    let output = node_b
        .send_on_link(&link_id_b, &payload_b)
        .expect("B send failed");
    dispatch_actions(&mut stream_b, &output).await;

    let data_raw = wait_for_link_data_packet(
        &mut stream_a,
        &mut deframer_a,
        &link_id_a,
        Duration::from_secs(30),
    )
    .await
    .expect("A should receive B's data");

    let output = node_a.handle_packet(InterfaceId(0), &data_raw);
    dispatch_actions(&mut stream_a, &output).await;

    let received_a = output.events.iter().find_map(|e| match e {
        NodeEvent::LinkDataReceived { data, .. } | NodeEvent::MessageReceived { data, .. } => {
            Some(data.clone())
        }
        _ => None,
    });
    assert!(
        verify_test_payload(&received_a.expect("A should receive data")),
        "B->A payload integrity failed"
    );

    tokio::time::sleep(Duration::from_millis(200)).await;

    // A -> B: full channel-MDU
    let payload_a = generate_test_payload(DAEMON_TCP_MAX_CHANNEL_PAYLOAD);
    let output = node_a
        .send_on_link(&link_id_a, &payload_a)
        .expect("A send failed");
    dispatch_actions(&mut stream_a, &output).await;

    let data_raw = wait_for_link_data_packet(
        &mut stream_b,
        &mut deframer_b,
        &link_id_b,
        Duration::from_secs(30),
    )
    .await
    .expect("B should receive A's data");

    let output = node_b.handle_packet(InterfaceId(0), &data_raw);
    dispatch_actions(&mut stream_b, &output).await;

    let received_b = output.events.iter().find_map(|e| match e {
        NodeEvent::LinkDataReceived { data, .. } | NodeEvent::MessageReceived { data, .. } => {
            Some(data.clone())
        }
        _ => None,
    });
    assert!(
        verify_test_payload(&received_b.expect("B should receive data")),
        "A->B payload integrity failed"
    );

    println!(
        "SUCCESS: A4 - Bidirectional {} bytes both directions verified",
        DAEMON_TCP_MAX_CHANNEL_PAYLOAD
    );
}

/// A0: Direct Rust-to-Rust TCP, no Python daemon, undamped TCP HW_MTU.
///
/// RustA (TCP server, responder) <-TCP-> RustB (TCP client, initiator)
///
/// Without a Python peer on the path, nothing clamps the signalled value, so
/// both sides negotiate our own TCP HW_MTU — the bitrate-derived 16384 since
/// Codeberg #355, which is what a Reticulum 1.5.2 peer derives as well.
///
/// Verifies:
/// - negotiated MTU = TCP_HW_MTU (16384)
/// - MDU = 16303
/// - Full channel-MDU payload (16297 bytes) transfers correctly
#[tokio::test]
async fn test_mtu_a0_direct_tcp_full_mtu() {
    use leviculum_core::identity::Identity;
    use leviculum_std::driver::ReticulumNodeBuilder;

    // Allocate a random TCP port
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp_port = listener.local_addr().unwrap().port();
    drop(listener);

    let tcp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();

    // Create identity for A (responder) before building node
    let identity_a = Identity::generate(&mut OsRng);
    let pub_key_bytes = identity_a.public_key_bytes();
    let signing_key: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    let mut dest_a = leviculum_core::Destination::new(
        Some(identity_a),
        Direction::In,
        DestinationType::Single,
        "mtu_a0",
        &["test"],
    )
    .unwrap();
    let dest_hash_a = *dest_a.hash();
    dest_a.set_accepts_links(true);

    // Node A: TCP server (responder)
    let _storage1 = crate::common::temp_storage("test_mtu_a0_direct_tcp_full_mtu", "node1");
    let mut node_a = ReticulumNodeBuilder::new()
        .add_tcp_server(tcp_addr)
        .storage_path(_storage1.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node A");
    node_a.start().await.expect("Failed to start node A");
    node_a.register_destination(dest_a);

    // Small delay to let TCP server bind
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Node B: TCP client (initiator)
    let _storage2 = crate::common::temp_storage("test_mtu_a0_direct_tcp_full_mtu", "node2");
    let mut node_b = ReticulumNodeBuilder::new()
        .add_tcp_client(tcp_addr)
        .storage_path(_storage2.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node B");
    node_b.start().await.expect("Failed to start node B");

    let mut events_a = node_a.take_event_receiver().unwrap();
    let mut events_b = node_b.take_event_receiver().unwrap();

    // Wait for TCP connection to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // A announces
    node_a
        .announce_destination(&dest_hash_a, Some(b"responder"))
        .await
        .expect("A announce failed");

    // Wait for B to learn path to A
    assert!(
        crate::common::wait_for_node_reannounce(
            || node_b.has_path(&dest_hash_a),
            &node_a,
            &dest_hash_a,
            b"responder",
            Duration::from_secs(5),
        )
        .await,
        "B should learn path to A"
    );

    // B connects to A
    let link_handle = node_b
        .connect(&dest_hash_a, &signing_key)
        .await
        .expect("B connect failed");
    let link_id_b = link_handle.link_id();

    // A's incoming link is auto-accepted and proved; wait for it to establish.
    let link_id_a = wait_for_responder_established_link(&mut events_a, Duration::from_secs(10))
        .await
        .expect("A should establish incoming link");

    // Wait for B side to be established (A already established above).
    assert!(
        wait_for_link_established(&mut events_b, link_id_b, Duration::from_secs(10)).await,
        "B link should be established"
    );

    // Verify negotiated MTU, full TCP HW_MTU, no daemon clamping
    let mtu_a = node_a.link_negotiated_mtu(&link_id_a);
    let mtu_b = node_b.link_negotiated_mtu(link_id_b);
    assert_eq!(
        mtu_a,
        Some(TCP_HW_MTU),
        "A should negotiate full TCP HW_MTU"
    );
    assert_eq!(
        mtu_b,
        Some(TCP_HW_MTU),
        "B should negotiate full TCP HW_MTU"
    );

    let mdu_a = node_a.link_mdu(&link_id_a);
    let mdu_b = node_b.link_mdu(link_id_b);
    assert_eq!(
        mdu_a,
        Some(DIRECT_TCP_LINK_MDU),
        "A MDU should be {}",
        DIRECT_TCP_LINK_MDU
    );
    assert_eq!(
        mdu_b,
        Some(DIRECT_TCP_LINK_MDU),
        "B MDU should be {}",
        DIRECT_TCP_LINK_MDU
    );

    // Send full channel-MDU payload from B to A (262057 bytes)
    let payload = generate_test_payload(DIRECT_TCP_MAX_CHANNEL_PAYLOAD);
    link_handle.send(&payload).await.expect("B send failed");

    let received = wait_for_data_event(&mut events_a, &link_id_a, Duration::from_secs(30))
        .await
        .expect("A should receive data from B");

    assert_eq!(
        received.len(),
        DIRECT_TCP_MAX_CHANNEL_PAYLOAD,
        "Received payload should be full direct TCP channel-MDU size"
    );
    assert!(
        verify_test_payload(&received),
        "Payload integrity check failed"
    );

    println!(
        "SUCCESS: A0 - Direct TCP MTU={}, MDU={}, payload {} bytes verified",
        TCP_HW_MTU, DIRECT_TCP_LINK_MDU, DIRECT_TCP_MAX_CHANNEL_PAYLOAD
    );

    node_a.stop().await.ok();
    node_b.stop().await.ok();
}

// =========================================================================
// Group B: UDP MTU negotiation
// =========================================================================

/// Python-to-Python UDP baseline: verify two Python daemons negotiate MTU=500.
///
/// Two Python daemons connected via UDP. Daemon A initiates a link to a
/// destination on daemon B. Both sides should report MTU=500 (base protocol MTU),
/// confirming that Python's UDPInterface does NOT set FIXED_MTU or AUTOCONFIGURE_MTU.
///
/// This baseline validates our code-reading conclusion independently and catches
/// any future Python changes to UDPInterface MTU flags.
#[tokio::test]
async fn test_mtu_b0_python_to_python_udp_baseline() {
    use crate::common::DAEMON_UDP_NEGOTIATED_MTU;
    use crate::harness::find_available_ports;

    // Allocate 6 ports: [rns_a, cmd_a, udp_a, rns_b, cmd_b, udp_b]
    let (ports, _port_alloc) = find_available_ports::<6>()
        .await
        .expect("Failed to find 6 available ports");
    let [rns_a, cmd_a, udp_a, rns_b, cmd_b, udp_b] = ports;

    // Start daemon A: listens on udp_a, forwards to udp_b. Hold _port_alloc
    // until both daemons are READY so the alloc → bind handoff cannot race.
    let daemon_a = TestDaemon::start_with_udp_ports(rns_a, cmd_a, udp_a, udp_b)
        .await
        .expect("Failed to start daemon A");

    // Start daemon B: listens on udp_b, forwards to udp_a
    let daemon_b = TestDaemon::start_with_udp_ports(rns_b, cmd_b, udp_b, udp_a)
        .await
        .expect("Failed to start daemon B");

    // Register destination on daemon B (accepts links)
    let dest_info = daemon_b
        .register_destination("mtu_b0", &["test"])
        .await
        .expect("Failed to register destination on daemon B");

    // Announce from B so A learns the path
    daemon_b
        .announce_destination(&dest_info.hash, b"python-udp-b0")
        .await
        .expect("Failed to announce on daemon B");

    // Wait for daemon A to learn the path
    let dest_hash = crate::common::parse_dest_hash(&dest_info.hash);
    assert!(
        crate::common::wait_for_path_reannounce_on_daemon(
            &daemon_a,
            &dest_hash,
            &daemon_b,
            &dest_info.hash,
            b"python-udp-b0",
            Duration::from_secs(5),
        )
        .await,
        "Daemon A should learn path to daemon B"
    );

    // Daemon A creates a link to B
    let link_hash = daemon_a
        .create_link(&dest_info.hash, &dest_info.public_key, 15)
        .await
        .expect("Daemon A create_link failed");

    // Wait briefly for link to stabilize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify MTU on daemon A (initiator)
    let status_a = daemon_a
        .get_link_status(&link_hash)
        .await
        .expect("Should get link status from daemon A");
    assert_eq!(status_a.status, "found", "Daemon A link should be found");
    if let Some(mtu_a) = status_a.mtu {
        assert_eq!(
            mtu_a, DAEMON_UDP_NEGOTIATED_MTU,
            "Daemon A (initiator) should negotiate MTU={DAEMON_UDP_NEGOTIATED_MTU} over UDP"
        );
    }

    // Verify MTU on daemon B (responder)
    let status_b = daemon_b
        .get_link_status(&link_hash)
        .await
        .expect("Should get link status from daemon B");
    assert_eq!(status_b.status, "found", "Daemon B link should be found");
    if let Some(mtu_b) = status_b.mtu {
        assert_eq!(
            mtu_b, DAEMON_UDP_NEGOTIATED_MTU,
            "Daemon B (responder) should negotiate MTU={DAEMON_UDP_NEGOTIATED_MTU} over UDP"
        );
    }

    println!(
        "SUCCESS: Python-to-Python UDP baseline: A mtu={:?}, B mtu={:?} (expected {})",
        status_a.mtu, status_b.mtu, DAEMON_UDP_NEGOTIATED_MTU
    );
}

/// Rust-to-Rust over UDP: verify negotiated MTU=1064 and data transfer.
///
/// Uses ReticulumNodeBuilder with UDP interfaces.
#[tokio::test]
async fn test_mtu_b1_rust_to_rust_udp_mtu() {
    use crate::common::{
        wait_for_data_event, wait_for_link_established, wait_for_responder_established_link,
        UDP_HW_MTU, UDP_LINK_MDU, UDP_MAX_CHANNEL_PAYLOAD,
    };
    use leviculum_core::identity::Identity;
    use leviculum_std::driver::ReticulumNodeBuilder;

    // Allocate two UDP port pairs (A and B need to cross-connect)
    let listener1 = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port_a = listener1.local_addr().unwrap().port();
    drop(listener1);

    let listener2 = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port_b = listener2.local_addr().unwrap().port();
    drop(listener2);

    // Create identity and destination for A before building the node,
    // so we can extract the signing key for B to use.
    let identity_a = Identity::generate(&mut OsRng);
    let pub_key_bytes = identity_a.public_key_bytes();
    // Ed25519 verifying key is the second 32 bytes of the 64-byte public key
    let signing_key: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    let mut dest_a = leviculum_core::Destination::new(
        Some(identity_a),
        Direction::In,
        DestinationType::Single,
        "mtu_b1",
        &["test"],
    )
    .unwrap();
    let dest_hash_a = *dest_a.hash();
    dest_a.set_accepts_links(true);

    // Node A: listens on port_a, forwards to port_b (Responder)
    let _storage1 = crate::common::temp_storage("test_mtu_b1_rust_to_rust_udp_mtu", "node1");
    let mut node_a = ReticulumNodeBuilder::new()
        .add_udp_interface(
            format!("127.0.0.1:{}", port_a).parse().unwrap(),
            format!("127.0.0.1:{}", port_b).parse().unwrap(),
        )
        .storage_path(_storage1.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node A");
    node_a.start().await.expect("Failed to start node A");
    node_a.register_destination(dest_a);

    // Node B: listens on port_b, forwards to port_a (Initiator)
    let _storage2 = crate::common::temp_storage("test_mtu_b1_rust_to_rust_udp_mtu", "node2");
    let mut node_b = ReticulumNodeBuilder::new()
        .add_udp_interface(
            format!("127.0.0.1:{}", port_b).parse().unwrap(),
            format!("127.0.0.1:{}", port_a).parse().unwrap(),
        )
        .storage_path(_storage2.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node B");
    node_b.start().await.expect("Failed to start node B");

    let mut events_a = node_a.take_event_receiver().unwrap();
    let mut events_b = node_b.take_event_receiver().unwrap();

    // A announces
    node_a
        .announce_destination(&dest_hash_a, Some(b"responder"))
        .await
        .expect("A announce failed");

    // Wait for B to learn path to A
    assert!(
        crate::common::wait_for_node_reannounce(
            || node_b.has_path(&dest_hash_a),
            &node_a,
            &dest_hash_a,
            b"responder",
            Duration::from_secs(5),
        )
        .await,
        "B should learn path to A"
    );

    // B connects to A using the signing key extracted before node creation
    let link_handle = node_b
        .connect(&dest_hash_a, &signing_key)
        .await
        .expect("B connect failed");
    let link_id_b = link_handle.link_id();

    // A's incoming link is auto-accepted and proved; wait for it to establish.
    let link_id_a = wait_for_responder_established_link(&mut events_a, Duration::from_secs(10))
        .await
        .expect("A should establish incoming link");

    // Wait for B side to be established (A already established above).
    assert!(
        wait_for_link_established(&mut events_b, link_id_b, Duration::from_secs(10)).await,
        "B link should be established"
    );

    // Verify negotiated MTU
    let mtu_a = node_a.link_negotiated_mtu(&link_id_a);
    let mtu_b = node_b.link_negotiated_mtu(link_id_b);
    assert_eq!(mtu_a, Some(UDP_HW_MTU), "A should negotiate UDP HW_MTU");
    assert_eq!(mtu_b, Some(UDP_HW_MTU), "B should negotiate UDP HW_MTU");

    let mdu_a = node_a.link_mdu(&link_id_a);
    let mdu_b = node_b.link_mdu(link_id_b);
    assert_eq!(
        mdu_a,
        Some(UDP_LINK_MDU),
        "A MDU should be {}",
        UDP_LINK_MDU
    );
    assert_eq!(
        mdu_b,
        Some(UDP_LINK_MDU),
        "B MDU should be {}",
        UDP_LINK_MDU
    );

    // Send max channel payload from B to A
    let payload = generate_test_payload(UDP_MAX_CHANNEL_PAYLOAD);
    link_handle.send(&payload).await.expect("B send failed");

    let received = wait_for_data_event(&mut events_a, &link_id_a, Duration::from_secs(10))
        .await
        .expect("A should receive data");

    assert_eq!(received.len(), UDP_MAX_CHANNEL_PAYLOAD);
    assert!(
        verify_test_payload(&received),
        "UDP payload integrity failed"
    );

    println!(
        "SUCCESS: Rust-to-Rust UDP MTU={}, MDU={}, payload {} bytes verified",
        UDP_HW_MTU, UDP_LINK_MDU, UDP_MAX_CHANNEL_PAYLOAD
    );

    node_a.stop().await.ok();
    node_b.stop().await.ok();
}

/// Rust-to-Python over UDP: verify both sides agree on base MTU=500.
///
/// Rust (initiator, UDP) → Python daemon (responder, UDP)
///
/// Python's UDPInterface has AUTOCONFIGURE_MTU=False and FIXED_MTU=False,
/// so Transport.inbound() clamps the link MTU to RNS.Reticulum.MTU=500
/// even though UDPInterface.HW_MTU=1064. Both sides negotiate 500.
#[tokio::test]
async fn test_mtu_b2_rust_to_python_udp() {
    use crate::common::{
        extract_signing_key, parse_dest_hash, wait_for_link_established, DAEMON_UDP_LINK_MDU,
        DAEMON_UDP_MAX_CHANNEL_PAYLOAD, DAEMON_UDP_NEGOTIATED_MTU,
    };
    use leviculum_std::driver::ReticulumNodeBuilder;

    let daemon = TestDaemon::start_with_udp()
        .await
        .expect("Failed to start daemon with UDP");

    let py_listen = daemon
        .udp_listen_addr()
        .expect("daemon should have UDP listen addr");
    let rust_listen = daemon
        .udp_forward_addr()
        .expect("daemon should have UDP forward addr");

    let _storage = crate::common::temp_storage("test_mtu_b2_rust_to_python_udp", "node");
    let mut node = ReticulumNodeBuilder::new()
        .add_udp_interface(rust_listen, py_listen)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");

    let mut events = node
        .take_event_receiver()
        .expect("Failed to get event receiver");

    // Wait for UDP interface to settle
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Register and announce a destination on the Python side
    let dest_info = daemon
        .register_destination("mtu_b2", &["test"])
        .await
        .expect("Failed to register destination");

    daemon
        .announce_destination(&dest_info.hash, b"python-udp-responder")
        .await
        .expect("Failed to announce");

    // Wait for Rust to learn the path
    let dest_hash = parse_dest_hash(&dest_info.hash);
    assert!(
        crate::common::wait_for_path_reannounce(
            || node.has_path(&dest_hash),
            &daemon,
            &dest_info.hash,
            b"python-udp-responder",
            Duration::from_secs(5),
        )
        .await,
        "Rust should learn path to Python destination over UDP"
    );

    // Initiate link
    let signing_key = extract_signing_key(&dest_info.public_key);
    let link_handle = node
        .connect(&dest_hash, &signing_key)
        .await
        .expect("Failed to connect");
    let link_id = *link_handle.link_id();

    // Wait for link to be established
    assert!(
        wait_for_link_established(&mut events, &link_id, Duration::from_secs(10)).await,
        "Link should be established over UDP"
    );

    // Verify negotiated MTU on Rust side, clamped to base MTU by Python
    let mtu = node.link_negotiated_mtu(&link_id);
    assert_eq!(
        mtu,
        Some(DAEMON_UDP_NEGOTIATED_MTU),
        "Rust should negotiate daemon UDP MTU (base protocol MTU)"
    );

    let mdu = node.link_mdu(&link_id);
    assert_eq!(
        mdu,
        Some(DAEMON_UDP_LINK_MDU),
        "Rust MDU should be {}",
        DAEMON_UDP_LINK_MDU
    );

    // Wait for daemon to finalize link table
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify Python-side MTU
    let link_hash = hex::encode(link_id.as_bytes());
    let status = daemon
        .get_link_status(&link_hash)
        .await
        .expect("Should get link status");

    assert_eq!(status.status, "found");
    if let Some(py_mtu) = status.mtu {
        assert_eq!(
            py_mtu, DAEMON_UDP_NEGOTIATED_MTU,
            "Python should report base protocol MTU for UDP"
        );
    }

    // Send max channel payload and verify receipt
    let payload = generate_test_payload(DAEMON_UDP_MAX_CHANNEL_PAYLOAD);
    link_handle.send(&payload).await.expect("Send failed");

    tokio::time::sleep(Duration::from_secs(1)).await;
    let packets = daemon.get_received_packets().await.unwrap_or_default();
    assert!(
        !packets.is_empty(),
        "Python should receive data over UDP link"
    );
    assert!(
        verify_test_payload(&packets[0].data),
        "Payload integrity check failed"
    );

    println!(
        "SUCCESS: Rust-to-Python UDP MTU={}, MDU={}, payload {} bytes verified",
        DAEMON_UDP_NEGOTIATED_MTU, DAEMON_UDP_LINK_MDU, DAEMON_UDP_MAX_CHANNEL_PAYLOAD
    );

    node.stop().await.ok();
}

/// Python-to-Rust over UDP: verify responder side MTU=500 (base protocol MTU).
///
/// Python daemon (initiator, UDP) → Rust (responder, UDP)
///
/// Python's `Transport.next_hop_interface_hw_mtu()` returns None for UDPInterface
/// (AUTOCONFIGURE_MTU=False, FIXED_MTU=False), so Python initiates with MTU=500.
/// Rust responder reads this from the link request and accepts with MTU=500.
///
/// `create_link` blocks until the link is fully established, so we must
/// spawn it as a background task while the main thread handles the
/// handshake (receive link request, accept, wait for established).
#[tokio::test]
async fn test_mtu_b3_python_to_rust_udp() {
    use crate::common::{
        wait_for_responder_established_link, DAEMON_UDP_LINK_MDU, DAEMON_UDP_MAX_CHANNEL_PAYLOAD,
        DAEMON_UDP_NEGOTIATED_MTU,
    };
    use leviculum_core::identity::Identity;
    use leviculum_std::driver::ReticulumNodeBuilder;

    let daemon = TestDaemon::start_with_udp()
        .await
        .expect("Failed to start daemon with UDP");

    let py_listen = daemon
        .udp_listen_addr()
        .expect("daemon should have UDP listen addr");
    let rust_listen = daemon
        .udp_forward_addr()
        .expect("daemon should have UDP forward addr");

    // Create identity for Rust (responder) before building node
    let identity = Identity::generate(&mut OsRng);
    let public_key_hex = hex::encode(identity.public_key_bytes());

    let mut dest = leviculum_core::Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mtu_b3",
        &["test"],
    )
    .expect("Failed to create destination");
    let dest_hash = *dest.hash();
    dest.set_accepts_links(true);

    let _storage = crate::common::temp_storage("test_mtu_b3_python_to_rust_udp", "node");
    let mut node = ReticulumNodeBuilder::new()
        .add_udp_interface(rust_listen, py_listen)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    node.start().await.expect("Failed to start node");
    node.register_destination(dest);

    let mut events = node
        .take_event_receiver()
        .expect("Failed to get event receiver");

    // Wait for UDP interface to settle
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Rust announces so Python learns the path
    node.announce_destination(&dest_hash, Some(b"rust-udp-responder"))
        .await
        .expect("Announce failed");

    // Wait for Python daemon to learn the path
    let dest_hash_hex = hex::encode(dest_hash.as_bytes());
    assert!(
        crate::common::wait_for_node_reannounce_on_daemon(
            &daemon,
            &dest_hash,
            &node,
            b"rust-udp-responder",
            Duration::from_secs(5),
        )
        .await,
        "Python should learn path to Rust destination"
    );

    // Spawn create_link in background, it blocks until link is ACTIVE
    let daemon_create_link = {
        let dest_hash_hex = dest_hash_hex.clone();
        let public_key_hex = public_key_hex.clone();
        let daemon_cmd_addr = daemon.cmd_addr();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            use tokio::net::TcpStream;

            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut cmd_stream = TcpStream::connect(daemon_cmd_addr)
                .await
                .expect("Failed to connect to daemon cmd");
            let cmd = serde_json::json!({
                "method": "create_link",
                "params": {
                    "dest_hash": dest_hash_hex,
                    "dest_key": public_key_hex,
                    "timeout": 15
                }
            });
            cmd_stream
                .write_all(cmd.to_string().as_bytes())
                .await
                .expect("write failed");
            cmd_stream.shutdown().await.expect("shutdown failed");
            let mut response = Vec::new();
            cmd_stream
                .read_to_end(&mut response)
                .await
                .expect("read failed");
            serde_json::from_slice::<serde_json::Value>(&response).expect("parse response failed")
        })
    };

    // The incoming link from Python is auto-accepted and proved; wait for it to
    // establish on the Rust side.
    let link_id = wait_for_responder_established_link(&mut events, Duration::from_secs(10))
        .await
        .expect("Rust should establish incoming link from Python");

    // Wait for the background create_link to complete
    let link_result = daemon_create_link.await.expect("create_link task panicked");
    if let Some(error) = link_result.get("error") {
        panic!("Python create_link failed: {}", error);
    }

    // Verify Rust-side MTU, base protocol MTU from Python's signaling
    let mtu = node.link_negotiated_mtu(&link_id);
    assert_eq!(
        mtu,
        Some(DAEMON_UDP_NEGOTIATED_MTU),
        "Rust responder should negotiate daemon UDP MTU (base protocol MTU)"
    );

    let mdu = node.link_mdu(&link_id);
    assert_eq!(
        mdu,
        Some(DAEMON_UDP_LINK_MDU),
        "Rust MDU should be {}",
        DAEMON_UDP_LINK_MDU
    );

    // Verify Python-side MTU
    tokio::time::sleep(Duration::from_millis(500)).await;
    let link_hash = hex::encode(link_id.as_bytes());
    let status = daemon
        .get_link_status(&link_hash)
        .await
        .expect("Should get link status");

    if let Some(py_mtu) = status.mtu {
        assert_eq!(
            py_mtu, DAEMON_UDP_NEGOTIATED_MTU,
            "Python should report base protocol MTU for UDP"
        );
    }

    // Python sends data to Rust over the established link
    let link_hash_str = link_result["result"]["link_hash"]
        .as_str()
        .expect("create_link should return link_hash");
    let payload = generate_test_payload(DAEMON_UDP_MAX_CHANNEL_PAYLOAD);
    daemon
        .send_on_link(link_hash_str, &payload)
        .await
        .expect("Python send_on_link failed");

    let received = wait_for_data_event(&mut events, &link_id, Duration::from_secs(10))
        .await
        .expect("Rust should receive data from Python over UDP");
    assert_eq!(received.len(), DAEMON_UDP_MAX_CHANNEL_PAYLOAD);
    assert!(
        verify_test_payload(&received),
        "Python-to-Rust payload integrity check failed"
    );

    println!(
        "SUCCESS: Python-to-Rust UDP MTU={}, MDU={}, payload {} bytes, Python mtu={:?}",
        DAEMON_UDP_NEGOTIATED_MTU, DAEMON_UDP_LINK_MDU, DAEMON_UDP_MAX_CHANNEL_PAYLOAD, status.mtu
    );

    node.stop().await.ok();
}

// =========================================================================
// Group C: Boundary tests
// =========================================================================

/// Exact MDU boundary: send exactly DAEMON_TCP_MAX_CHANNEL_PAYLOAD bytes.
///
/// (Covered by A1, included as explicit boundary test)
#[tokio::test]
async fn test_mtu_c1_exact_mdu() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream_b = connect_to_daemon(&daemon).await;
    let mut deframer_b = Deframer::new();
    let mut node_b = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_b.set_interface_hw_mtu(0, TCP_HW_MTU);

    let mut stream_a = connect_to_daemon(&daemon).await;
    let mut deframer_a = Deframer::new();

    let (destination_a, _) =
        setup_rust_destination(&mut stream_a, "mtu_c1", &["test"], b"rust-A").await;
    let dest_hash_a = *destination_a.hash();

    let mut node_a = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_a.set_interface_hw_mtu(0, TCP_HW_MTU);
    let mut dest_a = destination_a;
    dest_a.set_accepts_links(true);
    node_a.register_destination(dest_a);

    tokio::time::sleep(Duration::from_millis(500)).await;

    let (link_id_a, link_id_b) = establish_rust_to_rust_link(
        &mut node_a,
        &mut node_b,
        &mut stream_a,
        &mut stream_b,
        &mut deframer_a,
        &mut deframer_b,
        dest_hash_a,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send exactly max channel payload (should succeed)
    let payload = generate_test_payload(DAEMON_TCP_MAX_CHANNEL_PAYLOAD);
    let result = node_b.send_on_link(&link_id_b, &payload);
    assert!(result.is_ok(), "Exact MDU payload should succeed");

    let output = result.unwrap();
    dispatch_actions(&mut stream_b, &output).await;

    let data_raw = wait_for_link_data_packet(
        &mut stream_a,
        &mut deframer_a,
        &link_id_a,
        Duration::from_secs(30),
    )
    .await
    .expect("Should receive exact MDU payload");

    let output = node_a.handle_packet(InterfaceId(0), &data_raw);
    let received = output.events.iter().find_map(|e| match e {
        NodeEvent::LinkDataReceived { data, .. } | NodeEvent::MessageReceived { data, .. } => {
            Some(data.clone())
        }
        _ => None,
    });

    assert_eq!(
        received.as_ref().map(|d| d.len()),
        Some(DAEMON_TCP_MAX_CHANNEL_PAYLOAD),
        "Should receive exactly {} bytes",
        DAEMON_TCP_MAX_CHANNEL_PAYLOAD
    );

    println!(
        "SUCCESS: Exact MDU ({} bytes) transfer verified",
        DAEMON_TCP_MAX_CHANNEL_PAYLOAD
    );
}

/// Minimum payload: send 1 byte.
#[tokio::test]
async fn test_mtu_c2_one_byte() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream_b = connect_to_daemon(&daemon).await;
    let mut deframer_b = Deframer::new();
    let mut node_b = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_b.set_interface_hw_mtu(0, TCP_HW_MTU);

    let mut stream_a = connect_to_daemon(&daemon).await;
    let mut deframer_a = Deframer::new();

    let (destination_a, _) =
        setup_rust_destination(&mut stream_a, "mtu_c2", &["test"], b"rust-A").await;
    let dest_hash_a = *destination_a.hash();

    let mut node_a = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_a.set_interface_hw_mtu(0, TCP_HW_MTU);
    let mut dest_a = destination_a;
    dest_a.set_accepts_links(true);
    node_a.register_destination(dest_a);

    tokio::time::sleep(Duration::from_millis(500)).await;

    let (link_id_a, link_id_b) = establish_rust_to_rust_link(
        &mut node_a,
        &mut node_b,
        &mut stream_a,
        &mut stream_b,
        &mut deframer_a,
        &mut deframer_b,
        dest_hash_a,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send 1 byte
    let output = node_b
        .send_on_link(&link_id_b, &[0x42])
        .expect("1-byte send should succeed");
    dispatch_actions(&mut stream_b, &output).await;

    let data_raw = wait_for_link_data_packet(
        &mut stream_a,
        &mut deframer_a,
        &link_id_a,
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive 1-byte payload");

    let output = node_a.handle_packet(InterfaceId(0), &data_raw);
    let received = output.events.iter().find_map(|e| match e {
        NodeEvent::LinkDataReceived { data, .. } | NodeEvent::MessageReceived { data, .. } => {
            Some(data.clone())
        }
        _ => None,
    });

    assert_eq!(received.as_deref(), Some(&[0x42][..]));

    println!("SUCCESS: 1-byte transfer verified");
}

/// Over MDU: send MDU+1 bytes, expect error.
#[tokio::test]
async fn test_mtu_c3_over_mdu() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream_b = connect_to_daemon(&daemon).await;
    let mut deframer_b = Deframer::new();
    let mut node_b = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_b.set_interface_hw_mtu(0, TCP_HW_MTU);

    let mut stream_a = connect_to_daemon(&daemon).await;
    let mut deframer_a = Deframer::new();

    let (destination_a, _) =
        setup_rust_destination(&mut stream_a, "mtu_c3", &["test"], b"rust-A").await;
    let dest_hash_a = *destination_a.hash();

    let mut node_a = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    node_a.set_interface_hw_mtu(0, TCP_HW_MTU);
    let mut dest_a = destination_a;
    dest_a.set_accepts_links(true);
    node_a.register_destination(dest_a);

    tokio::time::sleep(Duration::from_millis(500)).await;

    let (_link_id_a, link_id_b) = establish_rust_to_rust_link(
        &mut node_a,
        &mut node_b,
        &mut stream_a,
        &mut stream_b,
        &mut deframer_a,
        &mut deframer_b,
        dest_hash_a,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send MDU+1 bytes, should fail
    let oversized = generate_test_payload(DAEMON_TCP_MAX_CHANNEL_PAYLOAD + 1);
    let result = node_b.send_on_link(&link_id_b, &oversized);
    assert!(result.is_err(), "Sending MDU+1 bytes should fail, got Ok");

    println!(
        "SUCCESS: MDU+1 ({} bytes) correctly rejected",
        DAEMON_TCP_MAX_CHANNEL_PAYLOAD + 1
    );
}

// =========================================================================
// Group D: Multi-hop MTU clamping
// =========================================================================

/// TCP -> RustRelay -> UDP: verify MTU clamps to UDP bottleneck (1064).
///
/// Topology:
///   RustA (TCP client, initiator) <-TCP-> RustRelay (TCP server + UDP, transport=true) <-UDP-> RustB (UDP, responder)
///
/// RustA's TCP interface has HW_MTU=16384. The relay's `clamp_link_request_mtu()`
/// detects the outgoing UDP interface with HW_MTU=1064 and clamps the signaling
/// bytes. Both sides negotiate MTU=1064.
#[tokio::test]
async fn test_mtu_d1_tcp_relay_udp_clamp() {
    use leviculum_core::identity::Identity;
    use leviculum_std::driver::ReticulumNodeBuilder;

    // Allocate ports
    let tcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp_port = tcp_listener.local_addr().unwrap().port();
    drop(tcp_listener);

    let udp_sock1 = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let relay_udp_port = udp_sock1.local_addr().unwrap().port();
    drop(udp_sock1);

    let udp_sock2 = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let b_udp_port = udp_sock2.local_addr().unwrap().port();
    drop(udp_sock2);

    let tcp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();
    let relay_udp_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", relay_udp_port).parse().unwrap();
    let b_udp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", b_udp_port).parse().unwrap();

    // Create identity for B (responder) before building node
    let identity_b = Identity::generate(&mut OsRng);
    let pub_key_bytes = identity_b.public_key_bytes();
    let signing_key_b: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    let mut dest_b = leviculum_core::Destination::new(
        Some(identity_b),
        Direction::In,
        DestinationType::Single,
        "mtu_d1",
        &["test"],
    )
    .unwrap();
    let dest_hash_b = *dest_b.hash();
    dest_b.set_accepts_links(true);

    // RustRelay: TCP server + UDP, transport enabled
    let _storage1 = crate::common::temp_storage("test_mtu_d1_tcp_relay_udp_clamp", "node1");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_server(tcp_addr)
        .add_udp_interface(relay_udp_addr, b_udp_addr)
        .storage_path(_storage1.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build relay");
    relay.start().await.expect("Failed to start relay");

    // Small delay to let TCP server bind
    tokio::time::sleep(Duration::from_millis(200)).await;

    // RustA: TCP client (initiator)
    let _storage2 = crate::common::temp_storage("test_mtu_d1_tcp_relay_udp_clamp", "node2");
    let mut node_a = ReticulumNodeBuilder::new()
        .add_tcp_client(tcp_addr)
        .storage_path(_storage2.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node A");
    node_a.start().await.expect("Failed to start node A");

    // RustB: UDP (responder)
    let _storage3 = crate::common::temp_storage("test_mtu_d1_tcp_relay_udp_clamp", "node3");
    let mut node_b = ReticulumNodeBuilder::new()
        .add_udp_interface(b_udp_addr, relay_udp_addr)
        .storage_path(_storage3.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node B");
    node_b.start().await.expect("Failed to start node B");
    node_b.register_destination(dest_b);

    let mut events_a = node_a.take_event_receiver().unwrap();
    let mut events_b = node_b.take_event_receiver().unwrap();

    // Wait for connections to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // B announces on UDP -> relay rebroadcasts on TCP -> A learns path
    node_b
        .announce_destination(&dest_hash_b, Some(b"responder"))
        .await
        .expect("B announce failed");

    // Wait for A to learn path (through relay)
    assert!(
        crate::common::wait_for_node_reannounce(
            || node_a.has_path(&dest_hash_b),
            &node_b,
            &dest_hash_b,
            b"responder",
            Duration::from_secs(10),
        )
        .await,
        "A should learn path to B via relay"
    );

    // A connects to B via relay
    let link_handle = node_a
        .connect(&dest_hash_b, &signing_key_b)
        .await
        .expect("A connect failed");
    let link_id_a = link_handle.link_id();

    // B's incoming link is auto-accepted and proved; wait for it to establish.
    let link_id_b = wait_for_responder_established_link(&mut events_b, Duration::from_secs(10))
        .await
        .expect("B should establish incoming link");

    // Wait for A side to be established (B already established above).
    assert!(
        wait_for_link_established(&mut events_a, link_id_a, Duration::from_secs(10)).await,
        "A link should be established"
    );

    // Verify negotiated MTU, clamped to UDP bottleneck
    let mtu_a = node_a.link_negotiated_mtu(link_id_a);
    let mtu_b = node_b.link_negotiated_mtu(&link_id_b);
    assert_eq!(
        mtu_a,
        Some(UDP_HW_MTU),
        "A should negotiate UDP HW_MTU (clamped by relay)"
    );
    assert_eq!(mtu_b, Some(UDP_HW_MTU), "B should negotiate UDP HW_MTU");

    let mdu_a = node_a.link_mdu(link_id_a);
    let mdu_b = node_b.link_mdu(&link_id_b);
    assert_eq!(
        mdu_a,
        Some(UDP_LINK_MDU),
        "A MDU should be {}",
        UDP_LINK_MDU
    );
    assert_eq!(
        mdu_b,
        Some(UDP_LINK_MDU),
        "B MDU should be {}",
        UDP_LINK_MDU
    );

    // Send max UDP channel payload from A to B
    let payload = generate_test_payload(UDP_MAX_CHANNEL_PAYLOAD);
    link_handle.send(&payload).await.expect("A send failed");

    let received = wait_for_data_event(&mut events_b, &link_id_b, Duration::from_secs(10))
        .await
        .expect("B should receive data from A");

    assert_eq!(received.len(), UDP_MAX_CHANNEL_PAYLOAD);
    assert!(
        verify_test_payload(&received),
        "TCP->Relay->UDP payload integrity failed"
    );

    println!(
        "SUCCESS: TCP->Relay->UDP MTU={}, MDU={}, payload {} bytes verified",
        UDP_HW_MTU, UDP_LINK_MDU, UDP_MAX_CHANNEL_PAYLOAD
    );

    node_a.stop().await.ok();
    node_b.stop().await.ok();
    relay.stop().await.ok();
}

/// UDP -> RustRelay -> TCP: verify MTU stays at UDP bottleneck (1064).
///
/// Topology (reverse of the TCP->Relay->UDP case):
///   RustA (UDP, initiator) <-UDP-> RustRelay (TCP server + UDP, transport=true) <-TCP-> RustB (TCP client, responder)
///
/// RustA's UDP interface has HW_MTU=1064. The link request signaling bytes
/// already encode 1064, so the relay doesn't need to clamp, but the packet
/// still traverses UDP->Relay->TCP, proving the full relay path works.
#[tokio::test]
async fn test_mtu_d2_udp_relay_tcp() {
    use leviculum_core::identity::Identity;
    use leviculum_std::driver::ReticulumNodeBuilder;

    // Allocate ports
    let tcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp_port = tcp_listener.local_addr().unwrap().port();
    drop(tcp_listener);

    let udp_sock1 = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let relay_udp_port = udp_sock1.local_addr().unwrap().port();
    drop(udp_sock1);

    let udp_sock2 = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let a_udp_port = udp_sock2.local_addr().unwrap().port();
    drop(udp_sock2);

    let tcp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", tcp_port).parse().unwrap();
    let relay_udp_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", relay_udp_port).parse().unwrap();
    let a_udp_addr: std::net::SocketAddr = format!("127.0.0.1:{}", a_udp_port).parse().unwrap();

    // Create identity for B (responder, TCP side) before building node
    let identity_b = Identity::generate(&mut OsRng);
    let pub_key_bytes = identity_b.public_key_bytes();
    let signing_key_b: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    let mut dest_b = leviculum_core::Destination::new(
        Some(identity_b),
        Direction::In,
        DestinationType::Single,
        "mtu_d2",
        &["test"],
    )
    .unwrap();
    let dest_hash_b = *dest_b.hash();
    dest_b.set_accepts_links(true);

    // RustRelay: TCP server + UDP, transport enabled
    let _storage1 = crate::common::temp_storage("test_mtu_d2_udp_relay_tcp", "node1");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_server(tcp_addr)
        .add_udp_interface(relay_udp_addr, a_udp_addr)
        .storage_path(_storage1.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build relay");
    relay.start().await.expect("Failed to start relay");

    // Small delay to let TCP server bind
    tokio::time::sleep(Duration::from_millis(200)).await;

    // RustB: TCP client (responder)
    let _storage2 = crate::common::temp_storage("test_mtu_d2_udp_relay_tcp", "node2");
    let mut node_b = ReticulumNodeBuilder::new()
        .add_tcp_client(tcp_addr)
        .storage_path(_storage2.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node B");
    node_b.start().await.expect("Failed to start node B");
    node_b.register_destination(dest_b);

    // RustA: UDP (initiator)
    let _storage3 = crate::common::temp_storage("test_mtu_d2_udp_relay_tcp", "node3");
    let mut node_a = ReticulumNodeBuilder::new()
        .add_udp_interface(a_udp_addr, relay_udp_addr)
        .storage_path(_storage3.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node A");
    node_a.start().await.expect("Failed to start node A");

    let mut events_a = node_a.take_event_receiver().unwrap();
    let mut events_b = node_b.take_event_receiver().unwrap();

    // Wait for connections to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // B announces on TCP -> relay rebroadcasts on UDP -> A learns path
    node_b
        .announce_destination(&dest_hash_b, Some(b"responder"))
        .await
        .expect("B announce failed");

    // Wait for A to learn path (through relay)
    assert!(
        crate::common::wait_for_node_reannounce(
            || node_a.has_path(&dest_hash_b),
            &node_b,
            &dest_hash_b,
            b"responder",
            Duration::from_secs(10),
        )
        .await,
        "A should learn path to B via relay"
    );

    // A connects to B via relay
    let link_handle = node_a
        .connect(&dest_hash_b, &signing_key_b)
        .await
        .expect("A connect failed");
    let link_id_a = link_handle.link_id();

    // B's incoming link is auto-accepted and proved; wait for it to establish.
    let link_id_b = wait_for_responder_established_link(&mut events_b, Duration::from_secs(10))
        .await
        .expect("B should establish incoming link");

    // Wait for A side to be established (B already established above).
    assert!(
        wait_for_link_established(&mut events_a, link_id_a, Duration::from_secs(10)).await,
        "A link should be established"
    );

    // Verify negotiated MTU, constrained by A's UDP interface
    let mtu_a = node_a.link_negotiated_mtu(link_id_a);
    let mtu_b = node_b.link_negotiated_mtu(&link_id_b);
    assert_eq!(mtu_a, Some(UDP_HW_MTU), "A should negotiate UDP HW_MTU");
    assert_eq!(mtu_b, Some(UDP_HW_MTU), "B should negotiate UDP HW_MTU");

    let mdu_a = node_a.link_mdu(link_id_a);
    let mdu_b = node_b.link_mdu(&link_id_b);
    assert_eq!(
        mdu_a,
        Some(UDP_LINK_MDU),
        "A MDU should be {}",
        UDP_LINK_MDU
    );
    assert_eq!(
        mdu_b,
        Some(UDP_LINK_MDU),
        "B MDU should be {}",
        UDP_LINK_MDU
    );

    // Send max UDP channel payload from A to B
    let payload = generate_test_payload(UDP_MAX_CHANNEL_PAYLOAD);
    link_handle.send(&payload).await.expect("A send failed");

    let received = wait_for_data_event(&mut events_b, &link_id_b, Duration::from_secs(10))
        .await
        .expect("B should receive data from A");

    assert_eq!(received.len(), UDP_MAX_CHANNEL_PAYLOAD);
    assert!(
        verify_test_payload(&received),
        "UDP->Relay->TCP payload integrity failed"
    );

    println!(
        "SUCCESS: UDP->Relay->TCP MTU={}, MDU={}, payload {} bytes verified",
        UDP_HW_MTU, UDP_LINK_MDU, UDP_MAX_CHANNEL_PAYLOAD
    );

    node_a.stop().await.ok();
    node_b.stop().await.ok();
    relay.stop().await.ok();
}
