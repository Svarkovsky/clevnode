//! mvr: a relay whose next hop toward D IS the requestor swallows the
//! path request (Codeberg #365, walk 2 and walk 5) — and the peer-up
//! pull recovers the cell.
//!
//! ## The field failure this reproduces
//!
//! Walk 2 (2026-09-04, ledger `365.md`): the Pocket, rebooted out of
//! BLE range, issued 20 path requests for the phone over LoRa, one per
//! minute, all received by the T114 (rx +24) — and the T114 forwarded
//! none (fwd 0) and answered none. The T114 held the phone's
//! destinations via the POCKET (learned from the Pocket's own LoRa
//! rebroadcast, next_hop = Pocket), so the requestor-is-next-hop guard
//! refused the answer — correctly, that route leads straight back
//! through the requestor — and then nothing else happened: the request
//! died at the relay. The cell stayed dark until the phone happened to
//! announce again (16:13). Walk 5 (2026-09-06/07, boards 28de836, no
//! reboot) matches the same signature: after the Pocket's BLE loss,
//! nothing over LoRa.
//!
//! The state is reachable without any reboot: the #365 cull itself
//! removes the relay's DIRECT entry when the phone's BLE link to the
//! relay flaps, Columba does not re-announce on reconnect (walk-2
//! finding, #255 comment), and the phone's next announce can reach the
//! relay only as the Pocket's LoRa rebroadcast — installed into the
//! void as next_hop = Pocket (`handle_announce`, new-destination arm).
//!
//! ## Reference semantics, and the closure
//!
//! Python has the identical dead end, and knows it: the
//! requestor-is-next-hop branch (Transport.py:2958-2966) drops the
//! request with a TODO — "Doing path invalidation here would decrease
//! the network convergence time. Maybe just drop it?" — and the elif
//! chain means the discovery/forward branches are unreachable while any
//! path entry exists. Our `handle_path_request` mirrors that shape
//! (transport.rs, `next_hop_is_requestor` early return before cases
//! 2b/3), and this mvr pins that the guard itself stays untouched.
//!
//! The recovery goes around the guard instead of through it: the M1
//! measurement (ledger 365, 2026-09-07) showed Columba answers a path
//! request for its own delivery destination in under a second, so the
//! relay can PULL the phone's announce over its own BLE link the moment
//! the peer comes up (`NodeCore::handle_interface_peer_up`; mechanism
//! pinned in `mvr_peer_up_pull`). The direct announce that comes back
//! displaces the stale via-the-requestor route (fewer hops win), and
//! the next path request from behind the relay is answered normally.
//! Wire-compatible both ways: the pull is the ordinary 48-byte path
//! request, the answer the ordinary announce.
//!
//! Since the 2026-09-08 batch the pull is OPT-IN
//! (`NodeCoreBuilder::peer_up_pull_names`, default empty): against
//! Columba the handshake identity is the phone's transport identity,
//! not its LXMF identity, so the derived destination does not exist
//! there — see `mvr_peer_up_pull` for the citations. This file builds
//! its nodes with the pull configured, which is sound for the trap it
//! walks: the peer whose announce closes the chain is modelled as a
//! single-identity node (handshake identity == LXMF identity), the
//! shape our own boards and lnsd present, for which the pull derivation
//! is correct.
//!
//! ## Shape
//!
//! 2 nodes (both the boards' exact storage type), deterministic,
//! sub-second. A (the Pocket) learns D direct on BLE, loses the peer,
//! culls, and emits the real 48-byte path request. R (the T114) holds D
//! via A. The request is piped into R and dies in the guard (pinned
//! red-era behaviour, still asserted); then the phone's peer-up lands
//! on R's BLE interface, R pulls, the phone's direct announce comes
//! back, and A's NEXT request IS answered. The control pins the same
//! harness green with the healthy relay state (D held direct), so the
//! answer detection is the rig's, not the fix's.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{
    MTU, PATH_REQUEST_GRACE_MS, PATH_REQUEST_MIN_INTERVAL_MS, TRUNCATED_HASHBYTES,
};
use crate::destination::{Destination, DestinationType, Direction};
use crate::embedded_storage::EmbeddedStorage;
use crate::identity::Identity;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{HeaderType, Packet, PacketType, TransportType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, Storage};
use crate::transport::{Action, InterfaceId, TickOutput};

/// Both boards' exact shape: `EmbeddedStorage`, transport enabled.
type EmbeddedNode = NodeCore<OsRng, MockClock, EmbeddedStorage>;

