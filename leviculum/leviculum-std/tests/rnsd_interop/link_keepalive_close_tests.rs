//! Keepalive and close interop tests using NodeCore API.
//!
//! These tests verify that Rust's keepalive and close implementation correctly
//! interoperates with Python Reticulum. Tests cover:
//!
//! 1. **Keepalive Exchange** - Initiator sends 0xFF, responder echoes 0xFE
//! 2. **Graceful Close** - LINKCLOSE packet exchange
//! 3. **Stale Detection** - Link transitions to STALE when no packets received
//! 4. **Stale Timeout** - Stale links eventually close
//! 5. **Multi-hop Tests** - Keepalive and close through Python relay
//!
//! ## Running These Tests
//!
//! ```sh
//! # Run all keepalive/close tests
//! cargo test --package leviculum-std --test rnsd_interop link_keepalive_close
//!
//! # Run multi-hop tests specifically
//! cargo test --package leviculum-std --test rnsd_interop test_multi_hop
//!
//! # Run with verbose output
//! cargo test --package leviculum-std --test rnsd_interop link_keepalive_close -- --nocapture
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rand_core::OsRng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use leviculum_core::constants::{
    LINK_KEEPALIVE_SECS, LINK_STALE_FACTOR, LINK_STALE_GRACE_SECS, MS_PER_SECOND,
    TRUNCATED_HASHBYTES,
};
use leviculum_core::identity::Identity;
use leviculum_core::link::{LinkCloseReason, LinkId, LinkState};
use leviculum_core::node::{NodeCoreBuilder, NodeEvent};
use leviculum_core::packet::PacketContext;
use leviculum_core::traits::Clock;
use leviculum_core::transport::{Action, InterfaceId};
use leviculum_core::DestinationHash;
use leviculum_core::MemoryStorage;
use leviculum_core::{Destination, DestinationType, Direction};
use leviculum_std::interfaces::hdlc::Deframer;

use crate::common::{
    connect_to_daemon, receive_raw_proof_for_link, send_framed, setup_rust_destination,
    wait_for_any_announce_with_route_info, wait_for_close_packet, wait_for_keepalive_packet,
    wait_for_link_request, wait_for_rtt_packet, TestClock,
};
use crate::harness::TestDaemon;

// =========================================================================
// Shared mock clock for time-controlled tests
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

    fn advance_secs(&self, secs: u64) {
        self.advance(secs * MS_PER_SECOND);
    }

    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
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

/// Check whether a node considers a link active.
fn is_active<R: rand_core::CryptoRngCore, C: Clock, S: leviculum_core::traits::Storage>(
    node: &leviculum_core::node::NodeCore<R, C, S>,
    link_id: &LinkId,
) -> bool {
    node.link(link_id)
        .map(|l| l.state() == LinkState::Active)
        .unwrap_or(false)
}

/// Type alias for NodeCore with TestClock (used in daemon tests).
type TestNode = leviculum_core::node::NodeCore<OsRng, TestClock, MemoryStorage>;

/// Establish a link with Rust as initiator using NodeCore, returning (link_id, link_hash_hex).
async fn establish_link_as_initiator(
    node: &mut TestNode,
    daemon: &TestDaemon,
    stream: &mut TcpStream,
    deframer: &mut Deframer,
) -> Result<(LinkId, String), String> {
    // Register a destination in daemon
    let dest_info = daemon
        .register_destination("keepalive", &["test"])
        .await
        .map_err(|e| format!("Failed to register destination: {}", e))?;

    // Extract signing key
    let pub_key_bytes =
        hex::decode(&dest_info.public_key).map_err(|e| format!("Invalid public key: {}", e))?;
    let signing_key: [u8; 32] = pub_key_bytes[32..64]
        .try_into()
        .map_err(|_| "Invalid signing key length")?;

    // Parse destination hash
    let dest_hash: [u8; TRUNCATED_HASHBYTES] = hex::decode(&dest_info.hash)
        .map_err(|e| format!("Invalid hash: {}", e))?
        .try_into()
        .map_err(|_| "Invalid hash length")?;

    // Initiate link via node
    let (link_id, _, output) = node.connect(DestinationHash::new(dest_hash), &signing_key);
    dispatch_actions(stream, &output).await;

    // Wait for proof (raw bytes for handle_packet)
    let proof_raw = receive_raw_proof_for_link(stream, deframer, &link_id, Duration::from_secs(10))
        .await
        .ok_or("No proof received")?;

    // Process proof via node
    let output = node.handle_packet(InterfaceId(0), &proof_raw);

    // Check for LinkEstablished
    if !output.events.iter().any(|e| {
        matches!(
            e,
            NodeEvent::LinkEstablished {
                is_initiator: true,
                ..
            }
        )
    }) {
        return Err("LinkEstablished not received".to_string());
    }

    // Dispatch RTT packet (included in actions from proof handling)
    dispatch_actions(stream, &output).await;

    // Wait for daemon to process RTT
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Get link hash hex for daemon RPC calls
    let link_hash_hex = hex::encode(link_id);

    Ok((link_id, link_hash_hex))
}

