//! Raw link packet interop tests: `send_packet_on_link` against real Python.
//!
//! Python `RNS.Packet(link, data)` sends plain link data (context NONE)
//! without Channel framing. The receiving side proves it according to the
//! link destination's proof strategy (Link.py:1002-1011), and the sender's
//! PacketReceipt turns DELIVERED when the proof validates
//! (Packet.py:450-495). These tests pin both directions of that contract:
//!
//! - Rust `NodeCore::send_packet_on_link` → Python plain link-data path
//!   (`link.set_packet_callback`), Python PROVE_ALL proof →
//!   `NodeEvent::LinkDeliveryConfirmed` on our side.
//! - Python `RNS.Packet(link, data)` → our `NodeEvent::LinkDataReceived`,
//!   our auto-proof (ProofStrategy::All) → Python PacketReceipt DELIVERED.
//! - Negative: no proof (PROVE_NONE on either side) must end in
//!   `LinkDeliveryFailed` / Python receipt FAILED, never a false confirm.

use std::time::Duration;

use rand_core::OsRng;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use leviculum_core::identity::Identity;
use leviculum_core::link::LinkId;
use leviculum_core::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use leviculum_core::transport::{Action, InterfaceId};
use leviculum_core::MemoryStorage;
use leviculum_core::{Destination, DestinationType, Direction, ProofStrategy};
use leviculum_std::interfaces::hdlc::{DeframeResult, Deframer};

use crate::common::{
    connect_to_daemon, create_link_raw, extract_signing_key, parse_dest_hash,
    receive_raw_proof_for_link, send_framed, temp_storage, wait_for_data_event,
    wait_for_node_reannounce_on_daemon, wait_for_responder_established_link, TestClock,
};
use crate::harness::{DestinationInfo, HarnessError, TestDaemon};

type TestNode = NodeCore<OsRng, TestClock, MemoryStorage>;

/// Send all wire actions from a TickOutput over the TCP stream.
async fn dispatch_actions(stream: &mut TcpStream, output: &leviculum_core::transport::TickOutput) {
    for action in &output.actions {
        match action {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => {
                send_framed(stream, data).await;
            }
        }
    }
}

/// Establish an initiator link from a NodeCore to a fresh Python destination.
async fn establish_initiator_link(
    node: &mut TestNode,
    daemon: &TestDaemon,
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    aspect: &str,
) -> Result<(LinkId, DestinationInfo), HarnessError> {
    let dest_info = daemon.register_destination("rawlink", &[aspect]).await?;
    let signing_key = extract_signing_key(&dest_info.public_key);
    let dest_hash = parse_dest_hash(&dest_info.hash);

    let (link_id, _, output) = node.connect(dest_hash, &signing_key);
    dispatch_actions(stream, &output).await;

    let proof_raw = receive_raw_proof_for_link(stream, deframer, &link_id, Duration::from_secs(10))
        .await
        .ok_or_else(|| HarnessError::CommandFailed("No link proof received".to_string()))?;

    let output = node.handle_packet(InterfaceId(0), &proof_raw);
    let established = output
        .events
        .iter()
        .any(|e| matches!(e, NodeEvent::LinkEstablished { link_id: id, .. } if *id == link_id));
    if !established {
        return Err(HarnessError::CommandFailed(
            "LinkEstablished not emitted".to_string(),
        ));
    }
    // RTT packet finalizing the link on the Python side is in these actions.
    dispatch_actions(stream, &output).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    Ok((link_id, dest_info))
}

/// Pump the wire and timers until `pred` matches an event or the deadline
/// passes. Returns (matched, all events seen).
async fn pump_until<F>(
    node: &mut TestNode,
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    duration: Duration,
    mut pred: F,
) -> (bool, Vec<NodeEvent>)
where
    F: FnMut(&NodeEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + duration;
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];

    while tokio::time::Instant::now() < deadline {
        let mut outputs = Vec::new();

        match timeout(Duration::from_millis(100), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                for result in deframer.process(&buf[..n]) {
                    if let DeframeResult::Frame(data) = result {
                        outputs.push(node.handle_packet(InterfaceId(0), &data));
                    }
                }
            }
            _ => {}
        }
        // Timer sweep drives receipt deadlines (LinkDeliveryFailed).
        outputs.push(node.handle_timeout());

        let mut matched = false;
        for output in outputs {
            dispatch_actions(stream, &output).await;
            for event in output.events {
                if pred(&event) {
                    matched = true;
                }
                seen.push(event);
            }
        }
        if matched {
            return (true, seen);
        }
    }
    (false, seen)
}

