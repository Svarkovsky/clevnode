//! Driver-level test for `ReticulumNode::send_response_resource`.
//!
//! The mechanism (response Resource with the `is_response` flag, delivered to
//! the requester as `ResponseReceived` via the is_response bypass) is proven
//! end-to-end in core (`leviculum-core/src/node/mvr_response_resource.rs`) and
//! against Python on the client side (`nomad_page_tests.rs`). This file proves
//! the NEW public driver method is wired and delegates correctly: two
//! in-process Rust nodes over TCP, the responder answers a request larger than
//! the link MDU via `send_response_resource`, and the requester receives the
//! complete bytes as a single `ResponseReceived`.
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop response_resource
//! ```

use std::time::Duration;

use rand_core::OsRng;

use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationType, Direction, RequestError, RequestPolicy};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::{Error, NodeEvent};

use crate::common::{init_tracing, temp_storage};

/// Over TCP, link-MTU discovery raises the link MDU up to the interface
/// HW_MTU (262144 B), so the response must exceed that ceiling to genuinely
/// require the resource path.
const RESPONSE_PAYLOAD_LEN: usize = 300_000;

/// One msgpack bin32 value of `RESPONSE_PAYLOAD_LEN` deterministic bytes.
fn large_response_value() -> Vec<u8> {
    let mut v = Vec::with_capacity(RESPONSE_PAYLOAD_LEN + 5);
    v.push(0xc6); // bin32
    v.extend_from_slice(&(RESPONSE_PAYLOAD_LEN as u32).to_be_bytes());
    v.extend((0..RESPONSE_PAYLOAD_LEN).map(|i| (i % 251) as u8));
    v
}

/// Full round trip: requester link → request → responder answers via the new
/// public `send_response_resource` → requester gets `ResponseReceived` with
/// the exact bytes. The requester keeps the default AcceptNone resource
/// strategy: delivery must ride the is_response bypass, not an accept-all
/// opt-in.
#[tokio::test]
async fn test_send_response_resource_delivers_large_response() {
    init_tracing();

    // Responder: serves the request handler over a TCP server interface.
    let responder_storage = temp_storage("test_send_response_resource", "responder");
    let mut responder = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_server("127.0.0.1:0".parse().expect("loopback addr"))
        .storage_path(responder_storage.path().to_path_buf())
        .build()
        .await
        .expect("build responder node");
    let mut responder_events = responder.take_event_receiver().expect("responder events");

    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "responseresource",
        &["test"],
    )
    .expect("create destination");
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    responder.register_destination(dest);
    responder.register_request_handler(dest_hash, "/page/large.mu", RequestPolicy::AllowAll);

    responder.start().await.expect("start responder node");
    let server_addr = *responder
        .tcp_listen_addrs()
        .first()
        .expect("server reports its bound TCP listener (Codeberg #221)");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Requester: an edge node with a direct TCP client to the responder.
    let requester_storage = temp_storage("test_send_response_resource", "requester");
    let mut requester = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(server_addr)
        .storage_path(requester_storage.path().to_path_buf())
        .build()
        .await
        .expect("build requester node");
    let mut requester_events = requester.take_event_receiver().expect("requester events");
    requester.start().await.expect("start requester node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Announce so the requester learns the path and the responder identity.
    responder
        .announce_destination(&dest_hash, None)
        .await
        .expect("announce destination");
    assert!(
        requester
            .wait_for_path(&dest_hash, Duration::from_secs(10), Duration::from_secs(2))
            .await
            .expect("wait_for_path"),
        "requester must learn the path from the announce"
    );

    // Establish the link.
    let remote_identity = requester
        .get_identity(&dest_hash)
        .expect("announce must deliver the responder identity");
    let pk = remote_identity.public_key_bytes();
    let mut signing_key = [0u8; 32];
    signing_key.copy_from_slice(&pk[32..64]);
    let _handle = requester
        .connect(&dest_hash, &signing_key)
        .await
        .expect("connect to responder destination");
    let link_id = loop {
        match tokio::time::timeout(Duration::from_secs(10), requester_events.recv())
            .await
            .expect("timed out waiting for LinkEstablished")
        {
            Some(NodeEvent::LinkEstablished { link_id, .. }) => break link_id,
            Some(NodeEvent::LinkClosed { .. }) => panic!("link closed before establishment"),
            Some(_) => continue,
            None => panic!("requester event channel closed"),
        }
    };

    // Issue the request.
    let request_id = requester
        .send_request(&link_id, "/page/large.mu", None, Some(30_000))
        .await
        .expect("send_request");

    // Responder dispatches the request to the app.
    let (resp_link_id, resp_request_id) = loop {
        match tokio::time::timeout(Duration::from_secs(10), responder_events.recv())
            .await
            .expect("timed out waiting for RequestReceived")
        {
            Some(NodeEvent::RequestReceived {
                link_id,
                request_id,
                path,
                ..
            }) if path == "/page/large.mu" => break (link_id, request_id),
            Some(_) => continue,
            None => panic!("responder event channel closed"),
        }
    };
    assert_eq!(
        resp_request_id, request_id,
        "responder request_id must match the requester's"
    );

    // The response exceeds the link MDU: the single-packet path must refuse
    // it — exactly the situation the new public method exists for.
    let response_value = large_response_value();
    let single = responder
        .send_response(&resp_link_id, &resp_request_id, &response_value)
        .await;
    assert!(
        matches!(single, Err(Error::Request(RequestError::PayloadTooLarge))),
        "send_response must refuse an over-MDU response, got: {single:?}"
    );

    // The new public method sends it as a response Resource.
    responder
        .send_response_resource(&resp_link_id, &resp_request_id, &response_value)
        .await
        .expect("send_response_resource must accept the over-MDU response");

    // The requester receives the complete bytes as ResponseReceived.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let (response_data, metadata) = loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .expect("timed out waiting for ResponseReceived");
        match tokio::time::timeout(remaining, requester_events.recv())
            .await
            .expect("timed out waiting for ResponseReceived")
        {
            Some(NodeEvent::ResponseReceived {
                request_id: rid,
                response_data,
                metadata,
                ..
            }) if rid == request_id => break (response_data, metadata),
            Some(_) => continue,
            None => panic!("requester event channel closed"),
        }
    };
    assert_eq!(
        response_data, response_value,
        "requester must receive the exact response value the responder sent"
    );
    assert_eq!(metadata, None, "a wrapped response carries no metadata");

    responder.stop().await.ok();
    requester.stop().await.ok();
}
