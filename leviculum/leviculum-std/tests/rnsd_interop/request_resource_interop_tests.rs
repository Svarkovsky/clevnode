//! Request-Resource interop tests: >MDU requests as Resources, both
//! directions, plus the documented split-Resource divergence.
//!
//! Python `Link.request()` sends the packed `[timestamp, path_hash, data]`
//! as a single REQUEST packet only while it fits the link MDU
//! (Link.py:496-512); anything larger becomes a Resource carrying the
//! request id (Link.py:514-527). `NodeCore::send_request_resource` is our
//! counterpart for that large-payload path.
//!
//! Divergence under test (negative case): Python auto-splits Resources
//! above `MAX_EFFICIENT_SIZE = 1_048_575` bytes (Resource.py:116, 285-313).
//! Our request-correlation path represents one transfer, so split
//! request/response advertisements are rejected explicitly
//! (link_management.rs `Split request/response Resources are unsupported`)
//! instead of accepting bytes that cannot be reassembled with Python
//! semantics. The peer's request must time out cleanly and the link must
//! stay usable.

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
use leviculum_core::{Destination, DestinationType, Direction, RequestPolicy};
use leviculum_std::interfaces::hdlc::{DeframeResult, Deframer};

use crate::common::{
    connect_to_daemon, create_link_raw, extract_signing_key, parse_dest_hash,
    receive_raw_proof_for_link, send_framed, temp_storage, wait_for_node_reannounce_on_daemon,
    wait_for_responder_established_link, TestClock,
};
use crate::harness::{HarnessError, TestDaemon};

type TestNode = NodeCore<OsRng, TestClock, MemoryStorage>;

/// Msgpack-encode a byte slice as one bin value (the request `data` payload
/// format Python's umsgpack produces for bytes).
fn msgpack_bin(data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &rmpv::Value::Binary(data.to_vec()))
        .expect("msgpack encode");
    buf
}

async fn dispatch_actions(stream: &mut TcpStream, output: &leviculum_core::transport::TickOutput) {
    for action in &output.actions {
        match action {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => {
                send_framed(stream, data).await;
            }
        }
    }
}

/// Establish an initiator link from a NodeCore to a fresh Python destination
/// with an `/echo` request handler registered (ALLOW_ALL).
async fn establish_link_to_echo_handler(
    node: &mut TestNode,
    daemon: &TestDaemon,
    stream: &mut TcpStream,
    deframer: &mut Deframer,
) -> Result<LinkId, HarnessError> {
    let dest_info = daemon.register_destination("reqres", &["echo"]).await?;
    daemon
        .register_echo_request_handler(&dest_info.hash, "/echo")
        .await?;

    let signing_key = extract_signing_key(&dest_info.public_key);
    let dest_hash = parse_dest_hash(&dest_info.hash);

    let (link_id, _, output) = node.connect(dest_hash, &signing_key);
    dispatch_actions(stream, &output).await;

    let proof_raw = receive_raw_proof_for_link(stream, deframer, &link_id, Duration::from_secs(10))
        .await
        .ok_or_else(|| HarnessError::CommandFailed("No link proof received".to_string()))?;
    let output = node.handle_packet(InterfaceId(0), &proof_raw);
    if !output
        .events
        .iter()
        .any(|e| matches!(e, NodeEvent::LinkEstablished { link_id: id, .. } if *id == link_id))
    {
        return Err(HarnessError::CommandFailed(
            "LinkEstablished not emitted".to_string(),
        ));
    }
    dispatch_actions(stream, &output).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(link_id)
}

/// Pump wire + timers until `pred` matches or the deadline passes.
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
    let mut buf = [0u8; 8192];

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

