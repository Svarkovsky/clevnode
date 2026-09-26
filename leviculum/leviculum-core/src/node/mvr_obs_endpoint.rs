//! mvr for Codeberg #114 (OBS-3): a node acting as the ENDPOINT of a link
//! emits structured events, so the miauhaus-style 3-day event-log analysis can
//! observe endpoint traffic (link accept, proof, local delivery, request /
//! response) and not just the RELAY path (PKT_FORWARD, relay LINK_ENTRY_SET).
//!
//! Topology (sans-I/O, deterministic, single mesh hop): an `initiator` connects
//! a link to a `responder` that owns a link-accepting destination with a
//! `/status` request handler. Per-packet delivery is scripted, so the run is
//! reproducible and < 5 s.
//!
//! Named failure mode this guards: an endpoint node that establishes a link,
//! delivers a locally-addressed packet, proves the link, and answers a request
//! emits NO structured events (the pre-#114 state). The test drives exactly
//! that round trip under captured tracing and asserts each endpoint event fires
//! with its documented keys.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::LinkId;
use crate::node::request::RequestPolicy;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::NoStorage;
use crate::transport::{Action, InterfaceId, TickOutput};

type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

// Tracing capture (assert on the exact structured event lines) routes through
// the shared global-subscriber helper. This module originated the design;
// crate::test_log_capture now owns the single process-global subscriber so all
// mvr tests share one, and no callsite-interest race can hide events.
use crate::test_log_capture::with_captured_logs;

// ----------------------------------------------------------------------------
// Sans-I/O node helpers.
// ----------------------------------------------------------------------------

