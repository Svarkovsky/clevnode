//! Relaying a `LinkIdentify` (0xFB) data packet through transit nodes.
//!
//! Identify forwarding across transit hops is a distinct path from LRPROOF and
//! generic link-data relay, and had no coverage. Sans-I/O, `MockClock`:
//! `I -> A -> G -> R` with symmetric single hops. The link is driven to Active
//! on both sides (including the initiator's RTT to the responder), then the
//! initiator identifies and the identify packet is shuttled hop by hop; the
//! test asserts the responder receives it and binds the remote identity. A
//! single-hop control (`I -> A -> R`) guards the same path with one relay.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, NoStorage};
use crate::transport::{Action, InterfaceId, TickOutput};

type TransportNode = NodeCore<OsRng, MockClock, MemoryStorage>;
type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

fn add_iface<C, S>(
    node: &mut NodeCore<OsRng, C, S>,
    name: &'static str,
    local_client: bool,
) -> usize
where
    C: crate::traits::Clock,
    S: crate::traits::Storage,
{
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    if local_client {
        node.set_interface_local_client(idx, true);
    }
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

fn has_link_established(output: &TickOutput) -> bool {
    output
        .events
        .iter()
        .any(|e| matches!(e, NodeEvent::LinkEstablished { .. }))
}

fn identified_hash(output: &TickOutput) -> Option<[u8; 16]> {
    output.events.iter().find_map(|e| match e {
        NodeEvent::LinkIdentified { identity_hash, .. } => Some(*identity_hash),
        _ => None,
    })
}

/// R's forwarded announce re-broadcast happens on a timeout tick.
fn forward_announce(relay: &mut TransportNode, in_iface: usize, raw: &[u8]) -> Vec<Vec<u8>> {
    let _ = relay.handle_packet(InterfaceId(in_iface), raw);
    let now = relay.transport().clock().now_ms();
    relay.transport().clock().set(now + 100_000);
    action_data(&relay.handle_timeout())
}

fn make_responder() -> (EndpointNode, crate::DestinationHash, [u8; 32], Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["identify"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();

    let announce = dest
        .announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000)
        .unwrap();
    let mut buf = [0u8; crate::constants::MTU];
    let len = announce.pack(&mut buf).unwrap();
    let announce_raw = buf[..len].to_vec();

    node.register_destination(dest);
    (node, dest_hash, signing_key, announce_raw)
}

fn make_transport_node() -> TransportNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        clock,
        MemoryStorage::with_defaults(),
    )
}

fn make_initiator() -> EndpointNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().build(OsRng, clock, NoStorage)
}