/// Rust → Python: `send_request_resource` round-trips through a Python
/// request handler.
///
/// The 500 kB payload exceeds the link MDU, so the request travels as a
/// Resource with `is_request` set — Python accepts request Resources
/// regardless of the link resource strategy and dispatches the registered
/// handler (Link.py:514-527 sender side; the responder assembles and calls
/// the handler exactly as for packet requests). The echo response is equally
/// oversized and comes back as a response Resource, which must surface as
/// `ResponseReceived` with our request id.
#[tokio::test]
async fn test_rust_request_resource_to_python_echo_round_trip() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut stream = connect_to_daemon(&daemon).await;
    let mut deframer = Deframer::new();
    let mut node = NodeCoreBuilder::new().build(OsRng, TestClock, MemoryStorage::with_defaults());

    let link_id = establish_link_to_echo_handler(&mut node, &daemon, &mut stream, &mut deframer)
        .await
        .expect("link establishment");

    // Both peers negotiate the TCP interface HW_MTU of 262144, so the link
    // MDU is far above the wire MTU; 500 kB exceeds it while staying below
    // the 1_048_575-byte single-segment ceiling.
    let payload: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let request_data = msgpack_bin(&payload);

    let (request_id, _resource_hash, output) = node
        .send_request_resource(&link_id, "/echo", Some(&request_data), None)
        .expect("send_request_resource");
    dispatch_actions(&mut stream, &output).await;

    let mut response = None;
    let (got_response, events) = pump_until(
        &mut node,
        &mut stream,
        &mut deframer,
        Duration::from_secs(30),
        |e| {
            if let NodeEvent::ResponseReceived {
                link_id: id,
                request_id: rid,
                response_data,
                ..
            } = e
            {
                if *id == link_id && *rid == request_id {
                    response = Some(response_data.clone());
                    return true;
                }
            }
            false
        },
    )
    .await;
    assert!(
        got_response,
        "ResponseReceived must fire for our request id; events: {events:?}"
    );

    // Python's echo handler returns the request data verbatim; the response
    // value is the same msgpack bin we sent.
    assert_eq!(
        response.expect("response captured"),
        request_data,
        "echo response must round-trip the >MDU request payload byte-exact"
    );
}

/// Driver-level setup for the Python-as-requester direction: Rust node with
/// an `/interop-echo` handler, Python-initiated link.
async fn setup_rust_echo_responder(
    test_name: &str,
) -> (
    leviculum_std::driver::ReticulumNode,
    leviculum_std::EventReceiver,
    TestDaemon,
    LinkId,
    String,
    tempfile::TempDir,
) {
    crate::common::init_tracing();
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
        "reqres",
        &["responder"],
    )
    .expect("destination");
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    let dest_hash_hex = hex::encode(dest_hash.as_bytes());

    rust_node.register_destination(dest);
    rust_node.register_request_handler(dest_hash, "/interop-echo", RequestPolicy::AllowAll);
    rust_node
        .announce_destination(&dest_hash, Some(b"reqres-responder"))
        .await
        .expect("announce");

    let has_path = wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &rust_node,
        b"reqres-responder",
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

/// Wait for a `RequestReceived` on the given link and return
/// `(request_id, path, data)`.
async fn wait_for_request_received(
    event_rx: &mut leviculum_std::EventReceiver,
    link_id: &LinkId,
    duration: Duration,
) -> Option<([u8; 16], String, Vec<u8>)> {
    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, event_rx.recv()).await {
            Ok(Some(NodeEvent::RequestReceived {
                link_id: id,
                request_id,
                path,
                data,
                ..
            })) if id == *link_id => return Some((request_id, path, data)),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
    None
}

