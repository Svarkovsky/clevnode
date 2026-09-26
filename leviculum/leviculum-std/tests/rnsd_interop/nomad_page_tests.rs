//! NomadNet page fetch-path interoperability tests against real Python RNS.
//!
//! NomadNet pages are fetched over RAW RNS request/response (not LXMF): the
//! server calls `destination.register_request_handler("/page/<name>.mu", ...)`
//! (nomadnet/Node.py) and the client issues `Link.request(...)`. A response that
//! fits one packet returns as a single RESPONSE packet; a response larger than
//! the link MDU is auto-upgraded by RNS to a Resource carrying `is_response=true`
//! and the `request_id` (`RNS.Link.handle_request`). This suite drives the Rust
//! client (`ReticulumNode::send_request` -> `NodeEvent::ResponseReceived`)
//! against a Python page node stood up via the shared `test_daemon.py` harness,
//! and asserts the received bytes are byte-identical to what Python served for:
//!
//! - `/page/small.mu` — a few hundred bytes -> single-packet response path.
//! - `/page/large.mu` — several KB -> the `is_response` Resource path (the P1
//!   case; the Rust initiator keeps the DEFAULT `AcceptNone` resource strategy,
//!   so the response resource must ride the `is_response`/`request_id` accept
//!   bypass, not a broad accept-all opt-in).
//! - `/page/echo.mu` — echoes the request data (query-field round trip).
//! - an unregistered path -> the client sees a clean `RequestTimedOut`, no hang.
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test -p leviculum-std --test rnsd_interop nomad_page -- --test-threads=1
//! ```

use std::time::Duration;

use leviculum_core::identity::Identity;
use leviculum_core::node::NodeEvent;
use leviculum_core::{Destination, DestinationType, Direction, LinkId};

use crate::common::{
    build_rust_node, extract_signing_key, parse_dest_hash, wait_for_event,
    wait_for_link_established,
};
use crate::harness::TestDaemon;

use leviculum_std::driver::ReticulumNode;
use leviculum_std::EventReceiver;

/// Decode a msgpack bin value into its raw bytes. Python packs a page (returned
/// as `bytes` from the handler) as a msgpack bin, so `ResponseReceived`'s raw
/// `response_data` is exactly that single bin value.
fn decode_msgpack_bin(data: &[u8]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(data);
    match rmpv::decode::read_value(&mut cursor).expect("response_data must be valid msgpack") {
        rmpv::Value::Binary(b) => b,
        other => panic!("expected msgpack Binary response, got: {other:?}"),
    }
}

/// Encode a byte slice as a single msgpack bin value (a valid request payload).
fn encode_msgpack_bin(data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &rmpv::Value::Binary(data.to_vec()))
        .expect("msgpack encode should not fail");
    buf
}

/// Stand up a Python NomadNet-style page node, connect a Rust initiator to it,
/// and drive the link to Active. Returns everything a page-fetch test needs.
async fn setup_page_link() -> (
    TestDaemon,
    ReticulumNode,
    EventReceiver,
    LinkId,
    tempfile::TempDir,
) {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let (rust_node, event_rx, storage) = build_rust_node(&daemon).await;

    // Register a NomadNet node destination (app aspects like NomadNet's node)
    // and its page handlers.
    let dest_info = daemon
        .register_destination("nomadnetwork", &["node"])
        .await
        .expect("Failed to register page destination");
    daemon
        .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
        .await
        .expect("Failed to set proof strategy");
    daemon
        .register_page_request_handler(&dest_info.hash)
        .await
        .expect("Failed to register page handlers");

    let py_dest_hash = parse_dest_hash(&dest_info.hash);

    // Register Python's destination on the Rust side for proof verification.
    let py_pub_bytes = hex::decode(&dest_info.public_key).expect("Invalid public key hex");
    let py_identity =
        Identity::from_public_key_bytes(&py_pub_bytes).expect("Failed to parse Python identity");
    let py_dest_on_rust = Destination::new(
        Some(py_identity),
        Direction::Out,
        DestinationType::Single,
        "nomadnetwork",
        &["node"],
    )
    .expect("Failed to create destination");
    rust_node.register_destination(py_dest_on_rust);

    // Python announces -> Rust learns path + identity.
    daemon
        .announce_destination(&dest_info.hash, b"nomad-page-node")
        .await
        .expect("Python announce should succeed");
    let found = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&py_dest_hash),
        &daemon,
        &dest_info.hash,
        b"nomad-page-node",
        Duration::from_secs(10),
    )
    .await;
    assert!(found, "Rust should learn path to the Python page node");

    // Establish the link (Rust initiator). The link keeps the DEFAULT resource
    // strategy (AcceptNone) on purpose: the large-page response resource must be
    // accepted via the is_response/request_id bypass.
    let signing_key = extract_signing_key(&dest_info.public_key);
    let link_handle = rust_node
        .connect(&py_dest_hash, &signing_key)
        .await
        .expect("Connect should succeed");
    let link_id = *link_handle.link_id();

    let mut event_rx = event_rx;
    let established =
        wait_for_link_established(&mut event_rx, &link_id, Duration::from_secs(10)).await;
    assert!(established, "Link to the Python page node should establish");

    (daemon, rust_node, event_rx, link_id, storage)
}

