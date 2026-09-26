//! mvr: the three `ResourceStrategy` arms of `handle_resource_advertisement`.
//!
//! An application Resource ADV (no request/response correlation) is dispatched
//! by the receiving link's strategy, mirroring Python `Link.ACCEPT_ALL` /
//! `ACCEPT_APP` / `ACCEPT_NONE`:
//!
//! - `AcceptAll`: the transfer starts immediately (REQ packet back to the
//!   sender, `ResourceTransferStarted { is_sender: false }`).
//! - `AcceptApp`: the ADV is parked for the application
//!   (`ResourceAdvertised`, no transfer until `accept_resource`).
//! - `AcceptNone` (the default): the ADV is rejected SILENTLY -- no event,
//!   no outbound packet, and a later `accept_resource` has nothing to accept.
//!
//! Each arm gets its own direct assertion; before this module the dispatch
//! was only crossed incidentally by transfer round-trip tests.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::LinkId;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::resource::{ResourceError, ResourceStrategy};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::NoStorage;
use crate::transport::{Action, InterfaceId, TickOutput};

type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

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

fn deliver_all(target: &mut EndpointNode, iface: usize, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for pkt in packets {
        out.extend(action_data(&target.handle_packet(InterfaceId(iface), &pkt)));
    }
    out
}

/// Establish sender <-> receiver, set `strategy` on the receiver side, and
/// deliver one application Resource ADV to the receiver. Returns the
/// receiver's `TickOutput` for the ADV packet plus its link id.
fn deliver_adv_with_strategy(
    strategy: ResourceStrategy,
) -> (EndpointNode, crate::transport::TickOutput, LinkId) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let mut receiver = NodeCoreBuilder::new().build(OsRng, MockClock::new(TEST_TIME_MS), NoStorage);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrstrat",
        &["adv"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    receiver.register_destination(dest);
    let r_iface = add_iface(&mut receiver, "R_mesh");

    let mut sender = NodeCoreBuilder::new().build(OsRng, MockClock::new(TEST_TIME_MS), NoStorage);
    let s_iface = add_iface(&mut sender, "S_mesh");

    let (sender_link, _routed, out) = sender.connect(dest_hash, &signing_key);
    let mut receiver_link = None;
    let mut for_receiver = action_data(&out);
    for _ in 0..8 {
        if for_receiver.is_empty() {
            break;
        }
        let mut back = Vec::new();
        for pkt in for_receiver {
            let o = receiver.handle_packet(InterfaceId(r_iface), &pkt);
            for ev in &o.events {
                if let NodeEvent::LinkEstablished { link_id, .. } = ev {
                    receiver_link = Some(*link_id);
                }
            }
            back.extend(action_data(&o));
        }
        for_receiver = deliver_all(&mut sender, s_iface, back);
    }
    let receiver_link = receiver_link.expect("receiver side must reach Active");

    receiver
        .set_resource_strategy(&receiver_link, strategy)
        .expect("strategy applies to the active link");

    // A plain application resource: no request/response flags involved.
    let payload: Vec<u8> = (0..3000usize).map(|i| (i % 251) as u8).collect();
    let (_hash, out) = sender
        .send_resource(&sender_link, &payload, None, false)
        .expect("sender advertises the resource");
    let adv_pkts = action_data(&out);
    assert_eq!(adv_pkts.len(), 1, "one ADV packet expected");

    let adv_out = receiver.handle_packet(InterfaceId(r_iface), &adv_pkts[0]);
    (receiver, adv_out, receiver_link)
}

/// `AcceptAll` starts the transfer immediately: REQ back to the sender and
/// `ResourceTransferStarted { is_sender: false }`.
#[test]
fn accept_all_starts_transfer_immediately() {
    let (_receiver, adv_out, _link) = deliver_adv_with_strategy(ResourceStrategy::AcceptAll);

    assert!(
        adv_out.events.iter().any(|e| matches!(
            e,
            NodeEvent::ResourceTransferStarted {
                is_sender: false,
                ..
            }
        )),
        "AcceptAll must start the incoming transfer.\nevents: {:?}",
        adv_out.events
    );
    assert!(
        !adv_out.actions.is_empty(),
        "AcceptAll must answer the ADV with a resource REQ packet"
    );
}

/// `AcceptApp` parks the ADV for the application: `ResourceAdvertised` with
/// the advertised sizes, no transfer until `accept_resource` starts it.
#[test]
fn accept_app_defers_to_application() {
    let (mut receiver, adv_out, link) = deliver_adv_with_strategy(ResourceStrategy::AcceptApp);

    let advertised = adv_out.events.iter().find_map(|e| match e {
        NodeEvent::ResourceAdvertised {
            transfer_size,
            data_size,
            ..
        } => Some((*transfer_size, *data_size)),
        _ => None,
    });
    let (transfer_size, data_size) = advertised.expect("AcceptApp must emit ResourceAdvertised");
    assert!(transfer_size > 0 && data_size > 0, "advertised sizes set");
    assert!(
        !adv_out
            .events
            .iter()
            .any(|e| matches!(e, NodeEvent::ResourceTransferStarted { .. })),
        "AcceptApp must not start the transfer before the app accepts.\nevents: {:?}",
        adv_out.events
    );
    assert!(
        adv_out.actions.is_empty(),
        "AcceptApp must not answer the ADV before the app accepts"
    );

    // The parked ADV is live: accepting it starts the transfer.
    let accept_out = receiver
        .accept_resource(&link)
        .expect("accept_resource consumes the parked ADV");
    assert!(
        accept_out.events.iter().any(|e| matches!(
            e,
            NodeEvent::ResourceTransferStarted {
                is_sender: false,
                ..
            }
        )),
        "accept_resource must start the parked transfer.\nevents: {:?}",
        accept_out.events
    );
}

/// `AcceptNone` rejects silently: no event, no outbound packet, and nothing
/// parked for a later `accept_resource`.
#[test]
fn accept_none_silently_rejects() {
    let (mut receiver, adv_out, link) = deliver_adv_with_strategy(ResourceStrategy::AcceptNone);

    assert!(
        adv_out.events.is_empty(),
        "AcceptNone must reject the ADV without any event.\nevents: {:?}",
        adv_out.events
    );
    assert!(
        adv_out.actions.is_empty(),
        "AcceptNone must reject the ADV without any outbound packet"
    );
    match receiver.accept_resource(&link) {
        Err(ResourceError::NoPendingResource) => {}
        Err(other) => panic!("expected NoPendingResource (nothing parked), got {other}"),
        Ok(_) => panic!("AcceptNone must not park the ADV for a later accept"),
    }
}
