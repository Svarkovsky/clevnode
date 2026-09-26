//! mvr: an application proof must leave on the interface the packet came in on.
//!
//! `ProofStrategy::App` hands proof emission to the application:
//! `NodeEvent::PacketProofRequested` now carries the ingress
//! `interface_index` (and, since #376, the ingress peer), and
//! `send_proof_on_peer` mirrors Python's
//! `packet.prove()`, which routes the proof over the packet's
//! `receiving_interface` instead of the path table. On a multi-interface
//! node the distinction is load-bearing: the prover has NO path-table entry
//! for the anonymous sender, so only ingress-interface routing can return
//! the proof to where the sender actually is.
//!
//! Named failure mode this guards: a two-interface node receives a packet on
//! interface 1 and emits the app proof on interface 0 (or broadcasts it),
//! so the sender never sees its delivery confirmed.
//!
//! Topology: sans-I/O sender <-> receiver; the receiver has two interfaces
//! and the packet is delivered on the SECOND one (index 1).

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::transport::{Action, InterfaceId, PathEntry, TickOutput};

type StoredNode = NodeCore<OsRng, MockClock, MemoryStorage>;

fn add_iface(node: &mut StoredNode, name: &'static str, id: u8) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, id)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn first_packet(output: &TickOutput) -> Vec<u8> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .next()
        .expect("expected an outbound packet")
}

/// A packet received on the receiver's SECOND interface must surface its
/// ingress `interface_index` in `PacketProofRequested`, and the app proof
/// sent via `send_proof_on_interface` must leave as a `SendPacket` on that
/// exact interface -- no broadcast, no other interface -- and confirm
/// delivery at the sender.
#[test]
fn app_proof_leaves_on_ingress_interface() {
    // Receiver: ProofStrategy::App destination, TWO interfaces, no path table.
    let recv_identity = Identity::generate(&mut OsRng);
    let mut receiver = NodeCoreBuilder::new().build(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        MemoryStorage::with_defaults(),
    );
    let mut dest = Destination::new(
        Some(recv_identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["ingressproof"],
    )
    .unwrap();
    dest.set_proof_strategy(ProofStrategy::App);
    let dest_hash = *dest.hash();
    receiver.register_destination(dest);

    let iface_a = add_iface(&mut receiver, "R_lan", 1);
    let iface_b = add_iface(&mut receiver, "R_lora", 2);
    assert_ne!(iface_a, iface_b);

    // Sender: knows the receiver's identity and a path via its own interface.
    let mut sender = NodeCoreBuilder::new().build(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        MemoryStorage::with_defaults(),
    );
    let sender_iface = add_iface(&mut sender, "S_mesh", 3);
    sender.transport.insert_path(
        dest_hash.into_bytes(),
        PathEntry {
            hops: 1,
            expires_ms: u64::MAX,
            interface_index: sender_iface,
            random_blobs: Vec::new(),
            next_hop: None,
            via_peer: None,
        },
    );
    let recv_pub = receiver
        .destination(&dest_hash)
        .unwrap()
        .identity()
        .unwrap()
        .public_key_bytes();
    sender.remember_identity(
        dest_hash,
        Identity::from_public_key_bytes(&recv_pub).unwrap(),
    );
    let sender_side_dest = Destination::new(
        Some(Identity::from_public_key_bytes(&recv_pub).unwrap()),
        Direction::Out,
        DestinationType::Single,
        "mvrapp",
        &["ingressproof"],
    )
    .unwrap();
    sender.register_destination(sender_side_dest);

    let (_receipt_hash, out) = sender
        .send_single_packet(&dest_hash, b"ingress proof probe")
        .unwrap();
    let raw = first_packet(&out);

    // Deliver on the SECOND receiver interface.
    let recv_out = receiver.handle_packet(InterfaceId(iface_b), &raw);
    let (packet_hash, event_dest, event_iface, event_peer) = recv_out
        .events
        .iter()
        .find_map(|e| match e {
            NodeEvent::PacketProofRequested {
                packet_hash,
                destination_hash,
                interface_index,
                peer,
            } => Some((*packet_hash, *destination_hash, *interface_index, *peer)),
            _ => None,
        })
        .expect("ProofStrategy::App must emit PacketProofRequested");
    assert_eq!(
        event_iface, iface_b,
        "PacketProofRequested must carry the ingress interface index"
    );
    assert_eq!(
        event_peer, None,
        "the packet arrived through `handle_packet`, which names no peer, \
         so the event must not manufacture a #376 delivery hint"
    );

    // App proves on the reported ingress interface, with the peer the same
    // event reported (none here).
    let proof_out = receiver
        .send_proof_on_peer(&packet_hash, &event_dest, event_iface, event_peer)
        .expect("send_proof_on_peer must succeed without a path entry");

    let mut proof_raw = None;
    for action in &proof_out.actions {
        match action {
            Action::SendPacket { iface, data, .. } => {
                assert_eq!(
                    *iface,
                    InterfaceId(iface_b),
                    "the app proof must leave on the ingress interface"
                );
                proof_raw = Some(data.clone());
            }
            Action::Broadcast { .. } => {
                panic!("the app proof must be targeted, not broadcast on all interfaces")
            }
        }
    }
    let proof_raw = proof_raw.expect("send_proof_on_interface must produce a SendPacket");

    // Round-trip: the sender accepts the proof as delivery confirmation.
    let sender_out = sender.handle_packet(InterfaceId(0), &proof_raw);
    assert!(
        sender_out
            .events
            .iter()
            .any(|e| matches!(e, NodeEvent::PacketDeliveryConfirmed { .. })),
        "the proof emitted on the ingress interface must confirm delivery.\nevents: {:?}",
        sender_out.events
    );
}
