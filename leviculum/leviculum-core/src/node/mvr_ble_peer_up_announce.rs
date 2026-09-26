//! mvr: a new BLE peer is announced to over its own link, and over no
//! other (Codeberg #376 part 4).
//!
//! ## The field failure this reproduces
//!
//! Operator report, 2026-09-09: a stationary relay had sent no announce
//! that reached the Columba phone for an hour, so its hop count could
//! not be judged at all, while the board that had announced recently sat
//! at one hop. The cause was that a board announced its delivery
//! destination at exactly one moment — immediately before a telemetry
//! report — so a phone that connected between two reports learned
//! nothing about the board it was linked to, and a board that reports
//! rarely was effectively invisible.
//!
//! The occasion this adds is the peer-up edge: the peer finished its
//! identity handshake, it can receive, and it is precisely the node that
//! does not know us.
//!
//! ## Why it must not be a broadcast
//!
//! This is the same issue's opening symptom, one packet earlier. With
//! two boards and a phone all linked, an announce broadcast to every
//! link reaches the neighbour board, which rebroadcasts it, and the
//! relayed copy races the direct one into the phone's path table — the
//! phone then lists a board one hop away at two hops. So the peer-up
//! announce carries the #376 delivery hint
//! (`Action::SendPacket::peer`) and goes on that peer's link alone.
//! That is what these tests observe.
//!
//! ## Shape
//!
//! 1 node, 1 mock multi-peer interface, deterministic, sub-second. Two
//! peers are up on the same interface; the observable is which
//! `SendPacket` actions carry which `peer` hint.
//!
//! The WHEN — one announce per peer-up edge, at most one per identity
//! per 15 minutes, nothing without a clock — is the caller's policy and
//! is host-tested in `leviculum-nrf/announce-policy` (shared verbatim by
//! the firmware and by `lnsd`). This file pins the WHAT: given the
//! decision to announce to a peer, exactly one announce leaves, on that
//! peer's link.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction};
use crate::embedded_storage::EmbeddedStorage;
use crate::identity::Identity;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{Packet, PacketType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::Storage;
use crate::transport::{Action, InterfaceId, TickOutput};

type EmbeddedNode = NodeCore<OsRng, MockClock, EmbeddedStorage>;

const PHONE: [u8; TRUNCATED_HASHBYTES] = [0xb9; TRUNCATED_HASHBYTES];
const NEIGHBOUR: [u8; TRUNCATED_HASHBYTES] = [0x5d; TRUNCATED_HASHBYTES];

fn make_node() -> Box<EmbeddedNode> {
    NodeCoreBuilder::new().enable_transport(true).build_boxed(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        EmbeddedStorage::new(),
    )
}

fn add_ble_iface(node: &mut EmbeddedNode) -> usize {
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new("ble", 2)));
    node.set_interface_name(idx, String::from("ble"));
    idx
}

/// The node's own delivery destination, registered and announced once —
/// the state a board is in from its first telemetry report or its first
/// periodic announce onward.
fn register_own_delivery(node: &mut EmbeddedNode) -> crate::DestinationHash {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "lxmf",
        &["delivery"],
    )
    .unwrap();
    let hash = *dest.hash();
    node.register_destination(dest);
    hash
}

/// Every ANNOUNCE this output puts on the wire, as (interface, delivery
/// hint). A `Broadcast` action is reported with no interface, which is
/// what makes "it went to every link" visible instead of silent.
fn announces(out: &TickOutput) -> Vec<(Option<InterfaceId>, Option<[u8; TRUNCATED_HASHBYTES]>)> {
    out.actions
        .iter()
        .filter_map(|action| match action {
            Action::SendPacket { iface, data, peer } => Packet::unpack(data)
                .ok()
                .filter(|p| p.flags.packet_type == PacketType::Announce)
                .map(|_| (Some(*iface), *peer)),
            Action::Broadcast { data, .. } => Packet::unpack(data)
                .ok()
                .filter(|p| p.flags.packet_type == PacketType::Announce)
                .map(|_| (None, None)),
        })
        .collect()
}

/// THE batch, in one assertion: the announce a new peer gets names that
/// peer, so the neighbour board on the same interface never sees it and
/// cannot produce the relayed copy that made a one-hop board read as
/// two.
#[test]
fn mvr_a_peer_up_announce_goes_on_that_peers_link_alone() {
    let mut node = make_node();
    let ble = add_ble_iface(&mut node);
    let delivery = register_own_delivery(&mut node);

    let app_data = b"\x91\xa6LN-beef".to_vec();
    let out = node
        .announce_destination_to_peer(&delivery, Some(&app_data), ble, PHONE)
        .expect("the destination is registered");

    assert_eq!(
        announces(&out),
        std::vec![(Some(InterfaceId(ble)), Some(PHONE))],
        "exactly one announce, on the BLE interface, addressed at the phone: \
         no broadcast (the neighbour would rebroadcast it) and no second copy"
    );
}