/// Pull configured (opt-in since 2026-09-08, see the module doc): this
/// file pins the single-identity-fleet recovery.
fn make_node() -> Box<EmbeddedNode> {
    NodeCoreBuilder::new()
        .enable_transport(true)
        .peer_up_pull_names(std::vec![String::from("lxmf.delivery")])
        .build_boxed(OsRng, MockClock::new(TEST_TIME_MS), EmbeddedStorage::new())
}

fn add_iface(node: &mut EmbeddedNode, name: &'static str, id: u8) -> usize {
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new(name, id)));
    node.set_interface_name(idx, String::from(name));
    idx
}

/// The phone: identity + its `lxmf.delivery` destination + announce
/// packers. The real aspect matters here — the peer-up pull derives
/// exactly this destination from the identity hash, so the field chain
/// only closes if the mvr's phone carries the name the pull guesses.
struct Peer {
    identity_hash: [u8; TRUNCATED_HASHBYTES],
    dest_hash: crate::DestinationHash,
    dest: Destination,
}

fn make_peer() -> Peer {
    let identity = Identity::generate(&mut OsRng);
    let identity_hash = *identity.hash();
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "lxmf",
        &["delivery"],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    Peer {
        identity_hash,
        dest_hash,
        dest,
    }
}

impl Peer {
    /// A direct (wire hops 0) announce emitted at `ts`.
    fn direct_announce(&mut self, ts: u64) -> Vec<u8> {
        let ann = self.dest.announce(None, &mut OsRng, ts, ts / 1000).unwrap();
        let mut buf = [0u8; MTU];
        let len = ann.pack(&mut buf).unwrap();
        buf[..len].to_vec()
    }

    /// The same announce one relay later: HEADER_2, wire hops 1, the
    /// relay's identity hash as `transport_id`.
    fn relayed_announce(&mut self, ts: u64, via: [u8; TRUNCATED_HASHBYTES]) -> Vec<u8> {
        let raw = self.direct_announce(ts);
        let mut p = Packet::unpack(&raw).unwrap();
        p.flags.header_type = HeaderType::Type2;
        p.flags.transport_type = TransportType::Transport;
        p.hops = 1;
        p.transport_id = Some(via);
        let mut buf = [0u8; MTU];
        let len = p.pack(&mut buf).unwrap();
        buf[..len].to_vec()
    }
}

/// Raw bytes of every action in this output, tagged with the send
/// target (`None` = broadcast).
fn wire_out(out: &TickOutput) -> Vec<(Option<InterfaceId>, Vec<u8>)> {
    out.actions
        .iter()
        .map(|action| match action {
            Action::SendPacket { iface, data, .. } => (Some(*iface), data.clone()),
            Action::Broadcast { data, .. } => (None, data.clone()),
        })
        .collect()
}

/// Announces naming `dest` in this output — the shape of a path answer.
fn announces_for(out: &TickOutput, dest: &[u8; TRUNCATED_HASHBYTES]) -> usize {
    wire_out(out)
        .iter()
        .filter(|(_, data)| {
            Packet::unpack(data)
                .map(|p| p.flags.packet_type == PacketType::Announce && &p.destination_hash == dest)
                .unwrap_or(false)
        })
        .count()
}

/// Path requests naming `dest` in this output — the shape of a
/// re-originated / forwarded discovery, and of the peer-up pull.
fn path_requests_for(
    out: &TickOutput,
    pr_hash: &[u8; TRUNCATED_HASHBYTES],
    dest: &[u8; TRUNCATED_HASHBYTES],
) -> usize {
    wire_out(out)
        .iter()
        .filter(|(_, data)| {
            Packet::unpack(data)
                .map(|p| {
                    p.flags.packet_type == PacketType::Data
                        && &p.destination_hash == pr_hash
                        && p.data.as_slice().len() >= TRUNCATED_HASHBYTES
                        && &p.data.as_slice()[..TRUNCATED_HASHBYTES] == dest
                })
                .unwrap_or(false)
        })
        .count()
}