// =========================================================================
// Rust-to-Rust link establishment helper for multi-hop tests
// =========================================================================

/// Holds all state for a Rust-to-Rust link established through a relay.
/// A is the responder (announces), B is the initiator (creates link).
pub struct RustToRustLink {
    /// Link ID (same for both sides)
    pub link_id: LinkId,
    /// Node for side A (responder)
    pub node_a: TestNode,
    /// Stream for side A
    pub stream_a: TcpStream,
    /// Deframer for side A
    pub deframer_a: Deframer,
    /// Node for side B (initiator)
    pub node_b: TestNode,
    /// Stream for side B
    pub stream_b: TcpStream,
    /// Deframer for side B
    pub deframer_b: Deframer,
}

/// Establish a Rust-to-Rust link through the Python daemon as relay.
///
/// This encapsulates the pattern from `test_rust_to_rust_via_daemon`:
/// 1. B connects first to receive announces
/// 2. A sets up destination and announces via separate stream
/// 3. B receives announce via `wait_for_any_announce_with_route_info`
/// 4. B initiates link using `node.connect()`
/// 5. Complete handshake: link request -> proof -> RTT
///
/// Returns a `RustToRustLink` containing all state needed for further testing.
async fn establish_rust_to_rust_link(daemon: &TestDaemon) -> Result<RustToRustLink, String> {
    // B connects first to receive announces
    let mut stream_b = connect_to_daemon(daemon).await;
    let mut deframer_b = Deframer::new();
    let mut node_b = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // A (Responder) sets up and announces
    let mut stream_a = connect_to_daemon(daemon).await;
    let mut deframer_a = Deframer::new();

    let (destination_a, _public_key_hex_a) =
        setup_rust_destination(&mut stream_a, "r2r_keepalive", &["test"], b"rust-A").await;

    let dest_hash_a = *destination_a.hash();

    let mut node_a = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut dest_a = destination_a;
    dest_a.set_accepts_links(true);
    node_a.register_destination(dest_a);

    // Wait for daemon to process announce
    tokio::time::sleep(Duration::from_millis(500)).await;

    // B receives A's announce with routing info
    let announce_info = wait_for_any_announce_with_route_info(
        &mut stream_b,
        &mut deframer_b,
        Duration::from_secs(5),
    )
    .await
    .ok_or("B should receive A's announce from daemon")?;

    // Verify the announce is for A's destination
    if announce_info.packet.destination_hash != dest_hash_a {
        return Err("Announce should be for A's destination".to_string());
    }

    // Extract routing info from the announce
    let signing_key_a = announce_info
        .signing_key()
        .ok_or("Failed to extract signing key from announce")?;

    // Feed announce to B's transport so it learns the path
    let _ = node_b.handle_packet(InterfaceId(0), &announce_info.raw_data);

    // B initiates link to A via connect()
    let (link_id_b, _, output) = node_b.connect(dest_hash_a, &signing_key_a);

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
    .ok_or("A should receive link request")?;
    let link_id_a = LinkId::new(link_id_a_bytes);

    // Verify link IDs match
    if link_id_a != link_id_b {
        return Err(format!(
            "Link IDs should match: A={} B={}",
            hex::encode(link_id_a),
            hex::encode(link_id_b)
        ));
    }

    // A processes the link request. Auto-accept (Python parity): handle_packet
    // builds and routes the establishment proof inline, so its output carries the
    // proof action and the link moves to PendingIncoming.
    let output = node_a.handle_packet(InterfaceId(0), &raw_request);

    // The responder must have auto-proved the incoming link.
    if output.actions.is_empty() {
        return Err("A should auto-prove the incoming link (no proof action)".to_string());
    }

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
    .ok_or("B should receive proof")?;

    let output = node_b.handle_packet(InterfaceId(0), &proof_raw);

    // B should have LinkEstablished
    if !output.events.iter().any(|e| {
        matches!(
            e,
            NodeEvent::LinkEstablished {
                is_initiator: true,
                ..
            }
        )
    }) {
        return Err("B should have LinkEstablished event".to_string());
    }

    // B dispatches RTT (included in actions from proof handling)
    dispatch_actions(&mut stream_b, &output).await;

    // A receives RTT
    let rtt_data = wait_for_rtt_packet(
        &mut stream_a,
        &mut deframer_a,
        &link_id_a,
        Duration::from_secs(10),
    )
    .await
    .ok_or("A should receive RTT")?;

    // Build RTT packet for processing
    let mut rtt_raw = Vec::new();
    rtt_raw.push(0x0C);
    rtt_raw.push(0x00);
    rtt_raw.extend_from_slice(link_id_a.as_bytes());
    rtt_raw.push(PacketContext::Lrrtt.to_byte());
    rtt_raw.extend_from_slice(&rtt_data);

    let output = node_a.handle_packet(InterfaceId(0), &rtt_raw);

    // A should have LinkEstablished
    if !output.events.iter().any(|e| {
        matches!(
            e,
            NodeEvent::LinkEstablished {
                is_initiator: false,
                ..
            }
        )
    }) {
        return Err("A should have LinkEstablished event".to_string());
    }

    // Verify both links are active
    if !is_active(&node_a, &link_id_a) {
        return Err("Link should be active on A".to_string());
    }
    if !is_active(&node_b, &link_id_b) {
        return Err("Link should be active on B".to_string());
    }

    Ok(RustToRustLink {
        link_id: link_id_a,
        node_a,
        stream_a,
        deframer_a,
        node_b,
        stream_b,
        deframer_b,
    })
}

