//! mvr: a routed packet names the peer it is for, a broadcast does not
//! (Codeberg #376 part 2).
//!
//! ## The field failure this reproduces
//!
//! Desk, 2026-09-09 10:53, two boards and a phone, every pair linked
//! over BLE. `lnflash --clear-position` armed a report on both boards.
//! The T114 queued its report on conn=1 AND conn=2 in the same
//! millisecond — the report was addressed to the phone (`send via=ble
//! next_hop=direct`), yet the other board got a copy too, forwarded it,
//! and the phone received the same report twice: double airtime per
//! report, `TRANSPORT dup=` on both boards, and a relayed copy racing
//! the direct one into the path table.
//!
//! The mechanism was the BLE interface's fan-out, which copied EVERY
//! outbound packet into every live link's queue. It cannot decide
//! otherwise on its own: only the core knows a packet's next hop, and
//! since #365 only the core knows which peer link that next hop was
//! learned on (`PathEntry::via_peer`). So the core must say, and this
//! test pins what it says.
//!
//! ## Reference semantics
//!
//! `ble-reticulum` spawns one sub-interface per peer identity
//! (`BLEInterface._spawn_peer_interface`,
//! ble-reticulum/src/ble_reticulum/BLEInterface.py:1892) and registers
//! each with `RNS.Transport.interfaces`
//! (ble-reticulum/src/ble_reticulum/BLEInterface.py:1944), exactly as
//! AutoInterface does. Python's Transport therefore already addresses
//! ONE peer: a routed packet goes to that peer's own interface and to no
//! other, and the parent's fan-out (`BLEInterface.process_outgoing`,
//! ble-reticulum/src/ble_reticulum/BLEInterface.py:2305-2333) is
//! reached only by packets Transport hands to the parent interface.
//! A Python neighbour consequently expects a packet routed to it to
//! arrive ONCE, which is the semantic this hint delivers — with one
//! shared interface plus a per-packet peer instead of one interface per
//! peer, because the firmware's interface table is fixed at boot.
//!
//! ## Shape
//!
//! 1 node, 1 mock BLE interface, deterministic, sub-second. Two peers
//! announce over the same interface; the observable is the `peer` field
//! of the emitted `SendPacket` actions.
//!
//! ## Part 3: the deferred proof
//!
//! Same issue, one packet later. A probe arriving over a BLE peer is
//! answered by a proof that is NOT generated inside
//! `process_incoming_from_peer` — the core queues
//! `TransportEvent::ProofRequested` and answers it after the incoming
//! call has returned, when `ingress_peer` is `None` again. The rig
//! (2026-09-09 15:32:13.701, T114 with lnsd as its only peer) showed
//! every proof leaving as `BLE_TX_FLOOD`. So the event carries the
//! arrival's peer, and the proof is addressed at it. A peer whose link
//! has since died is not downgraded to a flood: the interface drops it
//! and says so (`leviculum-nrf/ble-tx/src/registry.rs`,
//! `a_hint_for_a_peer_with_no_link_drops_instead_of_flooding`).

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::embedded_storage::EmbeddedStorage;
use crate::identity::Identity;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{Packet, PacketType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::transport::{Action, InterfaceId, PathEntry, TickOutput};

type EmbeddedNode = NodeCore<OsRng, MockClock, EmbeddedStorage>;

fn make_node() -> Box<EmbeddedNode> {
    NodeCoreBuilder::new().enable_transport(true).build_boxed(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        EmbeddedStorage::new(),
    )
}

fn add_iface(node: &mut EmbeddedNode, name: &'static str, id: u8) -> usize {
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new(name, id)));
    node.set_interface_name(idx, String::from(name));
    idx
}

struct Peer {
    dest_hash: crate::DestinationHash,
    dest: Destination,
}

fn make_peer(app: &'static str) -> Peer {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &[app],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    Peer { dest_hash, dest }
}

impl Peer {
    fn direct_announce(&mut self, ts: u64) -> Vec<u8> {
        let ann = self.dest.announce(None, &mut OsRng, ts, ts / 1000).unwrap();
        let mut buf = [0u8; MTU];
        let len = ann.pack(&mut buf).unwrap();
        buf[..len].to_vec()
    }
}