fn add_iface(node: &mut EndpointNode, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

/// All bytes the node wants to put on the wire this step.
fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

/// Single outbound packet; panics if not exactly one.
fn one_packet(output: &TickOutput) -> Vec<u8> {
    let data = action_data(output);
    assert_eq!(
        data.len(),
        1,
        "expected exactly one outbound packet, got {}",
        data.len()
    );
    data.into_iter().next().unwrap()
}

fn link_established(output: &TickOutput, initiator: bool) -> bool {
    output.events.iter().any(|e| {
        matches!(
            e,
            NodeEvent::LinkEstablished { is_initiator, .. } if *is_initiator == initiator
        )
    })
}

/// Pull the responder-side (link_id, request_id) out of a RequestReceived event.
fn request_received(output: &TickOutput) -> Option<(LinkId, [u8; 16])> {
    output.events.iter().find_map(|e| match e {
        NodeEvent::RequestReceived {
            link_id,
            request_id,
            ..
        } => Some((*link_id, *request_id)),
        _ => None,
    })
}

/// Build a responder node owning a link-accepting destination with a `/status`
/// request handler (mirrors the remote-management responder shape).
fn make_responder() -> (EndpointNode, crate::DestinationHash, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["obs114"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();

    node.register_destination(dest);
    node.register_request_handler(dest_hash, "/status", RequestPolicy::AllowAll);
    (node, dest_hash, signing_key)
}

fn make_initiator() -> EndpointNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().build(OsRng, clock, NoStorage)
}

/// Drive the full endpoint round trip and return the captured tracing.
fn run_endpoint_round_trip() -> String {
    let ((), logs) = with_captured_logs(|| {
        let (mut responder, dest_hash, signing_key) = make_responder();
        let mut initiator = make_initiator();
        let r_iface = add_iface(&mut responder, "R_mesh");
        let i_iface = add_iface(&mut initiator, "I_mesh");

        // 1. Initiator connects -> LinkRequest (broadcast, no path known).
        let (init_link, _routed, out) = initiator.connect(dest_hash, &signing_key);
        let request = one_packet(&out);

        // 2. Responder accepts the inbound link as the ENDPOINT: LINK_LOCAL +
        //    PROOF_GEN + PROOF_SEND fire, the establishment proof is returned.
        let out = responder.handle_packet(InterfaceId(r_iface), &request);
        let proof = one_packet(&out);

        // 3. Initiator validates the proof -> established; sends the RTT packet.
        let out = initiator.handle_packet(InterfaceId(i_iface), &proof);
        assert!(
            link_established(&out, true),
            "initiator must establish on valid proof"
        );
        let rtt = one_packet(&out);

        // 4. Responder receives the RTT -> its link goes Active (endpoint side).
        let out = responder.handle_packet(InterfaceId(r_iface), &rtt);
        assert!(
            link_established(&out, false),
            "responder link must reach Active after the RTT packet"
        );

        // 5. Initiator issues a `/status` request over the link.
        let (_req_id, out) = initiator
            .send_request(&init_link, "/status", None, None)
            .expect("send_request on active link");
        let req_pkt = one_packet(&out);

        // 6. Responder delivers the request locally (PKT_LOCAL) and dispatches
        //    it to the handler (REQUEST_RX); grab the responder-side ids.
        let out = responder.handle_packet(InterfaceId(r_iface), &req_pkt);
        let (resp_link, resp_request_id) =
            request_received(&out).expect("responder must dispatch the request");

        // 7. Responder answers -> RESPONSE_TX. `0x01` is a single valid msgpack
        //    value (positive fixint), satisfying send_response's contract.
        let _ = responder
            .send_response(&resp_link, &resp_request_id, &[0x01])
            .expect("send_response on active link");
    });
    logs
}

/// The endpoint round trip emits every OBS-3 event with its documented keys,
/// and does NOT emit the relay-only LINK_ENTRY_SET (the responder is the
/// endpoint, it never forwards this link for anyone else).
#[test]
fn endpoint_round_trip_emits_obs3_events() {
    let logs = run_endpoint_round_trip();

    // LINK_LOCAL: endpoint link acceptance, with dst/iface/link.
    assert!(
        logs.contains("event=\"LINK_LOCAL\""),
        "LINK_LOCAL must fire on endpoint link accept.\n--- logs ---\n{logs}"
    );
    assert_keys_present(&logs, "LINK_LOCAL", &["dst=", "iface=", "link="]);

    // PROOF_GEN + PROOF_SEND: the link establishment proof path.
    assert!(
        logs.contains("event=\"PROOF_GEN\""),
        "PROOF_GEN must fire for the establishment proof.\n--- logs ---\n{logs}"
    );
    assert_keys_present(&logs, "PROOF_GEN", &["for_pkt=", "to_dst="]);
    assert!(
        logs.contains("event=\"PROOF_SEND\""),
        "PROOF_SEND must fire for the establishment proof.\n--- logs ---\n{logs}"
    );
    assert_keys_present(&logs, "PROOF_SEND", &["pkt=", "iface="]);

    // PKT_LOCAL: local delivery of the request data packet to the endpoint.
    assert!(
        logs.contains("event=\"PKT_LOCAL\""),
        "PKT_LOCAL must fire for endpoint local delivery.\n--- logs ---\n{logs}"
    );
    assert_keys_present(&logs, "PKT_LOCAL", &["dst=", "iface=", "matched="]);

    // REQUEST_RX + RESPONSE_TX: the responder request/response round trip.
    assert!(
        logs.contains("event=\"REQUEST_RX\""),
        "REQUEST_RX must fire when a request is dispatched.\n--- logs ---\n{logs}"
    );
    assert_keys_present(&logs, "REQUEST_RX", &["link=", "path_hash=", "request_id="]);
    assert!(
        logs.contains("event=\"RESPONSE_TX\""),
        "RESPONSE_TX must fire when the responder answers.\n--- logs ---\n{logs}"
    );
    assert_keys_present(&logs, "RESPONSE_TX", &["link=", "request_id=", "len="]);

    // The endpoint must NOT emit the relay-only LINK_ENTRY_SET: it terminates
    // the link, it does not forward it. This is the distinction #114 requires.
    assert!(
        !logs.contains("event=\"LINK_ENTRY_SET\""),
        "endpoint must not emit the relay LINK_ENTRY_SET.\n--- logs ---\n{logs}"
    );
}

/// Assert every listed key token appears on at least one line carrying the
/// given event name (the captured fmt line holds both `event="NAME"` and the
/// `key=` tokens on the same line).
fn assert_keys_present(logs: &str, event: &str, keys: &[&str]) {
    let needle = std::format!("event=\"{event}\"");
    let line = logs
        .lines()
        .find(|l| l.contains(&needle))
        .unwrap_or_else(|| panic!("no line for event {event}"));
    for k in keys {
        assert!(
            line.contains(k),
            "event {event} line missing key '{k}': {line}"
        );
    }
}
