//! mvr: a stale path via a lost BLE peer must not keep eating traffic
//! (Codeberg #365).
//!
//! ## The field failure this reproduces
//!
//! Walk test 2026-09-04: a Pocket V2 learns the phone as a 1-hop path
//! over BLE, walks out of BLE range but stays in LoRa range of a T114
//! that still holds a BLE link to the phone. For 15 minutes every LoRa
//! packet from the Pocket is an announce — not one telemetry DATA packet
//! — because the dead 1-hop BLE entry keeps winning (one path entry per
//! destination) and nothing ever invalidates it. The phone receives no
//! position until the Pocket is rebooted (walk 2: after a reboot the
//! same node issued 20 path requests over LoRa — the RAM path table was
//! the difference).
//!
//! ## Reference semantics
//!
//! Python culls a path whose receiving interface no longer exists
//! (Transport.py:784-785) and expires paths on roaming / access-point
//! interfaces early (Transport.py:773-779: `ROAMING_PATH_TIME` 6 h,
//! `AP_PATH_TIME` 24 h; the expiry comparison is :780). Python has no
//! multi-peer interface, so its unit of loss is the whole interface; on
//! a BLE broadcast domain the unit that comes and goes is one peer link
//! inside the interface. The semantics carry over: a path whose carrier
//! can no longer reach the next hop is stale. Interface isolation puts
//! the detection in the interface (only it knows the link died); it
//! reports the peer's identity hash — the same 16 bytes the Columba
//! handshake exchanges and the same value the peer stamps as
//! `transport_id` on announces it relays — and the transport culls.
//!
//! ## Shape
//!
//! 1 node (the Pocket's exact storage type), 2 mock interfaces A (BLE)
//! and B (LoRa), deterministic, sub-second. D is announced direct on A
//! (1 hop) and relayed on B (2 hops); the table holds A. A reports the
//! peer gone. A subsequent report to D must not leave on A: either it
//! leaves on B, or — since the 2-hop entry was never installed — a path
//! request goes out. "Off to A into the void" is the bug.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction};
use crate::embedded_storage::EmbeddedStorage;
use crate::identity::Identity;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::packet::{HeaderType, Packet, PacketType, TransportType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::transport::{Action, InterfaceId, TickOutput};

/// The Pocket's exact shape: `EmbeddedStorage`, transport enabled.
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

/// A peer destination and everything the test needs to speak for it:
/// the identity hash the peer would present in the Columba handshake,
/// the destination hash, and a packer for fresh direct announces (each
/// call packs a NEW emission so replay protection never dedups).
struct Peer {
    identity_hash: [u8; TRUNCATED_HASHBYTES],
    dest_hash: crate::DestinationHash,
    dest: Destination,
}

fn make_peer(app: &'static str) -> Peer {
    let identity = Identity::generate(&mut OsRng);
    let identity_hash = *identity.hash();
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &[app],
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
    /// Pack a direct (wire hops 0) announce emitted at `ts`.
    fn direct_announce(&mut self, ts: u64) -> Vec<u8> {
        let ann = self.dest.announce(None, &mut OsRng, ts, ts / 1000).unwrap();
        let mut buf = [0u8; MTU];
        let len = ann.pack(&mut buf).unwrap();
        buf[..len].to_vec()
    }

    /// The same announce one relay later: HEADER_2, wire hops 1, the
    /// relay's identity hash as `transport_id` — byte-exact what a
    /// transport node puts on the air when it rebroadcasts.
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

/// Every DATA packet for `dest` in this output, as (send target, was it
/// a broadcast) — the observable that distinguishes "off to A into the
/// void" from a healthy reroute.
fn data_sends_to(out: &TickOutput, dest: &[u8; TRUNCATED_HASHBYTES]) -> Vec<Option<InterfaceId>> {
    out.actions
        .iter()
        .filter_map(|action| {
            let (data, target) = match action {
                Action::SendPacket { iface, data, .. } => (data, Some(*iface)),
                Action::Broadcast { data, .. } => (data, None),
            };
            Packet::unpack(data)
                .ok()
                .filter(|p| p.flags.packet_type == PacketType::Data && &p.destination_hash == dest)
                .map(|_| target)
        })
        .collect()
}

/// Every path request for `dest` in this output (same recognizer as
/// mvr_reboot_relay_nopath_solicit).
fn path_requests_for(
    out: &TickOutput,
    pr_hash: &[u8; TRUNCATED_HASHBYTES],
    dest: &[u8; TRUNCATED_HASHBYTES],
) -> usize {
    out.actions
        .iter()
        .filter(|action| {
            let data = match action {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
            };
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

fn path_lost_count(out: &TickOutput, dest: &crate::DestinationHash) -> usize {
    out.events
        .iter()
        .filter(
            |e| matches!(e, NodeEvent::PathLost { destination_hash } if destination_hash == dest),
        )
        .count()
}

/// THE field scenario: D direct on BLE (1 hop, wins) and relayed on
/// LoRa (2 hops, displaced — one entry per destination). The BLE
/// interface reports the peer gone. The stale entry must go, and the
/// next report must not be handed to the dead carrier.
#[test]
fn report_after_peer_loss_does_not_leave_on_the_dead_interface() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let lora = add_iface(&mut node, "lora_sx1262", 1);

    let mut phone = make_peer("blepeer");
    let t114 = [0xC7u8; TRUNCATED_HASHBYTES];

    // Relayed first (older emission), direct second (newer AND better):
    // the table must end up holding the 1-hop BLE entry either way.
    let _ = node.handle_packet(
        InterfaceId(lora),
        &phone.relayed_announce(TEST_TIME_MS, t114),
    );
    let _ = node.handle_packet(
        InterfaceId(ble),
        &phone.direct_announce(TEST_TIME_MS + 1_000),
    );

    let entry = node
        .transport
        .get_path_clone(phone.dest_hash.as_bytes())
        .expect("announce on BLE must install a path");
    assert_eq!(entry.hops, 1, "direct BLE announce wins the table");
    assert_eq!(entry.interface_index, ble, "and it points at BLE");

    // The hook under test: BLE says "peer X is gone".
    let peer_lost_out = node.handle_interface_peer_lost(InterfaceId(ble), phone.identity_hash);

    // What the telemetry sender does next tick (telemetry.rs): path? send
    // : request. With the stale entry culled it must NOT hand the report
    // to the dead BLE carrier.
    let pr_hash = *node.transport().path_request_hash();
    let out = if node.has_path(&phone.dest_hash) {
        let (_, out) = node
            .send_single_packet(&phone.dest_hash, b"position report")
            .expect("send with a live path");
        out
    } else {
        node.request_path(&phone.dest_hash)
    };

    let to_ble = data_sends_to(&out, phone.dest_hash.as_bytes())
        .iter()
        .filter(|t| **t == Some(InterfaceId(ble)))
        .count();
    assert_eq!(
        to_ble, 0,
        "the report left on the dead BLE interface — into the void; this \
         is the #365 field mechanism (15 min of LoRa announces, zero DATA)"
    );
    let to_lora = data_sends_to(&out, phone.dest_hash.as_bytes())
        .iter()
        .filter(|t| **t == Some(InterfaceId(lora)))
        .count();
    assert!(
        to_lora > 0 || path_requests_for(&out, &pr_hash, phone.dest_hash.as_bytes()) > 0,
        "with the stale entry culled the node must either route via LoRa \
         or solicit the path — silence is the other half of the bug"
    );
    assert_eq!(
        path_lost_count(&peer_lost_out, &phone.dest_hash),
        1,
        "the cull must be announced as PathLost (Transport.py:784-785 \
         semantics, per-peer)"
    );
}

/// Scope, both boundaries at once: on the lost peer's interface, culling
/// takes the relayed entries named after the peer AND leaves every other
/// peer's entries — relayed and direct — alone.
#[test]
fn cull_is_scoped_to_the_lost_peer() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);

    let lost_peer = [0xAAu8; TRUNCATED_HASHBYTES];
    let other_peer = [0xBBu8; TRUNCATED_HASHBYTES];

    // Relayed via the peer that will die, relayed via a healthy peer,
    // and a healthy peer's own direct destination — all on one BLE
    // broadcast domain.
    let mut via_lost = make_peer("vialost");
    let mut via_other = make_peer("viaother");
    let mut direct_other = make_peer("directother");
    let _ = node.handle_packet(
        InterfaceId(ble),
        &via_lost.relayed_announce(TEST_TIME_MS, lost_peer),
    );
    let _ = node.handle_packet(
        InterfaceId(ble),
        &via_other.relayed_announce(TEST_TIME_MS, other_peer),
    );
    let _ = node.handle_packet(
        InterfaceId(ble),
        &direct_other.direct_announce(TEST_TIME_MS),
    );
    assert_eq!(node.path_count(), 3, "all three paths installed");

    let out = node.handle_interface_peer_lost(InterfaceId(ble), lost_peer);

    assert!(
        !node.has_path(&via_lost.dest_hash),
        "the entry relayed via the lost peer must be culled"
    );
    assert_eq!(path_lost_count(&out, &via_lost.dest_hash), 1);
    assert!(
        node.has_path(&via_other.dest_hash),
        "an entry relayed via a DIFFERENT peer on the same interface must survive"
    );
    assert!(
        node.has_path(&direct_other.dest_hash),
        "a different peer's direct entry on the same interface must survive"
    );
}

/// Interface scope: the report names (interface, peer); the same peer
/// hash seen via another interface is a different neighbourhood and
/// must keep its entries (mirror of Python keying the cull by the
/// receiving interface).
#[test]
fn cull_is_scoped_to_the_reporting_interface() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let lora = add_iface(&mut node, "lora_sx1262", 1);

    let relay = [0xC7u8; TRUNCATED_HASHBYTES];
    let mut peer = make_peer("ifacescope");
    let _ = node.handle_packet(
        InterfaceId(lora),
        &peer.relayed_announce(TEST_TIME_MS, relay),
    );
    assert!(node.has_path(&peer.dest_hash), "LoRa path installed");

    let out = node.handle_interface_peer_lost(InterfaceId(ble), relay);
    assert_eq!(
        out.events.len(),
        0,
        "a peer loss on BLE must not touch paths learned over LoRa"
    );
    assert!(node.has_path(&peer.dest_hash), "the LoRa path survives");
}