/// Issue a request and wait for the matching `ResponseReceived`, returning the
/// raw msgpack `response_data`. Fails loudly on a timeout of the request.
async fn fetch(
    node: &ReticulumNode,
    event_rx: &mut EventReceiver,
    link_id: &LinkId,
    path: &str,
    data: Option<&[u8]>,
) -> Vec<u8> {
    let request_id = node
        .send_request(link_id, path, data, Some(20_000))
        .await
        .expect("send_request should dispatch");

    wait_for_event(
        event_rx,
        Duration::from_secs(30),
        move |event| match event {
            NodeEvent::ResponseReceived {
                request_id: rid,
                response_data,
                ..
            } if rid == request_id => Some(response_data),
            NodeEvent::RequestTimedOut {
                request_id: rid, ..
            } if rid == request_id => {
                panic!("request for {path} timed out instead of returning a response")
            }
            _ => None,
        },
    )
    .await
    .unwrap_or_else(|| panic!("no ResponseReceived for {path} within 30s"))
}

/// `/page/small.mu`: a few hundred bytes come back as a single-packet response,
/// byte-identical to what Python serves.
#[tokio::test]
async fn test_fetch_small_page_single_packet() {
    let (daemon, mut rust_node, mut event_rx, link_id, _storage) = setup_page_link().await;

    let served = daemon
        .get_page_content("/page/small.mu")
        .await
        .expect("get_page_content should succeed");

    let response_data = fetch(&rust_node, &mut event_rx, &link_id, "/page/small.mu", None).await;
    let page = decode_msgpack_bin(&response_data);

    assert_eq!(
        page, served,
        "small page bytes must be byte-identical to what Python served"
    );
    rust_node.stop().await.expect("Failed to stop node");
}

/// `/page/large.mu` (THE P1 case): several KB come back as an `is_response`
/// Resource and surface as `ResponseReceived`, byte-identical to Python. The
/// Rust initiator never opts into accepting resources; the is_response bypass
/// is what delivers this.
#[tokio::test]
async fn test_fetch_large_page_is_response_resource() {
    let (daemon, mut rust_node, mut event_rx, link_id, _storage) = setup_page_link().await;

    let served = daemon
        .get_page_content("/page/large.mu")
        .await
        .expect("get_page_content should succeed");
    // Over TCP, RNS link-MTU discovery raises the link MDU up to the interface
    // HW_MTU (262144 B), so the page must exceed that ceiling to force the
    // is_response Resource path rather than a single large RESPONSE packet.
    assert!(
        served.len() > 262_144,
        "large page must exceed the max link MDU to force the resource path (got {})",
        served.len()
    );

    let response_data = fetch(&rust_node, &mut event_rx, &link_id, "/page/large.mu", None).await;
    let page = decode_msgpack_bin(&response_data);

    assert_eq!(
        page, served,
        "large page bytes must be byte-identical to what Python served"
    );
    rust_node.stop().await.expect("Failed to stop node");
}

/// `/page/echo.mu`: the request data (a query-fields blob) round-trips
/// byte-identically through the Python handler.
#[tokio::test]
async fn test_fetch_echo_page_fields_roundtrip() {
    let (_daemon, mut rust_node, mut event_rx, link_id, _storage) = setup_page_link().await;

    let fields = b"var_field=value|var_page=/index.mu|var_n=42";
    let request_data = encode_msgpack_bin(fields);

    let response_data = fetch(
        &rust_node,
        &mut event_rx,
        &link_id,
        "/page/echo.mu",
        Some(&request_data),
    )
    .await;
    let echoed = decode_msgpack_bin(&response_data);

    assert_eq!(
        echoed.as_slice(),
        fields.as_slice(),
        "echo page must return the request query fields verbatim"
    );
    rust_node.stop().await.expect("Failed to stop node");
}

/// An unregistered path must produce a clean `RequestTimedOut`, never a hang:
/// Python sends no response for an unknown path.
#[tokio::test]
async fn test_fetch_unregistered_path_times_out_cleanly() {
    let (_daemon, mut rust_node, mut event_rx, link_id, _storage) = setup_page_link().await;

    let request_id = rust_node
        .send_request(&link_id, "/page/does-not-exist.mu", None, Some(2_000))
        .await
        .expect("send_request should dispatch");

    let timed_out = wait_for_event(
        &mut event_rx,
        Duration::from_secs(15),
        move |event| match event {
            NodeEvent::RequestTimedOut {
                request_id: rid, ..
            } if rid == request_id => Some(true),
            NodeEvent::ResponseReceived {
                request_id: rid, ..
            } if rid == request_id => Some(false),
            _ => None,
        },
    )
    .await;

    assert_eq!(
        timed_out,
        Some(true),
        "an unregistered path must surface RequestTimedOut, not a response or a hang"
    );
    rust_node.stop().await.expect("Failed to stop node");
}
