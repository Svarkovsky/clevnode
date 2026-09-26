//! mvr: a relay that cannot answer a path request asks its live peer
//! links (Codeberg #365, the 2026-09-08 17:17-17:19 field finding).
//!
//! ## The field failure this reproduces
//!
//! The T114 (relay, BLE link to the phone) held `paths=0` for 70 s
//! after the phone linked at 17:17:47: the peer-up pull asked for a
//! destination derived from the HANDSHAKE identity, which against
//! Columba is the transport identity, not the LXMF identity — a
//! destination the phone does not have (see `mvr_peer_up_pull` for
//! that half). What resolved the phone every time was a path request
//! for the RIGHT destination arriving at the T114 and being forwarded
//! over BLE: the laptop's `rnpath e286…` at 17:18:57.092 went out on
//! the BLE link (local-client forward, case 4) and the phone's
//! announce came back 162 ms later. But a request from a NON-local
//! interface — the Pocket asking over LoRa — dies in case 3: a Full-
//! mode relay with no path, no cached announce and no local clients
//! re-originates nothing (the DISCOVER_PATHS_FOR gate, mirror of
//! Transport.py:2917-2918), and Columba answers path requests only
//! for its own destinations, so nobody in the field asks it the right
//! one.
//!
//! ## The mechanism pinned here
//!
//! Python's discovery-mode re-origination (Transport.py:2917-2918,
//! 3015-3037) already re-broadcasts an unanswerable request on every
//! other interface and answers the requester when the announce comes
//! back — but only when the receiving interface is in a
//! DISCOVER_PATHS_FOR mode. We generalise the gate from mode-gated to
//! peer-link-gated: a request on interface X for a destination we
//! hold no usable path to is re-originated ONCE toward every OTHER
//! interface whose driver mirrors live direct peers
//! (`set_interface_peer_count`, the sibling of the #365 online
//! mirror). The flood argument behind Python's mode gate does not
//! apply to peer links: each hop re-originates only away from the
//! arrival interface, at most once per destination per
//! PATH_REQUEST_MIN_INTERVAL_MS, and only onto link-shaped carriers
//! (BLE, TCP client links, IPC) whose peers answer for their own
//! destinations instead of re-broadcasting. A broadcast domain (LoRa)
//! never mirrors a peer count and is never a target. Python peers see
//! an ordinary 48-byte path request.
//!
//! ## Shape
//!
//! Deterministic, sub-second, one relay plus packed wire input (the
//! boards' exact storage type). The relay holds nothing for D, has a
//! live peer mirrored on the BLE interface, and receives the request
//! on LoRa: exactly one request for D leaves, on BLE only. The peer's
//! announce answer arrives on BLE: the relay answers toward LoRa
//! (ordinary discovery path response). A repeat request inside
//! PATH_REQUEST_MIN_INTERVAL_MS stays quiet (the once-bound). The
//! control pins today's behaviour as the baseline: without live
//! peers, nothing leaves.

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
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId, TickOutput};

/// Both boards' exact shape: `EmbeddedStorage`, transport enabled.
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

/// The phone: identity + `lxmf.delivery` destination + announce packer.
struct Peer {
    dest_hash: crate::DestinationHash,
    dest: Destination,
}

fn make_peer() -> Peer {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "lxmf",
        &["delivery"],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    Peer { dest_hash, dest }
}