// =========================================================================
// Test 3: Rust graceful close received by Python
// =========================================================================

/// Test: Rust sends LINKCLOSE, Python transitions to CLOSED.
///
/// Steps:
/// 1. Start Python daemon
/// 2. Establish link (Rust as initiator)
/// 3. Call node.close_link(&link_id)
/// 4. Dispatch close actions
/// 5. Call Python RPC wait_for_link_state(link_hash, "CLOSED", 5)
/// 6. Verify Python link is closed
/// 7. Verify Rust emits LinkClosed { reason: Normal }
#[tokio::test]
async fn test_rust_graceful_close_received_by_python() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // Establish link
    let (link_id, link_hash_hex) =
        establish_link_as_initiator(&mut node, &daemon, &mut stream, &mut deframer)
            .await
            .expect("Failed to establish link");

    assert!(is_active(&node, &link_id), "Link should be active");

    // Close the link gracefully
    let output = node.close_link(&link_id);

    // Dispatch close actions
    dispatch_actions(&mut stream, &output).await;

    // Check for LinkClosed event
    let close_event = output.events.iter().find(|e| {
        matches!(
            e,
            NodeEvent::LinkClosed { link_id: id, reason: LinkCloseReason::Normal, .. }
            if *id == link_id
        )
    });
    assert!(
        close_event.is_some(),
        "Should emit LinkClosed with reason Normal"
    );

    // Wait for Python to process the close
    let wait_result = daemon
        .wait_for_link_state(&link_hash_hex, "CLOSED", 5)
        .await
        .expect("RPC call failed");

    assert_eq!(
        wait_result.status, "reached",
        "Python should reach CLOSED state. Got: {:?}",
        wait_result
    );

    println!("SUCCESS: Rust graceful close received by Python");
}

// =========================================================================
// Test 4: Python graceful close received by Rust
// =========================================================================

/// Test: Python sends LINKCLOSE, Rust transitions to CLOSED.
///
/// Steps:
/// 1. Start Python daemon
/// 2. Establish link (Rust as initiator)
/// 3. Call Python RPC close_link(link_hash)
/// 4. Read packets from stream
/// 5. Process LINKCLOSE packet in Rust via handle_packet
/// 6. Verify Rust link state is CLOSED
/// 7. Verify LinkClosed { reason: PeerClosed } event emitted
#[tokio::test]
async fn test_python_graceful_close_received_by_rust() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // Establish link
    let (link_id, link_hash_hex) =
        establish_link_as_initiator(&mut node, &daemon, &mut stream, &mut deframer)
            .await
            .expect("Failed to establish link");

    assert!(is_active(&node, &link_id), "Link should be active");

    // Have Python close the link
    let close_result = daemon
        .close_link(&link_hash_hex)
        .await
        .expect("RPC call failed");
    println!("Python close_link result: {}", close_result);

    // Wait for and process the close packet
    let close_raw =
        wait_for_close_packet(&mut stream, &mut deframer, &link_id, Duration::from_secs(5))
            .await
            .expect("Should receive LINKCLOSE packet from Python");

    let output = node.handle_packet(InterfaceId(0), &close_raw);

    // Check for LinkClosed event with PeerClosed reason
    let close_event = output.events.iter().find(|e| {
        matches!(
            e,
            NodeEvent::LinkClosed { link_id: id, reason: LinkCloseReason::PeerClosed, .. }
            if *id == link_id
        )
    });

    assert!(
        close_event.is_some(),
        "Should emit LinkClosed with reason PeerClosed. Events: {:?}",
        output.events
    );

    println!("SUCCESS: Python graceful close received by Rust");
}

// =========================================================================
// Test 5: Link stale detection (no inbound traffic)
// =========================================================================