/// The Pocket half of the chain, walked for real: D learned direct on
/// BLE, the peer-lost cull, then the path request the board actually
/// puts on the wire. The Pocket stays alive so the trap test can make
/// it re-request later. Returns the raw 48-byte-payload request.
fn pocket_emits_path_request(pocket: &mut EmbeddedNode, phone: &mut Peer) -> Vec<u8> {
    let ble = InterfaceId(0);
    let pocket_id = *pocket.identity().hash();
    let pr_hash = *pocket.transport().path_request_hash();

    let _ = pocket.handle_packet(ble, &phone.direct_announce(TEST_TIME_MS));
    assert!(pocket.has_path(&phone.dest_hash), "direct path installed");
    let _ = pocket.handle_interface_peer_lost(ble, phone.identity_hash);
    assert!(
        !pocket.has_path(&phone.dest_hash),
        "the cull must fire first — otherwise this is mechanism 1/2, not 3"
    );

    let out = pocket.request_path(&phone.dest_hash);
    let request = wire_out(&out)
        .into_iter()
        .map(|(_, data)| data)
        .find(|data| {
            Packet::unpack(data)
                .map(|p| p.destination_hash == pr_hash)
                .unwrap_or(false)
        })
        .expect("the culled Pocket must solicit the path");

    // The trap only arms on the 48-byte transport form naming the
    // Pocket as requestor (handle_path_request, requestor guard).
    let payload = Packet::unpack(&request).unwrap().data.as_slice().to_vec();
    assert_eq!(payload.len(), 3 * TRUNCATED_HASHBYTES, "transport form");
    assert_eq!(
        &payload[TRUNCATED_HASHBYTES..2 * TRUNCATED_HASHBYTES],
        &pocket_id,
        "requestor field is the Pocket's transport id"
    );
    request
}

/// Build the Pocket with the interface pair the boards carry.
fn make_pocket() -> Box<EmbeddedNode> {
    let mut pocket = make_node();
    let _ble = add_iface(&mut pocket, "ble_nrf", 0);
    let _lora = add_iface(&mut pocket, "lora_sx1262", 1);
    pocket
}

/// Run the relay's scheduler dry. Every announce received during setup
/// arms an ordinary rebroadcast in the announce_table; in compressed
/// mvr time that rebroadcast would fire inside an observation window
/// and read as an "answer". In the field it fired seconds after the
/// announce — setup traffic, not a reaction to the request.
fn drain_scheduler(relay: &mut EmbeddedNode) {
    for _ in 0..12 {
        let next = relay.transport().clock().now_ms() + 10_000;
        relay.transport().clock().set(next);
        if relay.handle_timeout().actions.is_empty() {
            break;
        }
    }
}

/// Everything the relay puts on the wire in response to the request:
/// the immediate actions plus the deferred grace-window answer
/// (announce_table entry fired by the scheduler).
fn relay_reaction(relay: &mut EmbeddedNode, lora: usize, request: &[u8]) -> Vec<TickOutput> {
    let immediate = relay.handle_packet(InterfaceId(lora), request);
    let fire_at = relay.transport().clock().now_ms() + PATH_REQUEST_GRACE_MS + 5_000;
    relay.transport().clock().set(fire_at);
    let deferred = relay.handle_timeout();
    std::vec![immediate, deferred]
}

/// Positive control: the healthy walk-5 start state. The relay holds D
/// DIRECT (its own BLE link to the phone), the request arrives over
/// LoRa from a third party — the relay answers with its cached announce
/// after the grace. Pins that the harness would see an answer if one
/// were given.
#[test]
fn control_relay_with_direct_path_answers_the_request() {
    let mut phone = make_peer();
    let mut pocket = make_pocket();
    let request = pocket_emits_path_request(&mut pocket, &mut phone);

    let mut relay = make_node();
    let ble = add_iface(&mut relay, "ble_nrf", 0);
    let lora = add_iface(&mut relay, "lora_sx1262", 1);
    let _ = relay.handle_packet(
        InterfaceId(ble),
        &phone.direct_announce(TEST_TIME_MS + 1_000),
    );
    let entry = relay
        .transport
        .get_path_clone(phone.dest_hash.as_bytes())
        .expect("relay holds D direct");
    assert_eq!(entry.next_hop, None, "direct: no next hop to match");

    drain_scheduler(&mut relay);
    let answered: usize = relay_reaction(&mut relay, lora, &request)
        .iter()
        .map(|out| announces_for(out, phone.dest_hash.as_bytes()))
        .sum();
    assert!(
        answered > 0,
        "a relay holding D direct must answer the path request"
    );
}

