//! mvr: deterministic reproduction of #124 — a link whose ONLY inbound is
//! validated PROOF packets froze its activity clock (`last_inbound`), was marked
//! Stale by `check_stale_links`, and was closed mid-flow while valid proofs kept
//! arriving.
//!
//! Trigger (our code): a node streaming CHANNEL data to a peer that proves every
//! packet. The sender's only inbound is the peer's PROOF packets — there is no
//! REQ/DATA to refresh `last_inbound` on another path and mask the bug (a
//! resource transfer is a worse vehicle for exactly that reason: the sender also
//! receives REQ DATA that hides the freeze). Before the fix `handle_data_proof`
//! validated the proof but never called `record_inbound` / `try_recover_stale`,
//! so the activity clock froze at establishment.
//!
//! Reference-first: RNS `Packet.py` sets `last_proof` on a validated proof and
//! `Link.py`'s stale check uses it, so a proof counts as activity there. The fix
//! refreshes `record_inbound` and runs `try_recover_stale` on the validated-proof
//! branch of both `handle_data_proof` and `handle_resource_proof`.
//!
//! Sans-I/O: direct initiator <-> responder over one mesh hop, `MockClock`
//! advanced to drive `check_stale_links` without sleeping the real timeout.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::{LinkCloseReason, LinkId, LinkState};
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, NoStorage};
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

/// All bytes a node wants to put on the wire this step (SendPacket + Broadcast).
fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

/// Deliver every packet to `target` and collect everything it emits in return.
fn deliver_all(target: &mut EndpointNode, iface: usize, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for pkt in packets {
        out.extend(action_data(&target.handle_packet(InterfaceId(iface), &pkt)));
    }
    out
}

fn has_link_closed(events: &[NodeEvent], reason: LinkCloseReason) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            NodeEvent::LinkClosed { reason: r, .. } if *r == reason
        )
    })
}

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
        &["proof"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    (node, dest_hash, signing_key)
}

fn make_initiator() -> EndpointNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().build(OsRng, clock, NoStorage)
}

/// Drive a clean initiator <-> responder link to Active on BOTH sides (no
/// re-key). Returns `(initiator, responder, i_iface, r_iface, caller_link_id)`.
fn establish() -> (EndpointNode, EndpointNode, usize, usize, LinkId) {
    let (mut responder, dest_hash, signing_key) = make_responder();
    let mut initiator = make_initiator();
    let r_iface = add_iface(&mut responder, "R_mesh");
    let i_iface = add_iface(&mut initiator, "I_mesh");

    let (caller_link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);

    // Ping-pong the handshake (request -> proof -> rtt -> ack) until quiescent.
    let mut for_responder = action_data(&out);
    for _ in 0..8 {
        if for_responder.is_empty() {
            break;
        }
        let back = deliver_all(&mut responder, r_iface, for_responder);
        for_responder = deliver_all(&mut initiator, i_iface, back);
    }

    assert_eq!(
        initiator.active_link_count(),
        1,
        "precondition: initiator link must be active"
    );
    assert_eq!(
        responder.active_link_count(),
        1,
        "precondition: responder link must be active"
    );
    (initiator, responder, i_iface, r_iface, caller_link_id)
}

/// One streaming round: the initiator sends a CHANNEL message, the responder
/// proves it, and the proof is delivered back to the initiator. The initiator's
/// only inbound in the round is that PROOF. Returns the initiator's events from
/// handling the proof.
fn stream_and_prove(
    initiator: &mut EndpointNode,
    responder: &mut EndpointNode,
    i_iface: usize,
    r_iface: usize,
    link_id: &LinkId,
    msg: &[u8],
) -> Vec<NodeEvent> {
    let out = initiator
        .send_on_link(link_id, msg)
        .expect("send_on_link must succeed on an active link");

    // Responder receives the channel packet and returns a PROOF.
    let proofs = deliver_all(responder, r_iface, action_data(&out));
    assert!(
        !proofs.is_empty(),
        "responder must return a proof for the channel packet"
    );

    // Deliver the proof(s) to the initiator; that is its ONLY inbound this round.
    let mut events = Vec::new();
    for pkt in proofs {
        events.extend(initiator.handle_packet(InterfaceId(i_iface), &pkt).events);
    }
    events
}

// ----------------------------------------------------------------------------
// Test
// ----------------------------------------------------------------------------