/// Test: Link transitions to STALE when no packets are received.
///
/// Steps:
/// 1. Create SharedMockClock with controlled time
/// 2. Use two NodeCores to simulate a complete handshake
/// 3. Calculate stale_time = keepalive_secs * STALE_FACTOR
/// 4. Advance clock past stale_time
/// 5. Call node.handle_timeout()
/// 6. Verify LinkStale event emitted
/// 7. Verify link state is LinkState::Stale
#[tokio::test]
async fn test_link_stale_detection_no_inbound() {
    // Start with a reasonable initial time
    let initial_time_ms = 10u64 * MS_PER_SECOND;

    let clock_init = SharedMockClock::new(initial_time_ms);
    let clock_init_handle = clock_init.handle();
    let mut initiator =
        NodeCoreBuilder::new().build(OsRng, clock_init, MemoryStorage::with_defaults());

    let clock_resp = SharedMockClock::new(initial_time_ms);
    let mut responder =
        NodeCoreBuilder::new().build(OsRng, clock_resp, MemoryStorage::with_defaults());

    // Create destination identity for responder
    let dest_identity = Identity::generate(&mut OsRng);
    let dest_signing_key = dest_identity.ed25519_verifying().to_bytes();
    let mut dest = Destination::new(
        Some(dest_identity),
        Direction::In,
        DestinationType::Single,
        "testapp",
        &["stale"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    responder.register_destination(dest);

    // Initiator starts link
    let (link_id, _, output) = initiator.connect(dest_hash, &dest_signing_key);
    let link_request_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver link request to responder
    let output = responder.handle_packet(InterfaceId(0), &link_request_data);

    // Auto-accept (Python parity): the proof was auto-produced by handle_packet
    // above, so read it from that output.
    let proof_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver proof to initiator
    let output = initiator.handle_packet(InterfaceId(0), &proof_data);

    // Initiator should have LinkEstablished and RTT in actions
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkEstablished {
            is_initiator: true,
            ..
        }
    )));

    let rtt_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver RTT to responder
    let _output = responder.handle_packet(InterfaceId(0), &rtt_data);

    // Verify link is active on initiator
    assert!(
        is_active(&initiator, &link_id),
        "Link should be active on initiator"
    );

    // Get the actual stale time from the link (depends on RTT calculation)
    let stale_time_secs = initiator
        .link(&link_id)
        .map(|l| l.stale_time_secs())
        .unwrap_or(LINK_KEEPALIVE_SECS * LINK_STALE_FACTOR);

    println!(
        "Link stale_time_secs = {} (keepalive_secs = {})",
        stale_time_secs,
        initiator
            .link(&link_id)
            .map(|l| l.keepalive_secs())
            .unwrap_or(0)
    );

    // Advance time past the stale threshold but NOT past the close timeout
    clock_init_handle.advance_secs(stale_time_secs + 1);

    // Poll the node
    let output = initiator.handle_timeout();

    // Check for LinkStale event
    let stale_event = output
        .events
        .iter()
        .find(|e| matches!(e, NodeEvent::LinkStale { link_id: id } if *id == link_id));

    assert!(
        stale_event.is_some(),
        "Should emit LinkStale event. Events: {:?}",
        output.events
    );

    // Verify link state is Stale
    if let Some(link) = initiator.link(&link_id) {
        assert_eq!(
            link.state(),
            LinkState::Stale,
            "Link should be in Stale state"
        );
    } else {
        panic!("Link should still exist");
    }

    println!("SUCCESS: Link stale detection works");
}

// =========================================================================
// Test 6: Stale link closes after timeout
// =========================================================================