impl Peer {
    /// A direct (wire hops 0) announce emitted at `ts`.
    fn direct_announce(&mut self, ts: u64) -> Vec<u8> {
        let ann = self.dest.announce(None, &mut OsRng, ts, ts / 1000).unwrap();
        let mut buf = [0u8; MTU];
        let len = ann.pack(&mut buf).unwrap();
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

/// Path requests naming `dest` in this output, with their send target.
fn path_requests_for(
    node: &EmbeddedNode,
    out: &TickOutput,
    dest: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<Option<InterfaceId>> {
    let pr_hash = *node.transport().path_request_hash();
    wire_out(out)
        .into_iter()
        .filter_map(|(target, data)| {
            let p = Packet::unpack(&data).ok()?;
            (p.flags.packet_type == PacketType::Data
                && p.destination_hash == pr_hash
                && p.data.as_slice().len() >= TRUNCATED_HASHBYTES
                && &p.data.as_slice()[..TRUNCATED_HASHBYTES] == dest)
                .then_some(target)
        })
        .collect()
}

/// Announces naming `dest` in this output, with their send target.
fn announces_for(out: &TickOutput, dest: &[u8; TRUNCATED_HASHBYTES]) -> Vec<Option<InterfaceId>> {
    wire_out(out)
        .into_iter()
        .filter_map(|(target, data)| {
            let p = Packet::unpack(&data).ok()?;
            (p.flags.packet_type == PacketType::Announce && &p.destination_hash == dest)
                .then_some(target)
        })
        .collect()
}

/// The 48-byte transport-form path request a rebooted/culled requester
/// puts on the wire (the Pocket's shape in the field), built from a
/// standalone node so the relay sees a foreign requestor id and tag.
fn foreign_path_request(dest: &[u8; TRUNCATED_HASHBYTES]) -> Vec<u8> {
    let mut requester = make_node();
    let _lora = add_iface(&mut requester, "lora_sx1262", 0);
    let pr_hash = *requester.transport().path_request_hash();
    let out = requester.request_path(&crate::DestinationHash::new(*dest));
    wire_out(&out)
        .into_iter()
        .map(|(_, data)| data)
        .find(|data| {
            Packet::unpack(data)
                .map(|p| p.destination_hash == pr_hash)
                .unwrap_or(false)
        })
        .expect("the requester emits a path request")
}

/// THE fix: with a live peer mirrored on BLE, a request the relay
/// cannot answer is re-originated once on the BLE interface — and only
/// there — and the peer's announce answer is relayed back to the
/// requesting interface. A second request inside the min interval
/// re-originates nothing (the once-bound).
#[test]
fn a_live_peer_link_gets_the_request_the_relay_cannot_answer() {
    let mut phone = make_peer();
    let mut relay = make_node();
    let ble = add_iface(&mut relay, "ble_nrf", 0);
    let lora = add_iface(&mut relay, "lora_sx1262", 1);
    // The driver's mirror: one live peer on the BLE link (the phone).
    relay.set_interface_peer_count(ble, 1);
    assert!(
        !relay.has_path(&phone.dest_hash),
        "premise: the relay holds nothing for D (the 17:17 field state)"
    );

    let request = foreign_path_request(phone.dest_hash.as_bytes());
    let out = relay.handle_packet(InterfaceId(lora), &request);
    let reoriginated = path_requests_for(&relay, &out, phone.dest_hash.as_bytes());
    assert_eq!(
        reoriginated,
        std::vec![Some(InterfaceId(ble))],
        "exactly one re-originated request, targeted at the peer-link \
         interface — never broadcast, never back onto the requester's"
    );

    // The once-bound: a fresh request (new requester, new tag) inside
    // PATH_REQUEST_MIN_INTERVAL_MS must not re-originate again.
    let request2 = foreign_path_request(phone.dest_hash.as_bytes());
    let out = relay.handle_packet(InterfaceId(lora), &request2);
    assert!(
        path_requests_for(&relay, &out, phone.dest_hash.as_bytes()).is_empty(),
        "one re-origination per destination per PATH_REQUEST_MIN_INTERVAL_MS"
    );

    // The phone's measured behaviour (M1, ledger 365): it answers a
    // path request for its own destination with an ordinary direct
    // announce over the same link — and the relay answers the
    // requesting interface (the ordinary discovery path response,
    // Transport.py:1838-1865 semantics).
    let ts = relay.transport().clock().now_ms();
    let answer = relay.handle_packet(InterfaceId(ble), &phone.direct_announce(ts));
    assert!(
        relay.has_path(&phone.dest_hash),
        "the answer installs the path at the relay"
    );
    let answered = announces_for(&answer, phone.dest_hash.as_bytes());
    assert!(
        answered.contains(&Some(InterfaceId(lora))),
        "the announce answer is relayed to the requesting interface"
    );
}

/// Control: today's behaviour is the baseline. Without live peers
/// anywhere, the same request still dies silently — the Full-mode
/// DISCOVER_PATHS_FOR gate (Transport.py:2917-2918) is untouched, and
/// no interface without mirrored peers is ever a re-origination
/// target (a LoRa broadcast domain never mirrors one).
#[test]
fn control_without_live_peers_the_request_still_dies() {
    let phone = make_peer();
    let mut relay = make_node();
    let _ble = add_iface(&mut relay, "ble_nrf", 0);
    let lora = add_iface(&mut relay, "lora_sx1262", 1);
    // No set_interface_peer_count call: nobody mirrored a peer.

    let request = foreign_path_request(phone.dest_hash.as_bytes());
    let out = relay.handle_packet(InterfaceId(lora), &request);
    assert!(
        wire_out(&out).is_empty(),
        "no live peers: nothing leaves on any interface (today's drop)"
    );
}