/// Rust → Python raw link packet with PROVE_ALL.
///
/// Python's plain link-data path receives the exact payload
/// (Link.py:985-1008 fires the packet callback for DATA/context NONE) and
/// PROVE_ALL sends the proof back (Link.py:1002-1003), which our receipt
/// tracker verifies into `LinkDeliveryConfirmed`. Python's callback echo also
/// lands on our plain-data path as `LinkDataReceived`.
#[tokio::test]
async fn test_rust_raw_link_packet_to_python_prove_all() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    let (link_id, dest_info) =
        establish_initiator_link(&mut node, &daemon, &mut stream, &mut deframer, "proveall")
            .await
            .expect("link establishment");

    daemon
        .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
        .await
        .expect("set_proof_strategy");

    let payload = b"raw link packet interop";
    let (packet_hash, output) = node
        .send_packet_on_link(&link_id, payload)
        .expect("send_packet_on_link");
    dispatch_actions(&mut stream, &output).await;

    let (confirmed, events) = pump_until(
        &mut node,
        &mut stream,
        &mut deframer,
        Duration::from_secs(10),
        |e| {
            matches!(
                e,
                NodeEvent::LinkDeliveryConfirmed { link_id: id, packet_hash: hash }
                    if *id == link_id && *hash == packet_hash
            )
        },
    )
    .await;
    assert!(
        confirmed,
        "Python PROVE_ALL proof should confirm the raw link packet; events: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, NodeEvent::LinkDeliveryFailed { .. })),
        "no delivery failure may fire alongside the confirmation"
    );

    // Python's packet callback saw the exact payload on the plain-data path.
    let received = daemon
        .get_received_packets()
        .await
        .expect("get_received_packets");
    assert!(
        received.iter().any(|p| p.data == payload),
        "Python link packet callback should have received the payload"
    );

    // The daemon's callback echoes via RNS.Packet(link, data): that echo is
    // plain link data and must surface as LinkDataReceived, not
    // MessageReceived. It races the proof, so keep pumping if it has not
    // arrived alongside the confirmation yet.
    let echo_matcher = |e: &NodeEvent| {
        matches!(
            e,
            NodeEvent::LinkDataReceived { link_id: id, data }
                if *id == link_id && data == payload
        )
    };
    let echoed = if events.iter().any(echo_matcher) {
        true
    } else {
        pump_until(
            &mut node,
            &mut stream,
            &mut deframer,
            Duration::from_secs(5),
            echo_matcher,
        )
        .await
        .0
    };
    assert!(
        echoed,
        "Python's RNS.Packet echo should arrive on our plain link-data path"
    );
}

/// Rust → Python raw link packet with PROVE_NONE (negative).
///
/// Python's default PROVE_NONE sends no proof (Link.py:1002-1011 has no
/// else-branch), so the raw receipt must expire into `LinkDeliveryFailed`
/// after max(rtt*6, 5ms) — the same deadline Python computes for its own
/// PacketReceipt (Packet.py:431).
#[tokio::test]
async fn test_rust_raw_link_packet_prove_none_delivery_failed() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    let (link_id, dest_info) =
        establish_initiator_link(&mut node, &daemon, &mut stream, &mut deframer, "provenone")
            .await
            .expect("link establishment");

    // Default is PROVE_NONE; assert it to make the negative premise explicit.
    let strategy = daemon
        .get_proof_strategy(&dest_info.hash)
        .await
        .expect("get_proof_strategy");
    assert_eq!(strategy, "PROVE_NONE");

    let payload = b"unproven raw link packet";
    let (packet_hash, output) = node
        .send_packet_on_link(&link_id, payload)
        .expect("send_packet_on_link");
    dispatch_actions(&mut stream, &output).await;

    let (failed, events) = pump_until(
        &mut node,
        &mut stream,
        &mut deframer,
        Duration::from_secs(10),
        |e| {
            matches!(
                e,
                NodeEvent::LinkDeliveryFailed { link_id: id, packet_hash: hash }
                    if *id == link_id && *hash == packet_hash
            )
        },
    )
    .await;
    assert!(
        failed,
        "receipt must expire into LinkDeliveryFailed without a proof; events: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, NodeEvent::LinkDeliveryConfirmed { .. })),
        "no confirmation may fire under PROVE_NONE (the callback echo is plain \
         data, not a proof)"
    );

    // Python still received the payload; only the proof is absent.
    let received = daemon
        .get_received_packets()
        .await
        .expect("get_received_packets");
    assert!(
        received.iter().any(|p| p.data == payload),
        "Python should receive the payload even without proving it"
    );
}