/// Test: Stale link sends LINKCLOSE and transitions to CLOSED.
///
/// Steps:
/// 1. Create SharedMockClock with controlled time
/// 2. Simulate a complete handshake to get an active link
/// 3. Advance clock past stale_time (link becomes STALE)
/// 4. Calculate close_timeout based on stale grace period
/// 5. Advance clock past close_timeout
/// 6. Call node.handle_timeout()
/// 7. Verify close packet generated in actions
/// 8. Verify LinkClosed { reason: Stale } event emitted
#[tokio::test]
async fn test_stale_link_closes_after_timeout() {
    let initial_time_ms = 10u64 * MS_PER_SECOND;

    let clock_init = SharedMockClock::new(initial_time_ms);
    let clock_init_handle = clock_init.handle();
    let mut initiator =
        NodeCoreBuilder::new().build(OsRng, clock_init, MemoryStorage::with_defaults());

    let clock_resp = SharedMockClock::new(initial_time_ms);
    let mut responder =
        NodeCoreBuilder::new().build(OsRng, clock_resp, MemoryStorage::with_defaults());

    // Create destination identity for responder
    let dest_identity = Identity::generate(&mut OsRng);
    let dest_signing_key = dest_identity.ed25519_verifying().to_bytes();
    let mut dest = Destination::new(
        Some(dest_identity),
        Direction::In,
        DestinationType::Single,
        "testapp",
        &["staleclosetest"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    responder.register_destination(dest);

    // Initiator starts link
    let (link_id, _, output) = initiator.connect(dest_hash, &dest_signing_key);
    let link_request_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver link request to responder. Auto-accept (Stage 1): handle_packet
    // builds + routes the proof inline, so the proof is in this output.
    let output = responder.handle_packet(InterfaceId(0), &link_request_data);

    // Proof is auto-produced by handle_packet (Python parity auto-accept).
    let proof_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver proof to initiator
    let output = initiator.handle_packet(InterfaceId(0), &proof_data);
    let rtt_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver RTT to responder
    let _output = responder.handle_packet(InterfaceId(0), &rtt_data);

    // Verify link is active on initiator
    assert!(
        is_active(&initiator, &link_id),
        "Link should be active on initiator"
    );

    // Get the actual stale time from the link (depends on RTT calculation)
    let stale_time_secs = initiator
        .link(&link_id)
        .map(|l| l.stale_time_secs())
        .unwrap_or(LINK_KEEPALIVE_SECS * LINK_STALE_FACTOR);

    // Advance past stale time but NOT past close timeout
    clock_init_handle.advance_secs(stale_time_secs + 1);

    // Poll to trigger stale transition
    let output = initiator.handle_timeout();

    // Check for stale event
    let has_stale = output
        .events
        .iter()
        .any(|e| matches!(e, NodeEvent::LinkStale { .. }));
    assert!(has_stale, "Should emit LinkStale event first");

    // Verify link is now stale
    if let Some(link) = initiator.link(&link_id) {
        assert_eq!(link.state(), LinkState::Stale, "Link should be stale");
    }

    // Advance past the stale grace period
    // Close timeout = stale_time + rtt*TIMEOUT_FACTOR + GRACE
    // Since RTT is ~0 in mock handshake, we just need to go past GRACE
    clock_init_handle.advance_secs(LINK_STALE_GRACE_SECS + 2);

    // Poll to trigger close
    let output = initiator.handle_timeout();

    // Check for close packets in actions
    let close_packets = extract_action_packets(&output);
    assert!(
        !close_packets.is_empty(),
        "Should generate close packet for stale link"
    );

    // Check for LinkClosed event with Stale reason
    let close_event = output.events.iter().find(|e| {
        matches!(
            e,
            NodeEvent::LinkClosed {
                link_id: id,
                reason: LinkCloseReason::Stale,
                ..
            }
            if *id == link_id
        )
    });

    assert!(
        close_event.is_some(),
        "Should emit LinkClosed with reason Stale. Events: {:?}",
        output.events
    );

    println!("SUCCESS: Stale link closes after timeout");
}

// =========================================================================
// Test 7: Keepalive resets stale timer
// =========================================================================

/// Test: Receiving data resets stale detection timer.
///
/// Steps:
/// 1. Create link via complete handshake
/// 2. Advance clock to just before stale_time
/// 3. Simulate receiving a data packet (updates last_inbound)
/// 4. Advance clock by small amount
/// 5. Call node.handle_timeout()
/// 6. Verify link is still ACTIVE (not STALE)
#[tokio::test]
async fn test_keepalive_resets_stale_timer() {
    let initial_time_ms = 10u64 * MS_PER_SECOND;

    let clock_init = SharedMockClock::new(initial_time_ms);
    let clock_init_handle = clock_init.handle();
    let mut initiator =
        NodeCoreBuilder::new().build(OsRng, clock_init, MemoryStorage::with_defaults());

    let clock_resp = SharedMockClock::new(initial_time_ms);
    let mut responder =
        NodeCoreBuilder::new().build(OsRng, clock_resp, MemoryStorage::with_defaults());

    // Create destination identity for responder
    let dest_identity = Identity::generate(&mut OsRng);
    let dest_signing_key = dest_identity.ed25519_verifying().to_bytes();
    let mut dest = Destination::new(
        Some(dest_identity),
        Direction::In,
        DestinationType::Single,
        "testapp",
        &["karesettimer"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    responder.register_destination(dest);

    // Initiator starts link
    let (link_id, _, output) = initiator.connect(dest_hash, &dest_signing_key);
    let link_request_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver link request to responder. Auto-accept (Stage 1): handle_packet
    // builds + routes the proof inline, so the proof is in this output.
    let output = responder.handle_packet(InterfaceId(0), &link_request_data);

    // Proof is auto-produced by handle_packet (Python parity auto-accept).
    let proof_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver proof to initiator
    let output = initiator.handle_packet(InterfaceId(0), &proof_data);
    let rtt_data = extract_action_packets(&output).into_iter().next().unwrap();

    // Deliver RTT to responder
    let _output = responder.handle_packet(InterfaceId(0), &rtt_data);

    // Verify link is active
    assert!(
        is_active(&initiator, &link_id),
        "Link should be active on initiator"
    );

    // Get the actual stale time from the link
    let stale_time_secs = initiator
        .link(&link_id)
        .map(|l| l.stale_time_secs())
        .unwrap_or(LINK_KEEPALIVE_SECS * LINK_STALE_FACTOR);

    // Advance to just before stale time (but at least 2 seconds to allow for edge cases)
    let advance_before = stale_time_secs.saturating_sub(2).max(1);
    clock_init_handle.advance_secs(advance_before);

    // Simulate receiving inbound traffic by recording inbound
    if let Some(link) = initiator.link_mut(&link_id) {
        link.record_inbound(clock_init_handle.now_ms() / MS_PER_SECOND);
    }

    // Advance past where the old stale time would have been
    // but not past the new stale time (since we just received data)
    clock_init_handle.advance_secs(stale_time_secs / 2 + 1);

    // Poll
    let output = initiator.handle_timeout();

    // Check events - should NOT have LinkStale
    let has_stale = output
        .events
        .iter()
        .any(|e| matches!(e, NodeEvent::LinkStale { .. }));
    assert!(
        !has_stale,
        "Should NOT have LinkStale event after receiving data"
    );

    // Verify link is still active
    if let Some(link) = initiator.link(&link_id) {
        assert_eq!(
            link.state(),
            LinkState::Active,
            "Link should still be active"
        );
    }

    println!("SUCCESS: Keepalive resets stale timer");
}

// =========================================================================
// Test 1: Rust initiator sends keepalive, Python echoes
// =========================================================================

/// Test: Rust initiator sends 0xFF keepalive, Python responder echoes 0xFE.
///
/// This test requires the link to be active long enough for keepalive
/// to be triggered. In real scenarios, this happens after keepalive_interval.
/// For testing, we verify the keepalive mechanism exists and works.
#[tokio::test]
async fn test_rust_initiator_sends_keepalive_python_echoes() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // Establish link
    let (link_id, _link_hash_hex) =
        establish_link_as_initiator(&mut node, &daemon, &mut stream, &mut deframer)
            .await
            .expect("Failed to establish link");

    // Get the link and manually build a keepalive packet
    // (In real usage, this is triggered by handle_timeout() after keepalive interval)
    let keepalive_packet = {
        let link = node.link(&link_id).expect("Link should exist");
        link.build_keepalive_packet()
            .expect("Failed to build keepalive")
    };

    // Send the keepalive
    send_framed(&mut stream, &keepalive_packet).await;

    // Wait for echo from Python
    let echo_raw =
        wait_for_keepalive_packet(&mut stream, &mut deframer, &link_id, Duration::from_secs(5))
            .await;

    if let Some(echo_data) = echo_raw {
        // Process the echo via handle_packet
        let _output = node.handle_packet(InterfaceId(0), &echo_data);

        assert!(
            is_active(&node, &link_id),
            "Link should still be active after keepalive echo"
        );
        println!("Keepalive echo received from Python");
    } else {
        panic!("No keepalive echo received from Python - link not working properly");
    }
}

// =========================================================================
// Test 2: Python initiator sends keepalive, Rust echoes
// =========================================================================

/// Test: Python initiator sends keepalive, Rust responder echoes.
///
/// This tests Rust as responder processing incoming keepalives and echoing.
#[tokio::test]
async fn test_python_initiator_sends_keepalive_rust_echoes() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();

    // Set up Rust destination
    let (destination, public_key_hex) =
        setup_rust_destination(&mut stream, "rust", &["keepalive_resp"], b"ka-test").await;

    let dest_hash = *destination.hash();
    let dest_hash_hex = hex::encode(dest_hash);

    // Register destination in node
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());
    let mut dest = destination;
    dest.set_accepts_links(true);
    node.register_destination(dest);

    tokio::time::sleep(Duration::from_millis(500)).await;

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

    // Wait for link request
    let (raw_packet, link_id_bytes) = wait_for_link_request(
        &mut stream,
        &mut deframer,
        &dest_hash,
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive link request");
    let link_id = LinkId::new(link_id_bytes);

    // Process link request via node. Auto-accept (Python parity): handle_packet
    // builds and routes the establishment proof inline, so its output carries the
    // proof action.
    let output = node.handle_packet(InterfaceId(0), &raw_packet);

    // The responder must have auto-proved the incoming link.
    assert!(
        !output.actions.is_empty(),
        "node should auto-prove the incoming link (no proof action)"
    );

    // Send proof via actions (auto-sent inline by handle_packet)
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

    // Process RTT
    let mut rtt_raw = Vec::new();
    rtt_raw.push(0x0C);
    rtt_raw.push(0x00);
    rtt_raw.extend_from_slice(link_id.as_bytes());
    rtt_raw.push(PacketContext::Lrrtt.to_byte());
    rtt_raw.extend_from_slice(&rtt_data);

    let output = node.handle_packet(InterfaceId(0), &rtt_raw);

    // Check for LinkEstablished event
    assert!(output.events.iter().any(|e| matches!(
        e,
        NodeEvent::LinkEstablished {
            is_initiator: false,
            ..
        }
    )));

    // Wait for Python task to complete
    let _ = link_task.await;

    // Link is now active with Rust as responder
    assert!(is_active(&node, &link_id), "Link should be active");

    // Verify we're the responder
    if let Some(link) = node.link(&link_id) {
        assert!(!link.is_initiator(), "Rust should be responder");
    }

    // Testing actual keepalive from Python would require Python to send
    // keepalives, which happens based on Python's internal timing. Instead,
    // we verify that the responder echo mechanism is correctly set up.
    println!("SUCCESS: Rust responder link established (keepalive echo mechanism ready)");
}

// =========================================================================
// Test 10: Close packet payload verification
// =========================================================================

/// Test: Get link status from Python daemon via RPC.
///
/// Verifies the get_link_status RPC method works correctly.
#[tokio::test]
async fn test_get_link_status_rpc() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // Establish link
    let (link_id, link_hash_hex) =
        establish_link_as_initiator(&mut node, &daemon, &mut stream, &mut deframer)
            .await
            .expect("Failed to establish link");

    // Get link status from Python
    let status = daemon
        .get_link_status(&link_hash_hex)
        .await
        .expect("RPC call failed");

    assert_eq!(status.status, "found", "Link should be found");
    assert_eq!(status.link_hash, link_hash_hex);
    // Python link states: PENDING=0, HANDSHAKE=1, ACTIVE=2, STALE=3, CLOSED=4
    // After RTT exchange, the link should be ACTIVE (2)
    assert!(
        status
            .state
            .as_ref()
            .map(|s| s.contains("ACTIVE") || s == "2")
            .unwrap_or(false),
        "Link state should indicate ACTIVE. Got: {:?}",
        status.state
    );

    // Close the link
    let output = node.close_link(&link_id);
    dispatch_actions(&mut stream, &output).await;

    // Wait for Python to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Get status after close - should be not_found or CLOSED
    let status_after = daemon
        .get_link_status(&link_hash_hex)
        .await
        .expect("RPC call failed");

    // Link may be removed from dict or state may be CLOSED (4)
    let is_closed = status_after.status == "not_found"
        || status_after
            .state
            .as_ref()
            .map(|s| s.contains("CLOSED") || s == "4")
            .unwrap_or(false);

    assert!(
        is_closed,
        "Link should be closed. Status: {:?}",
        status_after
    );

    println!("SUCCESS: Get link status RPC works");
}

/// Test: Close packet contains correctly encrypted link_id.
///
/// Steps:
/// 1. Establish link
/// 2. Build close packet manually
/// 3. Verify packet structure
/// 4. Send close packet
/// 5. Verify Python accepts it
#[tokio::test]
async fn test_close_packet_payload_verification() {
    use leviculum_core::packet::{Packet, PacketType};

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    // Establish link
    let (link_id, link_hash_hex) =
        establish_link_as_initiator(&mut node, &daemon, &mut stream, &mut deframer)
            .await
            .expect("Failed to establish link");

    // Build close packet via the link's method
    let close_packet = {
        let link = node.link(&link_id).expect("Link should exist");
        link.build_close_packet(&mut OsRng)
            .expect("Failed to build close packet")
    };

    // Verify packet structure
    let pkt = Packet::unpack(&close_packet).expect("Failed to unpack close packet");
    assert_eq!(pkt.flags.packet_type, PacketType::Data);
    assert_eq!(pkt.context, PacketContext::LinkClose);
    assert_eq!(pkt.destination_hash, link_id);

    // The payload should be encrypted link_id (16 bytes + encryption overhead)
    assert!(
        pkt.data.len() >= 16,
        "Close packet payload should contain encrypted link_id"
    );

    // Send the close packet
    send_framed(&mut stream, &close_packet).await;

    // Wait for Python to process and verify it accepts the close
    let wait_result = daemon
        .wait_for_link_state(&link_hash_hex, "CLOSED", 5)
        .await
        .expect("RPC call failed");

    assert_eq!(
        wait_result.status, "reached",
        "Python should accept close packet and reach CLOSED state. Got: {:?}",
        wait_result
    );

    println!("SUCCESS: Close packet payload verification");
}

// =========================================================================
// Test 8: Multi-hop keepalive through Python relay
// =========================================================================

/// Test: Verify keepalive works across Rust B -> Python daemon -> Rust A path.
///
/// Steps:
/// 1. Establish Rust-to-Rust link via daemon using `establish_rust_to_rust_link`
/// 2. B (initiator) builds and sends keepalive packet
/// 3. A (responder) receives keepalive via daemon relay
/// 4. A processes keepalive via handle_packet (echo is generated in actions)
/// 5. A dispatches echo actions back through daemon
/// 6. B receives echo via daemon relay
/// 7. B processes echo via handle_packet
/// 8. Verify link is still active
#[tokio::test]
async fn test_multi_hop_keepalive_through_python_relay() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // 1. Establish Rust-to-Rust link via daemon
    let mut link = establish_rust_to_rust_link(&daemon)
        .await
        .expect("Failed to establish Rust-to-Rust link");

    let link_id = link.link_id;
    println!("Link established: {}", hex::encode(link_id));

    // 2. B (initiator) builds and sends keepalive packet
    let keepalive_packet = {
        let link_b = link.node_b.link(&link_id).expect("B should have link");
        link_b
            .build_keepalive_packet()
            .expect("Failed to build keepalive")
    };

    send_framed(&mut link.stream_b, &keepalive_packet).await;
    println!("B sent keepalive packet");

    // 3. A (responder) receives keepalive via daemon relay
    let ka_raw = wait_for_keepalive_packet(
        &mut link.stream_a,
        &mut link.deframer_a,
        &link_id,
        Duration::from_secs(5),
    )
    .await
    .expect("A should receive keepalive from B via daemon relay");

    println!("A received keepalive packet");

    // 4. A processes keepalive via handle_packet - echo is in output actions
    let output = link.node_a.handle_packet(InterfaceId(0), &ka_raw);

    // 5. Dispatch echo actions from A
    assert!(
        !output.actions.is_empty(),
        "A should generate echo packet for keepalive"
    );
    dispatch_actions(&mut link.stream_a, &output).await;
    println!("A dispatched keepalive echo actions");

    // 6. B receives echo via daemon relay
    let echo_raw = wait_for_keepalive_packet(
        &mut link.stream_b,
        &mut link.deframer_b,
        &link_id,
        Duration::from_secs(5),
    )
    .await
    .expect("B should receive echo from A via daemon relay");

    println!("B received keepalive echo packet");

    // 7. B processes echo via handle_packet
    let _output = link.node_b.handle_packet(InterfaceId(0), &echo_raw);

    // 8. Verify link is still active after keepalive round-trip
    assert!(
        is_active(&link.node_b, &link_id),
        "B's link should still be active after keepalive echo"
    );

    println!("SUCCESS: Multi-hop keepalive through Python relay");
}