/// Every DATA packet for `dest`, as (interface, delivery hint).
fn data_sends(
    out: &TickOutput,
    dest: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<(InterfaceId, Option<[u8; TRUNCATED_HASHBYTES]>)> {
    out.actions
        .iter()
        .filter_map(|action| match action {
            Action::SendPacket { iface, data, peer } => Packet::unpack(data)
                .ok()
                .filter(|p| p.flags.packet_type == PacketType::Data && &p.destination_hash == dest)
                .map(|_| (*iface, *peer)),
            Action::Broadcast { .. } => None,
        })
        .collect()
}

/// THE desk scenario, one hop before the air: the phone's path was
/// learned on the phone's BLE link, so the report for the phone names
/// the phone — and nothing else on that interface is addressed.
#[test]
fn a_report_routed_over_a_ble_link_names_that_links_peer() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);

    let mut phone = make_peer("phone");
    let phone_link = [0xB9u8; TRUNCATED_HASHBYTES];
    let _ = node.handle_packet_from_peer(
        InterfaceId(ble),
        phone_link,
        &phone.direct_announce(TEST_TIME_MS),
    );

    let (_, out) = node
        .send_single_packet(&phone.dest_hash, b"position report")
        .expect("send with a live path");

    let sends = data_sends(&out, phone.dest_hash.as_bytes());
    assert_eq!(sends.len(), 1, "one report, one send action");
    assert_eq!(sends[0].0, InterfaceId(ble));
    assert_eq!(
        sends[0].1,
        Some(phone_link),
        "the report must name the peer link its path was learned on; \
         without the hint the interface copies it onto the other board's \
         link too, which forwards it back to the phone (#376 desk log \
         2026-09-09 10:53)"
    );
}

/// The hint is READ from the path, never invented: a path learned
/// without a named peer — every single-peer interface, and a BLE packet
/// that arrived before the identity handshake — routes with no hint, and
/// the interface falls back to its broadcast fan-out.
#[test]
fn a_path_learned_without_a_peer_routes_without_a_hint() {
    let mut node = make_node();
    let lora = add_iface(&mut node, "lora_sx1262", 1);

    let mut peer = make_peer("lorapeer");
    // `handle_packet`, not `handle_packet_from_peer`: no peer link named.
    let _ = node.handle_packet(InterfaceId(lora), &peer.direct_announce(TEST_TIME_MS));

    let (_, out) = node
        .send_single_packet(&peer.dest_hash, b"position report")
        .expect("send with a live path");

    let sends = data_sends(&out, peer.dest_hash.as_bytes());
    assert_eq!(sends.len(), 1);
    assert_eq!(
        sends[0].1, None,
        "a path with no via_peer must not manufacture one"
    );
}

/// Two peers on ONE broadcast domain: each destination's report names
/// its own link. This is the property the fan-out needs — a hint that
/// named the interface rather than the peer would be useless here.
#[test]
fn each_destination_names_its_own_peer_link_on_the_same_interface() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);

    let mut phone = make_peer("phone");
    let mut board = make_peer("board");
    let phone_link = [0xB9u8; TRUNCATED_HASHBYTES];
    let board_link = [0xC7u8; TRUNCATED_HASHBYTES];
    let _ = node.handle_packet_from_peer(
        InterfaceId(ble),
        phone_link,
        &phone.direct_announce(TEST_TIME_MS),
    );
    let _ = node.handle_packet_from_peer(
        InterfaceId(ble),
        board_link,
        &board.direct_announce(TEST_TIME_MS),
    );

    let (_, to_phone) = node
        .send_single_packet(&phone.dest_hash, b"for the phone")
        .expect("path to the phone");
    let (_, to_board) = node
        .send_single_packet(&board.dest_hash, b"for the board")
        .expect("path to the board");

    assert_eq!(
        data_sends(&to_phone, phone.dest_hash.as_bytes())[0].1,
        Some(phone_link)
    );
    assert_eq!(
        data_sends(&to_board, board.dest_hash.as_bytes())[0].1,
        Some(board_link)
    );
}

/// An announce is a broadcast and carries no hint, whatever the path
/// table holds: every peer on the medium is its audience. The
/// per-interface announce emission is a `SendPacket` (the airtime-cap
/// path splits a Broadcast into one action per interface), so the
/// absence of a hint has to be pinned on the action, not on the variant.
#[test]
fn an_announce_carries_no_delivery_hint() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);

    // A live BLE peer, so a hint COULD be found if the code looked for
    // one in the wrong place.
    let mut phone = make_peer("phone");
    let phone_link = [0xB9u8; TRUNCATED_HASHBYTES];
    let _ = node.handle_packet_from_peer(
        InterfaceId(ble),
        phone_link,
        &phone.direct_announce(TEST_TIME_MS),
    );

    let mut own = make_peer("ownapp");
    let out = node.handle_packet(InterfaceId(ble), &own.direct_announce(TEST_TIME_MS + 1_000));

    let hinted: Vec<_> = out
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::SendPacket { peer, .. } => *peer,
            Action::Broadcast { .. } => None,
        })
        .collect();
    assert!(
        hinted.is_empty(),
        "a relayed announce must reach every peer on the medium, so no \
         SendPacket for it may carry a delivery hint (got {hinted:?})"
    );
}

/// Every proof packet in this output, as (interface, delivery hint).
fn proof_sends(out: &TickOutput) -> Vec<(InterfaceId, Option<[u8; TRUNCATED_HASHBYTES]>)> {
    out.actions
        .iter()
        .filter_map(|action| match action {
            Action::SendPacket { iface, data, peer } => Packet::unpack(data)
                .ok()
                .filter(|p| p.flags.packet_type == PacketType::Proof)
                .map(|_| (*iface, *peer)),
            Action::Broadcast { .. } => None,
        })
        .collect()
}