/// A link whose only inbound is validated PROOF packets must stay Active while
/// the proofs keep arriving. The near-zero RTT of the mock handshake gives
/// `stale_time ~= 10s` and `close ~= 15s`, so advancing in 6s steps and proving
/// one channel packet per step keeps the link alive with the fix; without it the
/// activity clock freezes at establishment and the link goes Stale (step 2) then
/// closes (step 3).
#[test]
fn proof_only_inbound_keeps_link_alive() {
    let (mut initiator, mut responder, i_iface, r_iface, link_id) = establish();

    let step_ms = 6_000u64;
    let mut all_events: Vec<NodeEvent> = Vec::new();

    for round in 0..5u64 {
        // Stream one channel message and let the responder prove it. The proof
        // is the initiator's only inbound this round.
        all_events.extend(stream_and_prove(
            &mut initiator,
            &mut responder,
            i_iface,
            r_iface,
            &link_id,
            b"stream",
        ));

        // Advance the initiator's clock and run its stale watchdog.
        let now = initiator.transport().clock().now_ms();
        initiator.transport().clock().set(now + step_ms);
        all_events.extend(initiator.handle_timeout().events);

        // The proof must count as activity, so the link stays Active.
        let state = initiator.link(&link_id).map(|l| l.state());
        assert_eq!(
            state,
            Some(LinkState::Active),
            "round {round}: proof-only inbound must keep the link Active, got {state:?}\nevents: {all_events:?}"
        );
    }

    assert!(
        !has_link_closed(&all_events, LinkCloseReason::Stale),
        "a proof-refreshed link must never be closed as Stale.\nevents: {all_events:?}"
    );
    assert_eq!(
        initiator.active_link_count(),
        1,
        "the link must still be Active after a proof-only stream"
    );
}

/// L-0019: the sender-side RESOURCE completion proof must refresh the
/// activity clock too. The #124 fix's resource arm sat on `Ok(_)` in
/// `handle_resource_proof` — but `OutgoingResource::handle_proof` returns
/// only `Ok(Complete)` or `Err`, so the live success path never recorded
/// activity and a link whose last inbound is that proof reads as idle
/// since the preceding REQ.
///
/// Reference: RNS `Link.receive()` (Link.py:972-984) sets
/// `self.last_inbound = time.time()` for every link-associated packet —
/// RESOURCE_PRF included — and flips a STALE link back to ACTIVE.
#[test]
fn resource_completion_proof_refreshes_activity_clock() {
    let (mut initiator, mut responder, i_iface, r_iface, link_id) = establish();

    responder
        .set_resource_strategy(&link_id, crate::resource::ResourceStrategy::AcceptApp)
        .expect("responder link must accept a strategy");
    let data = std::vec![0xCDu8; 4096];
    let (_resource_hash, out) = initiator
        .send_resource(&link_id, &data, None, false)
        .expect("send_resource must advertise");

    // ADV to the responder; the app-accept starts the transfer (first REQ).
    let mut for_initiator = deliver_all(&mut responder, r_iface, action_data(&out));
    for_initiator.extend(action_data(
        &responder
            .accept_resource(&link_id)
            .expect("accept_resource must start the incoming transfer"),
    ));

    // Shuttle to completion. The initiator's clock jumps before every
    // delivery round, so its link's `last_inbound` afterwards carries the
    // timestamp of the LAST inbound packet — which, in a completing
    // transfer, is the resource completion proof.
    let mut completed = false;
    let mut last_delivery_secs = 0u64;
    for _ in 0..64 {
        if for_initiator.is_empty() {
            break;
        }
        initiator.transport().clock().advance(7_000);
        last_delivery_secs = initiator.transport().clock().now_ms() / 1_000;
        let mut for_responder = Vec::new();
        for pkt in for_initiator.drain(..) {
            let out = initiator.handle_packet(InterfaceId(i_iface), &pkt);
            completed |= out.events.iter().any(|e| {
                matches!(
                    e,
                    NodeEvent::ResourceCompleted {
                        is_sender: true,
                        ..
                    }
                )
            });
            for_responder.extend(action_data(&out));
        }
        for_initiator = deliver_all(&mut responder, r_iface, for_responder);
    }
    assert!(
        completed,
        "positive control: the transfer must complete on the sender"
    );

    let last_inbound = initiator
        .link(&link_id)
        .expect("link must survive a completed transfer")
        .last_inbound_secs();
    assert_eq!(
        last_inbound,
        last_delivery_secs,
        "L-0019: the resource completion proof must refresh last_inbound \
         (froze {}s before the proof arrived)",
        last_delivery_secs.saturating_sub(last_inbound)
    );
}