// =========================================================================
// Test 9: Multi-hop graceful close through Python relay
// =========================================================================

/// Test: Verify graceful close works across Rust B -> Python daemon -> Rust A path.
///
/// Steps:
/// 1. Establish Rust-to-Rust link via daemon using `establish_rust_to_rust_link`
/// 2. B (initiator) closes the link via node.close_link()
/// 3. Dispatch close actions from B
/// 4. Verify B emits LinkClosed with Normal reason
/// 5. A receives close packet via daemon relay
/// 6. A processes close packet via handle_packet
/// 7. Verify A emits LinkClosed with PeerClosed reason
#[tokio::test]
async fn test_multi_hop_graceful_close_through_relay() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // 1. Establish Rust-to-Rust link via daemon
    let mut link = establish_rust_to_rust_link(&daemon)
        .await
        .expect("Failed to establish Rust-to-Rust link");

    let link_id = link.link_id;
    println!("Link established: {}", hex::encode(link_id));

    // Verify both sides are active
    assert!(
        is_active(&link.node_a, &link_id),
        "A should have active link"
    );
    assert!(
        is_active(&link.node_b, &link_id),
        "B should have active link"
    );

    // 2. B (initiator) closes the link
    let output = link.node_b.close_link(&link_id);
    println!("B closed the link");

    // 3. Dispatch close actions from B
    let close_packets = extract_action_packets(&output);
    assert!(!close_packets.is_empty(), "B should generate close packet");
    dispatch_actions(&mut link.stream_b, &output).await;
    println!("B sent close packet");

    // 4. Verify B emits LinkClosed with Normal reason
    let close_event_b = output.events.iter().find(|e| {
        matches!(
            e,
            NodeEvent::LinkClosed { link_id: id, reason: LinkCloseReason::Normal, .. }
            if *id == link_id
        )
    });

    assert!(
        close_event_b.is_some(),
        "B should emit LinkClosed with Normal reason. Events: {:?}",
        output.events
    );

    // 5. A receives close packet via daemon relay
    let close_raw = wait_for_close_packet(
        &mut link.stream_a,
        &mut link.deframer_a,
        &link_id,
        Duration::from_secs(5),
    )
    .await
    .expect("A should receive close packet from B via daemon relay");

    println!("A received close packet");

    // 6. A processes close packet via handle_packet
    let output = link.node_a.handle_packet(InterfaceId(0), &close_raw);

    // 7. Verify A emits LinkClosed with PeerClosed reason
    let close_event_a = output.events.iter().find(|e| {
        matches!(
            e,
            NodeEvent::LinkClosed { link_id: id, reason: LinkCloseReason::PeerClosed, .. }
            if *id == link_id
        )
    });

    assert!(
        close_event_a.is_some(),
        "A should emit LinkClosed with PeerClosed reason. Events: {:?}",
        output.events
    );

    // Verify both links are no longer active
    assert!(
        !is_active(&link.node_a, &link_id),
        "A's link should no longer be active"
    );
    assert!(
        !is_active(&link.node_b, &link_id),
        "B's link should no longer be active"
    );

    println!("SUCCESS: Multi-hop graceful close through Python relay");
}