/// The negative half, stated as its own test because it is the field
/// symptom: nothing addressed at the OTHER peer, and nothing unaddressed.
#[test]
fn mvr_a_peer_up_announce_does_not_reach_the_other_peer() {
    let mut node = make_node();
    let ble = add_ble_iface(&mut node);
    let delivery = register_own_delivery(&mut node);

    let out = node
        .announce_destination_to_peer(&delivery, None, ble, PHONE)
        .expect("the destination is registered");

    for (iface, peer) in announces(&out) {
        assert_eq!(
            peer,
            Some(PHONE),
            "an announce on {iface:?} with hint {peer:?} would reach the \
             neighbour board, which forwards it: the two-hop copy of #376"
        );
        assert_ne!(peer, Some(NEIGHBOUR));
    }
}

/// A plain interface-wide announce still has no hint: the peer-up path
/// is the only one that addresses a peer, and adding it must not have
/// narrowed `announce_destination_on_interface` to one link.
#[test]
fn mvr_an_interface_announce_still_reaches_every_peer() {
    let mut node = make_node();
    let ble = add_ble_iface(&mut node);
    let delivery = register_own_delivery(&mut node);

    let out = node
        .announce_destination_on_interface(&delivery, None, ble)
        .expect("the destination is registered");

    assert_eq!(
        announces(&out),
        std::vec![(Some(InterfaceId(ble)), None)],
        "no hint means every live link, which is what an interface-wide \
         announce is"
    );
}

/// And the broadcast announce is still a broadcast — the shape the
/// periodic announce of item 2 uses, on every interface at once.
#[test]
fn mvr_a_periodic_announce_is_still_a_broadcast() {
    let mut node = make_node();
    let _ble = add_ble_iface(&mut node);
    let delivery = register_own_delivery(&mut node);

    let out = node
        .announce_destination(&delivery, None)
        .expect("the destination is registered");

    assert_eq!(
        announces(&out),
        std::vec![(None, None)],
        "a periodic announce is for everyone that can hear us"
    );
}

/// `lnsd`'s call site: the daemon does not own an LXMF delivery
/// destination, it owns whatever destinations were registered on it, and
/// the peer-up path re-announces exactly the set interface recovery
/// re-announces — addressed at the one peer.
///
/// The count is returned rather than inferred so the daemon can log
/// "nothing was owed" instead of claiming an announce it never made.
#[test]
fn mvr_the_daemon_announces_its_own_destinations_to_the_new_peer() {
    let mut node = make_node();
    let ble = add_ble_iface(&mut node);
    let delivery = register_own_delivery(&mut node);

    // A destination that has never announced is not re-announced by the
    // interface-up path (announcing is the application's decision), so
    // give it its first announce the way an application would.
    let seed = node
        .announce_destination(&delivery, Some(b"seed"))
        .expect("registered");
    assert_eq!(announces(&seed), std::vec![(None, None)]);

    let (sent, out) = node.announce_local_destinations_to_peer(InterfaceId(ble), PHONE);

    assert_eq!(sent, 1, "one destination is held and has announced before");
    assert_eq!(
        announces(&out),
        std::vec![(Some(InterfaceId(ble)), Some(PHONE))],
        "the daemon's own destination, on the new peer's link alone"
    );
}

/// The other side of that count: a node with nothing to say says so, and
/// emits nothing. Without this the daemon would log a peer-up announce
/// on every fresh link of a node that has never announced anything.
#[test]
fn mvr_a_node_with_nothing_announced_yet_sends_no_peer_up_announce() {
    let mut node = make_node();
    let ble = add_ble_iface(&mut node);
    let _delivery = register_own_delivery(&mut node);

    let (sent, out) = node.announce_local_destinations_to_peer(InterfaceId(ble), PHONE);

    assert_eq!(sent, 0);
    assert!(
        announces(&out).is_empty(),
        "a destination that has never announced has no announce to repeat"
    );
}

/// Echo dedup survives the hint: the peer-up announce's own hash is
/// cached, so the copy a neighbour relays back to us is dropped instead
/// of being processed as a fresh announce from ourselves. The broadcast
/// path gets this from `send_on_all_interfaces`; the single-interface
/// path has to do it itself, and did before #376 — this pins that the
/// peer variant did not lose it.
#[test]
fn mvr_the_peer_up_announce_is_cached_for_echo_dedup() {
    let mut node = make_node();
    let ble = add_ble_iface(&mut node);
    let delivery = register_own_delivery(&mut node);

    let out = node
        .announce_destination_to_peer(&delivery, None, ble, PHONE)
        .expect("registered");

    let bytes = out
        .actions
        .iter()
        .find_map(|a| match a {
            Action::SendPacket { data, .. } => Some(data.clone()),
            Action::Broadcast { .. } => None,
        })
        .expect("one announce went out");
    assert!(bytes.len() <= MTU);

    let hash = crate::packet::packet_hash(&bytes);
    assert!(
        node.storage().has_packet_hash(&hash),
        "our own announce must be in the dedup cache, or a neighbour's \
         relayed copy comes back and is processed as new"
    );
}