/// Poll a Python RequestReceipt until it leaves SENT/DELIVERED/RECEIVING or
/// the deadline passes. Returns `(status, response)`.
async fn poll_request_status(
    daemon: &TestDaemon,
    request_id: &str,
    duration: Duration,
) -> (String, Option<Vec<u8>>) {
    let deadline = tokio::time::Instant::now() + duration;
    let mut last = (String::from("SENT"), None);
    while tokio::time::Instant::now() < deadline {
        last = daemon
            .get_request_status(request_id)
            .await
            .expect("get_request_status");
        if last.0 == "READY" || last.0 == "FAILED" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    last
}

/// Python → Rust: `Link.request()` with a >MDU payload lands in our
/// registered request handler and the (equally oversized) response returns
/// as a response Resource.
///
/// Python packs the request and, because it exceeds the link MDU, sends it
/// as a request Resource (Link.py:496/514-527). Our core accepts request
/// Resources regardless of resource strategy, assembles, and dispatches
/// `RequestReceived`. `send_response` must refuse the oversized response
/// (`PayloadTooLarge`, no silent auto-upgrade) and `send_response_resource`
/// must deliver it so Python's RequestReceipt turns READY.
#[tokio::test]
async fn test_python_request_resource_to_rust_echo_round_trip() {
    let (mut rust_node, mut event_rx, daemon, link_id, py_link_hash, _storage) =
        setup_rust_echo_responder("test_python_request_resource_to_rust").await;

    // Above the negotiated link MDU (TCP negotiates a 262144 MTU), below the
    // 1_048_575-byte split threshold: Python must choose the request-Resource
    // path (Link.py:496/514-527) with a single segment.
    let payload: Vec<u8> = (0..500_000u32)
        .map(|i| (i.wrapping_mul(7) % 253) as u8)
        .collect();
    let link_mdu = rust_node.link_mdu(&link_id).expect("link MDU");
    assert!(
        payload.len() > link_mdu,
        "test premise: payload ({}) must exceed the negotiated link MDU ({link_mdu})",
        payload.len()
    );

    let py_request_id = daemon
        .send_link_request(&py_link_hash, "/interop-echo", Some(&payload), Some(60.0))
        .await
        .expect("send_link_request");

    let (request_id, path, data) =
        wait_for_request_received(&mut event_rx, &link_id, Duration::from_secs(20))
            .await
            .expect("RequestReceived should fire for the >MDU request");
    assert_eq!(path, "/interop-echo");

    // `data` is the raw msgpack value Python packed (a bin holding `payload`).
    assert_eq!(
        data,
        msgpack_bin(&payload),
        "request payload must arrive byte-exact as one msgpack value"
    );

    // The echoed response also exceeds the MDU: send_response must refuse it
    // (Python parity: no silent auto-upgrade) and the Resource form must work.
    let err = rust_node
        .send_response(&link_id, &request_id, &data)
        .await
        .expect_err("oversized single-packet response must be refused");
    assert!(
        matches!(
            err,
            leviculum_std::Error::Request(leviculum_core::RequestError::PayloadTooLarge)
        ),
        "expected PayloadTooLarge, got {err:?}"
    );
    rust_node
        .send_response_resource(&link_id, &request_id, &data)
        .await
        .expect("send_response_resource");

    let (status, response) =
        poll_request_status(&daemon, &py_request_id, Duration::from_secs(30)).await;
    assert_eq!(status, "READY", "Python RequestReceipt must turn READY");
    assert_eq!(
        response.as_deref(),
        Some(payload.as_slice()),
        "Python must unpack the echoed bytes it originally sent"
    );

    rust_node.stop().await.expect("stop node");
}

/// Negative divergence: a >1_048_575-byte request from Python arrives as a
/// split Resource advertisement, our node ignores it, Python times out
/// cleanly, and the link stays healthy.
///
/// Python spills the packed request to a tempfile and splits it
/// (Resource.py:273-279, 285-313: `total_segments > 1`, `split = True`);
/// our core rejects split request/response advertisements explicitly rather
/// than accepting bytes it cannot reassemble with Python semantics. The
/// Python RequestReceipt must FAIL (Link.py:1402-1434), no `RequestReceived`
/// may fire on our side, and a subsequent small request over the same link
/// must still succeed.
#[tokio::test]
async fn test_python_split_request_resource_ignored_link_survives() {
    let (rust_node, mut event_rx, daemon, link_id, py_link_hash, _storage) =
        setup_rust_echo_responder("test_python_split_request_resource").await;

    // 1_100_000 raw bytes: with msgpack framing and the [ts, path_hash, data]
    // wrapper the packed request exceeds MAX_EFFICIENT_SIZE = 1_048_575.
    let payload = vec![0x5Au8; 1_100_000];

    // Python forwards the timeout to the Resource (Link.py:517), where the
    // ADV watchdog waits timeout+1s per retry across MAX_ADV_RETRIES=4
    // (Resource.py:131, 574): 2s keeps the FAILED verdict within ~15s.
    let py_request_id = daemon
        .send_link_request(&py_link_hash, "/interop-echo", Some(&payload), Some(2.0))
        .await
        .expect("send_link_request");

    // The split advertisement must be ignored: no RequestReceived within the
    // window in which Python is still retrying the ADV.
    let received =
        wait_for_request_received(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(
        received.is_none(),
        "split request Resource must not dispatch a request handler"
    );

    // Python's RequestReceipt must conclude FAILED, not hang or go READY.
    let (status, _response) =
        poll_request_status(&daemon, &py_request_id, Duration::from_secs(90)).await;
    assert_eq!(
        status, "FAILED",
        "Python Link.request must time out cleanly on the ignored split ADV"
    );

    // The link is still healthy: the daemon drops closed links from its
    // registry (_on_link_closed), so "found" means it never tore down...
    let link_status = daemon
        .get_link_status(&py_link_hash)
        .await
        .expect("get_link_status");
    assert_eq!(
        link_status.status, "found",
        "link must survive the ignored split request"
    );

    // ...and a small request over the same link still round-trips.
    let small_payload = b"small follow-up".to_vec();
    let py_small_id = daemon
        .send_link_request(
            &py_link_hash,
            "/interop-echo",
            Some(&small_payload),
            Some(15.0),
        )
        .await
        .expect("small send_link_request");

    let (request_id, _path, data) =
        wait_for_request_received(&mut event_rx, &link_id, Duration::from_secs(15))
            .await
            .expect("small request must still reach our handler");
    assert_eq!(data, msgpack_bin(&small_payload));
    rust_node
        .send_response(&link_id, &request_id, &data)
        .await
        .expect("small response fits a single packet");

    let (status, response) =
        poll_request_status(&daemon, &py_small_id, Duration::from_secs(20)).await;
    assert_eq!(status, "READY", "follow-up request must succeed");
    assert_eq!(response.as_deref(), Some(small_payload.as_slice()));
}