/// THE walk-2/walk-5 mechanism, closed: the relay holds D via the
/// requestor, so the Pocket's request dies in the requestor guard —
/// still, and deliberately (refusing that answer is correct, and the
/// guard is reference-identical). Then the phone's peer-up lands on the
/// relay's BLE interface: the relay pulls the phone's delivery path
/// over its own link, the phone's direct announce displaces the stale
/// via-the-requestor route, and the Pocket's next request IS answered.
/// Green closure of mechanism 3 (formerly `#[ignore]`d red as
/// `relay_holding_the_requestor_as_next_hop_swallows_the_request`).
#[test]
fn peer_up_pull_recovers_the_relay_from_the_requestor_next_hop_trap() {
    let mut phone = make_peer();
    let mut pocket = make_pocket();
    let pocket_id = *pocket.identity().hash();
    let request = pocket_emits_path_request(&mut pocket, &mut phone);
    let pr_hash = *pocket.transport().path_request_hash();

    // The relay's trap state: D via the Pocket (walk 2: rebooted relay
    // + Columba silent on reconnect; post-28de836 also reachable via
    // the relay's own cull on a BLE flap).
    let mut relay = make_node();
    let ble = add_iface(&mut relay, "ble_nrf", 0);
    let lora = add_iface(&mut relay, "lora_sx1262", 1);
    let _ = relay.handle_packet(
        InterfaceId(lora),
        &phone.relayed_announce(TEST_TIME_MS, pocket_id),
    );
    let entry = relay
        .transport
        .get_path_clone(phone.dest_hash.as_bytes())
        .expect("relay holds D via the Pocket");
    assert_eq!(entry.next_hop, Some(pocket_id), "next hop IS the requestor");
    // Pin WHICH branch the silence comes from: with the cache missing,
    // case 2b would fall through toward discovery (#169 deviation) and
    // silence would mean something else. Cache present + path present +
    // requestor == next_hop leaves exactly the requestor guard.
    assert!(
        relay
            .transport
            .storage()
            .get_announce_cache(phone.dest_hash.as_bytes())
            .is_some(),
        "the relay holds the cached announce it could answer with"
    );

    // Part 1, the guard is untouched: the request still dies silently.
    drain_scheduler(&mut relay);
    let reactions = relay_reaction(&mut relay, lora, &request);
    let answers: usize = reactions
        .iter()
        .map(|out| announces_for(out, phone.dest_hash.as_bytes()))
        .sum();
    let forwards: usize = reactions
        .iter()
        .map(|out| path_requests_for(out, &pr_hash, phone.dest_hash.as_bytes()))
        .sum();
    assert_eq!(
        answers + forwards,
        0,
        "the requestor guard must keep dropping the request itself — the \
         recovery goes around it, not through it"
    );

    // Part 2, the recovery: the phone links to the relay (walk 5's BLE
    // reconnect) and the interface reports peer-up. The relay holds D
    // only VIA the requestor, so the pull fires on the BLE link.
    let pull = relay.handle_interface_peer_up(InterfaceId(ble), phone.identity_hash);
    let relay_pr_hash = *relay.transport().path_request_hash();
    assert_eq!(
        path_requests_for(&pull, &relay_pr_hash, phone.dest_hash.as_bytes()),
        1,
        "peer-up with only a stale relayed entry must pull the delivery path"
    );

    // The phone's measured M1 behaviour: it answers the pull with an
    // ordinary direct announce over the same link.
    let ts = relay.transport().clock().now_ms();
    let _ = relay.handle_packet(InterfaceId(ble), &phone.direct_announce(ts));
    let entry = relay
        .transport
        .get_path_clone(phone.dest_hash.as_bytes())
        .expect("relay still holds D");
    assert_eq!(
        entry.next_hop, None,
        "the direct announce displaces the via-the-requestor route"
    );

    // Retire the rebroadcast the fresh announce armed, so the answer we
    // assert next cannot be setup traffic.
    drain_scheduler(&mut relay);

    // The Pocket asks again (its per-minute cadence in walk 2) — and
    // this time the relay answers. This is the former red assertion.
    let now = pocket.transport().clock().now_ms() + PATH_REQUEST_MIN_INTERVAL_MS + 1_000;
    pocket.transport().clock().set(now);
    let out = pocket.request_path(&phone.dest_hash);
    let request2 = wire_out(&out)
        .into_iter()
        .map(|(_, data)| data)
        .find(|data| {
            Packet::unpack(data)
                .map(|p| p.destination_hash == pr_hash)
                .unwrap_or(false)
        })
        .expect("the Pocket re-requests on its cadence");
    let answered: usize = relay_reaction(&mut relay, lora, &request2)
        .iter()
        .map(|out| announces_for(out, phone.dest_hash.as_bytes()))
        .sum();
    assert!(
        answered > 0,
        "after the peer-up pull the relay holds D direct and the Pocket's \
         next path request is answered — the cell recovers without waiting \
         for the phone's periodic announce"
    );
}
