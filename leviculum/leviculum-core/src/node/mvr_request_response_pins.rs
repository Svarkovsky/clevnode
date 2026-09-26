//! mvr pins for the #138 request/response hardening (red on pre-#138 master).
//!
//! F1: `send_response_resource` / `send_file_response` with a payload above
//! `RESOURCE_MAX_EFFICIENT_SIZE` (1_048_575 B) must refuse with
//! `ResourceError::ResourceTooLarge`. Python splits such payloads into
//! multiple Resource segments; the pre-#138 code advertised the unsplit
//! single-segment form instead, which no Python peer can reassemble. Until
//! request/response segmentation exists, the only wire-compatible behavior
//! is an explicit sender-side refusal.
//!
//! F2: a RESPONSE packet carrying a valid pending `request_id` but arriving
//! on a DIFFERENT link must not consume the pending request. Pre-#138 the
//! pending-request map was keyed by `request_id` alone, so any peer the node
//! holds a link with could answer (and thereby steal) a request issued on
//! another link; the real response was then dropped as "no pending request".
//!
//! Topology: deterministic sans-I/O endpoints, same pattern as the other mvr
//! modules. F2 uses one initiator with links to two independent responders.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::TRUNCATED_HASHBYTES;
use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::LinkId;
use crate::node::request::RequestPolicy;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::resource::msgpack::{write_bin, write_fixstr};
use crate::resource::{ResourceError, RESOURCE_MAX_EFFICIENT_SIZE};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::NoStorage;
use crate::transport::{Action, InterfaceId, TickOutput};

type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

// ----------------------------------------------------------------------------
// Sans-I/O helpers (same pattern as the other mvr modules).
// ----------------------------------------------------------------------------

fn add_iface(node: &mut EndpointNode, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

/// Deliver packets to `target`, returning both its outbound packets and every
/// event it emitted.
fn deliver_collect(
    target: &mut EndpointNode,
    iface: usize,
    packets: Vec<Vec<u8>>,
) -> (Vec<Vec<u8>>, Vec<NodeEvent>) {
    let mut out = Vec::new();
    let mut events = Vec::new();
    for pkt in packets {
        let o = target.handle_packet(InterfaceId(iface), &pkt);
        out.extend(action_data(&o));
        events.extend(o.events);
    }
    (out, events)
}

fn make_responder(aspect: &'static str) -> (EndpointNode, crate::DestinationHash, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrpins",
        &[aspect],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    node.register_request_handler(dest_hash, "/echo", RequestPolicy::AllowAll);
    (node, dest_hash, signing_key)
}

fn make_initiator() -> EndpointNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().build(OsRng, clock, NoStorage)
}

/// Drive one initiator <-> responder link to Active on both sides. Returns the
/// caller-side and responder-side link ids.
fn establish(
    initiator: &mut EndpointNode,
    i_iface: usize,
    responder: &mut EndpointNode,
    r_iface: usize,
    dest_hash: crate::DestinationHash,
    signing_key: [u8; 32],
) -> (LinkId, LinkId) {
    let (caller_link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);

    let mut responder_link_id = None;
    let mut for_responder = action_data(&out);
    for _ in 0..8 {
        if for_responder.is_empty() {
            break;
        }
        let (back, r_events) = deliver_collect(responder, r_iface, for_responder);
        for ev in &r_events {
            if let NodeEvent::LinkEstablished { link_id, .. } = ev {
                responder_link_id = Some(*link_id);
            }
        }
        let (fwd, _) = deliver_collect(initiator, i_iface, back);
        for_responder = fwd;
    }

    (
        caller_link_id,
        responder_link_id.expect("responder side must reach Active"),
    )
}

/// A response value exceeding `RESOURCE_MAX_EFFICIENT_SIZE`: one msgpack bin
/// whose raw content alone is already past the single-segment boundary.
fn oversized_msgpack_value() -> Vec<u8> {
    let raw: Vec<u8> = (0..RESOURCE_MAX_EFFICIENT_SIZE + 1)
        .map(|i| (i % 251) as u8)
        .collect();
    let mut v = Vec::new();
    write_bin(&mut v, &raw);
    v
}

// ----------------------------------------------------------------------------
// F1: oversized internal response senders must refuse, not send unsplit.
// ----------------------------------------------------------------------------