/// A node that proves what it receives, and a sender that can address it.
///
/// The receiver has ONE interface and NO path back to the sender: the
/// only thing it knows about where the probe came from is the arrival
/// itself, which is exactly the field situation (a phone probing a board
/// it has a link to).
struct Probe {
    receiver: Box<EmbeddedNode>,
    ble: usize,
    raw: Vec<u8>,
}

fn make_probe() -> Probe {
    let mut receiver = make_node();
    let ble = add_iface(&mut receiver, "ble_nrf", 0);

    let recv_identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(recv_identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["probe"],
    )
    .unwrap();
    // What a Columba telemetry destination uses: the node proves every
    // packet itself, without asking the application.
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    let recv_pub = dest
        .identity()
        .expect("the receiver destination owns an identity")
        .public_key_bytes();
    receiver.register_destination(dest);

    // The sender is scaffolding: it exists only to produce one correctly
    // encrypted probe addressed at the receiver.
    let mut sender = NodeCoreBuilder::new().build_boxed(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        EmbeddedStorage::new(),
    );
    let sender_iface = add_iface(&mut sender, "S_mesh", 9);
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
    sender.remember_identity(
        dest_hash,
        Identity::from_public_key_bytes(&recv_pub).unwrap(),
    );
    sender.register_destination(
        Destination::new(
            Some(Identity::from_public_key_bytes(&recv_pub).unwrap()),
            Direction::Out,
            DestinationType::Single,
            "mvrapp",
            &["probe"],
        )
        .unwrap(),
    );

    let (_, out) = sender
        .send_single_packet(&dest_hash, b"telemetry probe")
        .expect("the scaffolding sender has a path");
    let raw = out
        .actions
        .iter()
        .map(|a| match a {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data.clone(),
        })
        .next()
        .expect("the probe must have been emitted");

    Probe { receiver, ble, raw }
}

/// The proof answering a probe leaves addressed at the peer the probe
/// arrived from — even though it is generated AFTER
/// `process_incoming_from_peer` returned and `ingress_peer` is `None`
/// again.
///
/// Rig signature this pins: with lnsd as the T114's only peer, the proof
/// for a probe leaves as `BLE_TX_ROUTE peer=<lnsd> conn=… len=115`. Before
/// this, every proof was a `BLE_TX_FLOOD` (rig, 2026-09-09 15:32:13.701)
/// — invisible with one link, a relayed duplicate with a neighbour board
/// on the medium.
#[test]
fn a_deferred_proof_names_the_peer_the_probe_arrived_from() {
    let mut probe = make_probe();
    let phone_link = [0xB9u8; TRUNCATED_HASHBYTES];

    let out =
        probe
            .receiver
            .handle_packet_from_peer(InterfaceId(probe.ble), phone_link, &probe.raw);

    let proofs = proof_sends(&out);
    assert_eq!(proofs.len(), 1, "one probe, one proof: {proofs:?}");
    assert_eq!(proofs[0].0, InterfaceId(probe.ble));
    assert_eq!(
        proofs[0].1,
        Some(phone_link),
        "the proof must name the peer the probe arrived from. Proving is \
         deferred (the queued TransportEvent::ProofRequested is answered \
         after process_incoming returned), so the peer has to be carried \
         with the event; without it the proof floods every live link and a \
         neighbour board forwards it back (#376)"
    );
}

/// The hint is the ARRIVAL's, not a path's: the receiver has no path
/// entry for the prober at all, and still addresses the proof.
#[test]
fn the_proof_hint_survives_having_no_path_to_the_prober() {
    let mut probe = make_probe();
    let phone_link = [0xB9u8; TRUNCATED_HASHBYTES];

    let out =
        probe
            .receiver
            .handle_packet_from_peer(InterfaceId(probe.ble), phone_link, &probe.raw);

    let proof_dest = out
        .actions
        .iter()
        .find_map(|a| match a {
            Action::SendPacket { data, .. } => Packet::unpack(data)
                .ok()
                .filter(|p| p.flags.packet_type == PacketType::Proof)
                .map(|p| p.destination_hash),
            Action::Broadcast { .. } => None,
        })
        .expect("a proof must have been emitted");
    assert!(
        probe.receiver.transport.path(&proof_dest).is_none(),
        "the proof's destination (the truncated packet hash) has no path \
         entry, so the hint cannot have come from the path table"
    );
    assert_eq!(proof_sends(&out)[0].1, Some(phone_link));
}

/// Negative control: a probe that arrives without a named peer (every
/// single-peer interface, and a BLE packet received before the identity
/// handshake) is proved without a hint — the core never invents one.
#[test]
fn a_probe_without_a_peer_is_proved_without_a_hint() {
    let mut probe = make_probe();

    let out = probe
        .receiver
        .handle_packet(InterfaceId(probe.ble), &probe.raw);

    let proofs = proof_sends(&out);
    assert_eq!(proofs.len(), 1, "one probe, one proof: {proofs:?}");
    assert_eq!(
        proofs[0].1, None,
        "an arrival with no peer must not manufacture one"
    );
}