/// `I -> A -> G -> R`, symmetric single hops. R is 2 hops from A (via G),
/// 1 hop from G. Establish the link to Active on both ends, then the initiator
/// identifies; the identify packet is forwarded A -> G -> R. Returns whether R
/// registered the remote identity, plus R's `LinkIdentified` hash if any.
#[test]
fn identify_relays_across_two_transit_hops() {
    let (mut responder, dest_hash, signing_key, announce_raw) = make_responder();
    let mut relay_a = make_transport_node();
    let mut relay_g = make_transport_node();
    let mut initiator = make_initiator();

    let a_local = add_iface(&mut relay_a, "A_local_initiator", true); // I -> A
    let a_to_g = add_iface(&mut relay_a, "A_to_G", false);
    let g_from_a = add_iface(&mut relay_g, "G_from_A", false);
    let g_to_r = add_iface(&mut relay_g, "G_to_R", false);
    let r_iface = add_iface(&mut responder, "R_mesh", false);
    let i_iface = add_iface(&mut initiator, "I_to_A", false);

    // Path build: R -> G (direct, 1 hop), then G re-broadcasts -> A (2 hops).
    let g_forwarded_ann = forward_announce(&mut relay_g, g_to_r, &announce_raw);
    assert_eq!(
        relay_g.hops_to(&dest_hash),
        Some(1),
        "G must reach R in 1 hop"
    );
    for ann in &g_forwarded_ann {
        let _ = relay_a.handle_packet(InterfaceId(a_to_g), ann);
    }
    assert_eq!(
        relay_a.hops_to(&dest_hash),
        Some(2),
        "A must reach R in 2 hops via G"
    );

    // Establish: request I -> A -> G -> R, proof R -> G -> A -> I.
    let (link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
    let request = one_packet(&out);
    let a_req = one_packet(&relay_a.handle_packet(InterfaceId(a_local), &request));
    let g_req = one_packet(&relay_g.handle_packet(InterfaceId(g_from_a), &a_req));
    let proof = one_packet(&responder.handle_packet(InterfaceId(r_iface), &g_req));
    let g_proof = one_packet(&relay_g.handle_packet(InterfaceId(g_to_r), &proof));
    let a_proof = one_packet(&relay_a.handle_packet(InterfaceId(a_to_g), &g_proof));
    let est = initiator.handle_packet(InterfaceId(i_iface), &a_proof);
    assert!(
        has_link_established(&est),
        "link must establish across two transit hops"
    );

    // Finish the handshake: the initiator's RTT packet must reach R to bring the
    // responder link Active (I -> A -> G -> R). Without it R stays PENDING.
    for rtt in action_data(&est) {
        let a_rtt = action_data(&relay_a.handle_packet(InterfaceId(a_local), &rtt));
        for p in a_rtt {
            let g_rtt = action_data(&relay_g.handle_packet(InterfaceId(g_from_a), &p));
            for q in g_rtt {
                let _ = responder.handle_packet(InterfaceId(r_iface), &q);
            }
        }
    }
    assert_eq!(
        responder.active_link_count(),
        1,
        "responder link must be active"
    );
    assert!(
        responder.get_remote_identity(&link_id).is_none(),
        "responder must not have a remote identity before identify"
    );

    // Identify: I -> A -> G -> R.
    let identity = Identity::generate(&mut OsRng);
    let expected = *identity.hash();
    let ident_out = initiator
        .identify_link(&link_id, &identity)
        .expect("identify_link must succeed on an active link");
    let i_ident = one_packet(&ident_out);
    let a_fwd = action_data(&relay_a.handle_packet(InterfaceId(a_local), &i_ident));
    assert!(
        !a_fwd.is_empty(),
        "relay A must forward the identify toward G"
    );
    let g_fwd = action_data(&relay_g.handle_packet(InterfaceId(g_from_a), &a_fwd[0]));
    assert!(
        !g_fwd.is_empty(),
        "relay G must forward the identify toward R"
    );
    let r_out = responder.handle_packet(InterfaceId(r_iface), &g_fwd[0]);

    assert_eq!(
        identified_hash(&r_out),
        Some(expected),
        "responder must emit LinkIdentified after a two-hop-relayed identify"
    );
    let remote = responder
        .get_remote_identity(&link_id)
        .expect("responder must store the remote identity");
    assert_eq!(remote.hash(), &expected);
}

/// Control: the SAME identify over a SINGLE transit hop `I -> A -> R`. If this
/// passes and the two-hop test fails, the second transit hop is the cause.
#[test]
fn identify_relays_across_one_transit_hop() {
    let (mut responder, dest_hash, signing_key, announce_raw) = make_responder();
    let mut relay_a = make_transport_node();
    let mut initiator = make_initiator();

    let a_local = add_iface(&mut relay_a, "A_local_initiator", true);
    let a_mesh = add_iface(&mut relay_a, "A_mesh", false);
    let r_iface = add_iface(&mut responder, "R_mesh", false);
    let i_iface = add_iface(&mut initiator, "I_to_A", false);

    let _ = relay_a.handle_packet(InterfaceId(a_mesh), &announce_raw);
    assert_eq!(relay_a.hops_to(&dest_hash), Some(1));

    let (link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
    let request = one_packet(&out);
    let a_req = one_packet(&relay_a.handle_packet(InterfaceId(a_local), &request));
    let proof = one_packet(&responder.handle_packet(InterfaceId(r_iface), &a_req));
    let a_proof = one_packet(&relay_a.handle_packet(InterfaceId(a_mesh), &proof));
    let est = initiator.handle_packet(InterfaceId(i_iface), &a_proof);
    assert!(has_link_established(&est));

    // Deliver the initiator's RTT so the responder link goes Active (I -> A -> R).
    for rtt in action_data(&est) {
        for p in action_data(&relay_a.handle_packet(InterfaceId(a_local), &rtt)) {
            let _ = responder.handle_packet(InterfaceId(r_iface), &p);
        }
    }
    assert_eq!(
        responder.active_link_count(),
        1,
        "responder link must be active"
    );

    let identity = Identity::generate(&mut OsRng);
    let expected = *identity.hash();
    let i_ident = one_packet(
        &initiator
            .identify_link(&link_id, &identity)
            .expect("identify_link must succeed"),
    );
    let a_fwd = action_data(&relay_a.handle_packet(InterfaceId(a_local), &i_ident));
    assert!(!a_fwd.is_empty(), "relay A must forward the identify");
    let r_out = responder.handle_packet(InterfaceId(r_iface), &a_fwd[0]);

    assert_eq!(
        identified_hash(&r_out),
        Some(expected),
        "single-hop identify must reach R"
    );
}