/// `send_response_resource` above the single-segment boundary must return
/// `ResourceTooLarge` instead of advertising an unsplit Resource no Python
/// peer can reassemble.
#[test]
fn oversized_send_response_resource_is_refused() {
    let (mut responder, dest_hash, signing_key) = make_responder("f1-wrapped");
    let mut initiator = make_initiator();
    let r_iface = add_iface(&mut responder, "R_mesh");
    let i_iface = add_iface(&mut initiator, "I_mesh");
    let (_caller_link, responder_link) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    let request_id = [0x42u8; TRUNCATED_HASHBYTES];
    match responder.send_response_resource(&responder_link, &request_id, &oversized_msgpack_value())
    {
        Err(ResourceError::ResourceTooLarge) => {}
        Err(other) => panic!("expected ResourceTooLarge, got {other}"),
        Ok(_) => panic!("oversized send_response_resource must be refused, but it advertised"),
    }
}

/// `send_file_response` above the single-segment boundary must return
/// `ResourceTooLarge` for the same reason.
#[test]
fn oversized_send_file_response_is_refused() {
    let (mut responder, dest_hash, signing_key) = make_responder("f1-file");
    let mut initiator = make_initiator();
    let r_iface = add_iface(&mut responder, "R_mesh");
    let i_iface = add_iface(&mut initiator, "I_mesh");
    let (_caller_link, responder_link) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    let data: Vec<u8> = (0..RESOURCE_MAX_EFFICIENT_SIZE + 1)
        .map(|i| (i % 251) as u8)
        .collect();
    // `{"name": "big.bin"}` as msgpack (fixmap(1) + fixstr + fixstr).
    let mut metadata = std::vec![0x81];
    write_fixstr(&mut metadata, "name");
    write_fixstr(&mut metadata, "big.bin");

    let request_id = [0x42u8; TRUNCATED_HASHBYTES];
    match responder.send_file_response(&responder_link, &request_id, &data, &metadata) {
        Err(ResourceError::ResourceTooLarge) => {}
        Err(other) => panic!("expected ResourceTooLarge, got {other}"),
        Ok(_) => panic!("oversized send_file_response must be refused, but it advertised"),
    }
}

// ----------------------------------------------------------------------------
// F2: response correlation must require the request's own link.
// ----------------------------------------------------------------------------

/// A response with a valid pending `request_id` arriving on a DIFFERENT link
/// must be ignored, and the real response on the original link must still be
/// delivered afterwards.
#[test]
fn response_on_wrong_link_does_not_consume_pending_request() {
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "I_mesh");

    let (mut responder_a, dest_a, key_a) = make_responder("alpha");
    let a_iface = add_iface(&mut responder_a, "A_mesh");
    let (mut responder_b, dest_b, key_b) = make_responder("beta");
    let b_iface = add_iface(&mut responder_b, "B_mesh");

    let (link_a, responder_a_link) = establish(
        &mut initiator,
        i_iface,
        &mut responder_a,
        a_iface,
        dest_a,
        key_a,
    );
    let (_link_b, responder_b_link) = establish(
        &mut initiator,
        i_iface,
        &mut responder_b,
        b_iface,
        dest_b,
        key_b,
    );

    // Request goes out on link A; responder A dispatches it.
    let (request_id, out) = initiator
        .send_request(&link_a, "/echo", None, None)
        .expect("send_request on active link A");
    let (to_initiator, a_events) = deliver_collect(&mut responder_a, a_iface, action_data(&out));
    assert!(
        a_events
            .iter()
            .any(|e| matches!(e, NodeEvent::RequestReceived { .. })),
        "responder A must dispatch the request.\nevents: {a_events:?}"
    );
    let _ = deliver_collect(&mut initiator, i_iface, to_initiator);

    // Responder B answers the SAME request_id on link B: the initiator must
    // not deliver it, and must keep the pending request intact.
    let mut spoofed = Vec::new();
    write_fixstr(&mut spoofed, "spoofed");
    let out_b = responder_b
        .send_response(&responder_b_link, &request_id, &spoofed)
        .expect("responder B can build a response packet");
    let (_, init_events) = deliver_collect(&mut initiator, i_iface, action_data(&out_b));
    assert!(
        !init_events
            .iter()
            .any(|e| matches!(e, NodeEvent::ResponseReceived { .. })),
        "a response arriving on a different link must not be delivered.\nevents: {init_events:?}"
    );

    // The real response on link A must still find its pending request.
    let mut real = Vec::new();
    write_fixstr(&mut real, "real");
    let out_a = responder_a
        .send_response(&responder_a_link, &request_id, &real)
        .expect("responder A can build a response packet");
    let (_, init_events) = deliver_collect(&mut initiator, i_iface, action_data(&out_a));
    let delivered = init_events.iter().find_map(|e| match e {
        NodeEvent::ResponseReceived {
            request_id: rid,
            response_data,
            ..
        } if *rid == request_id => Some(response_data.clone()),
        _ => None,
    });
    assert_eq!(
        delivered.as_deref(),
        Some(real.as_slice()),
        "the real response on the original link must still be delivered.\nevents: {init_events:?}"
    );
}