/// Shared setup for the Python → Rust direction: driver-level node, announced
/// destination with the given proof strategy, Python-initiated link.
async fn setup_python_initiated_link(
    proof_strategy: ProofStrategy,
    test_name: &str,
) -> (
    leviculum_std::driver::ReticulumNode,
    leviculum_std::EventReceiver,
    TestDaemon,
    LinkId,
    String,
    tempfile::TempDir,
) {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let storage = temp_storage(test_name, "node");
    let mut rust_node = leviculum_std::driver::ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");
    let mut event_rx = rust_node.take_event_receiver().unwrap();
    rust_node.start().await.expect("Failed to start node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let identity = Identity::generate(&mut OsRng);
    let public_key_hex = hex::encode(identity.public_key_bytes());
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "rawlink",
        &["responder"],
    )
    .expect("destination");
    dest.set_accepts_links(true);
    dest.set_proof_strategy(proof_strategy);
    let dest_hash = *dest.hash();
    let dest_hash_hex = hex::encode(dest_hash.as_bytes());

    rust_node.register_destination(dest);
    rust_node
        .announce_destination(&dest_hash, Some(b"rawlink-responder"))
        .await
        .expect("announce");

    let has_path = wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &rust_node,
        b"rawlink-responder",
        Duration::from_secs(20),
    )
    .await;
    assert!(has_path, "daemon should learn path to Rust destination");

    let create_link_handle = {
        let cmd_addr = daemon.cmd_addr();
        let dh = dest_hash_hex.clone();
        let pk = public_key_hex.clone();
        tokio::spawn(async move { create_link_raw(cmd_addr, &dh, &pk, 30).await })
    };

    let link_id = wait_for_responder_established_link(&mut event_rx, Duration::from_secs(15))
        .await
        .expect("Rust should establish the incoming link");
    let py_link_hash = create_link_handle
        .await
        .expect("create_link task panicked")
        .expect("Python create_link should succeed");

    (rust_node, event_rx, daemon, link_id, py_link_hash, storage)
}

/// Poll a tracked Python PacketReceipt until it reaches a terminal status or
/// the deadline passes. Returns the last observed status.
async fn poll_receipt_status(daemon: &TestDaemon, receipt_id: &str, duration: Duration) -> String {
    let deadline = tokio::time::Instant::now() + duration;
    let mut status = String::from("SENT");
    while tokio::time::Instant::now() < deadline {
        status = daemon
            .get_receipt_status(receipt_id)
            .await
            .expect("get_receipt_status");
        if status != "SENT" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    status
}

/// Python → Rust raw link packet, our destination proves (ProofStrategy::All).
///
/// `RNS.Packet(link, data).send()` creates a PacketReceipt (Packet.py:274,
/// 123/144); our responder auto-proves plain link data for a PROVE_ALL
/// destination, and Python's `validate_link_proof` must flip the receipt to
/// DELIVERED (Packet.py:450-495).
#[tokio::test]
async fn test_python_raw_link_packet_to_rust_prove_all_receipt_delivered() {
    let (mut rust_node, mut event_rx, daemon, link_id, py_link_hash, _storage) =
        setup_python_initiated_link(
            ProofStrategy::All,
            "test_python_raw_link_packet_to_rust_prove_all",
        )
        .await;

    let payload = b"python raw packet with receipt";
    let (receipt_id, _packet_hash) = daemon
        .send_on_link_tracked(&py_link_hash, payload)
        .await
        .expect("send_on_link_tracked");

    // Our side must see the exact payload on the plain link-data path.
    let data = wait_for_data_event(&mut event_rx, &link_id, Duration::from_secs(10))
        .await
        .expect("LinkDataReceived should fire");
    assert_eq!(data, payload, "payload must round-trip byte-exact");

    // Our auto-proof must reach Python's PacketReceipt.
    let status = poll_receipt_status(&daemon, &receipt_id, Duration::from_secs(10)).await;
    assert_eq!(
        status, "DELIVERED",
        "Python PacketReceipt must be DELIVERED by our data proof"
    );

    rust_node.stop().await.expect("stop node");
}

/// Python → Rust raw link packet without proving (negative).
///
/// With our destination at the default ProofStrategy::None no proof is sent,
/// so Python's PacketReceipt must run into its timeout and report FAILED
/// (Packet.py:431 deadline, check_timeout → FAILED), never DELIVERED.
#[tokio::test]
async fn test_python_raw_link_packet_to_rust_prove_none_receipt_fails() {
    let (mut rust_node, mut event_rx, daemon, link_id, py_link_hash, _storage) =
        setup_python_initiated_link(
            ProofStrategy::None,
            "test_python_raw_link_packet_to_rust_prove_none",
        )
        .await;

    let payload = b"python raw packet unproven";
    let (receipt_id, _packet_hash) = daemon
        .send_on_link_tracked(&py_link_hash, payload)
        .await
        .expect("send_on_link_tracked");

    // Delivery of the data itself is unaffected by the missing proof.
    let data = wait_for_data_event(&mut event_rx, &link_id, Duration::from_secs(10))
        .await
        .expect("LinkDataReceived should fire");
    assert_eq!(data, payload);

    let status = poll_receipt_status(&daemon, &receipt_id, Duration::from_secs(15)).await;
    assert_eq!(
        status, "FAILED",
        "Python PacketReceipt must time out FAILED without our proof"
    );

    rust_node.stop().await.expect("stop node");
}
