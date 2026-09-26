//! mvr: a path over an offline interface is no path (Codeberg #365).
//!
//! ## The desk failure this reproduces
//!
//! Desk test 2026-09-08 17:20 (Pocket V2 on 2e4840b): `--set-media
//! ble=off`, then a telemetry report with `reason=immediate`. The path
//! table still held the phone's `lxmf.delivery` direct over BLE, the
//! transport routed the report there, and the BLE interface's
//! carrier-off `try_send` returned `Ok(())` — a silent black hole the
//! transport counted as sent. No data packet on serial, none on LoRa,
//! no path request, `nopath` stayed 0.
//!
//! ## Reference semantics
//!
//! Python culls a path whose attached interface no longer exists
//! (Transport.py:784-785) and each Python interface's
//! `process_outgoing` is gated on `self.online` — an offline interface
//! never carries the packet, and the path to it is torn down rather
//! than trusted. Our runtime media switch turns a carrier off without
//! detaching the interface, so the equivalent statement is made at the
//! path lookup: the driver mirrors each interface's `is_online()` into
//! the transport, and a path entry whose interface is offline does not
//! count as a path (`has_path` false, `send_to_destination` NoPath).
//! Deviation rule: wire format untouched (nothing new on the air),
//! semantics untouched (a peer sees an ordinary path request instead
//! of silence), P1 gain is the desk-measured black hole closed.
//!
//! ## Shape
//!
//! 1 node, 2 mock interfaces X (BLE) and Y (LoRa), deterministic,
//! sub-second. D is announced direct on X. X goes offline. A report to
//! D must NOT be handed to X; the sender's `has_path ? send : request`
//! sequence must fall through to a path request, which still leaves on
//! the online interfaces (broadcast).

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

/// A peer destination the test can announce for (same helper shape as
/// mvr_ble_peer_loss_reroute).
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

/// Every DATA packet for `dest` in this output, as (send target, was it
/// a broadcast).
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
/// mvr_ble_peer_loss_reroute).
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

/// THE desk scenario: D direct on X, X offline, a report to D. The
/// packet must not be handed to X; the sender sequence must fall
/// through to a path request instead.
#[test]
fn send_to_a_path_over_an_offline_interface_is_withheld() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let _lora = add_iface(&mut node, "lora_sx1262", 1);

    let mut phone = make_peer("offline1");
    let _ = node.handle_packet(InterfaceId(ble), &phone.direct_announce(TEST_TIME_MS));
    assert!(
        node.has_path(&phone.dest_hash),
        "announce on BLE must install a path"
    );

    // The carrier goes off; the driver mirrors is_online() == false.
    node.set_interface_online(ble, false);

    assert!(
        !node.has_path(&phone.dest_hash),
        "a path over an offline interface must not count as a path \
         (the #365 desk black hole: the entry stood, the carrier was off)"
    );

    // What the telemetry sender does next tick (telemetry.rs): path? send
    // : request. With X offline it must land in the request arm.
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
        "the report was handed to the offline interface — the silent \
         carrier-off drop counts it as sent (desk test 2026-09-08 17:20)"
    );
    assert!(
        path_requests_for(&out, &pr_hash, phone.dest_hash.as_bytes()) > 0,
        "with the path unusable the node must solicit one over the \
         carriers that are still online"
    );
}

/// The transport half stated directly: `send_to_destination` refuses a
/// path whose interface is offline, so no caller — telemetry or any
/// other — can hand a packet to a dead carrier through the path table.
#[test]
fn send_single_packet_refuses_the_offline_path() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);

    let mut phone = make_peer("offline2");
    let _ = node.handle_packet(InterfaceId(ble), &phone.direct_announce(TEST_TIME_MS));
    node.set_interface_online(ble, false);

    assert!(
        node.send_single_packet(&phone.dest_hash, b"report")
            .is_err(),
        "send_single_packet must refuse to route onto an offline interface"
    );
}

/// The recovery chain of the desk test, end to end: the stale 1-hop
/// entry sits on the offline interface, the path request marks it
/// unresponsive, and the neighbour's answer — the SAME announce
/// emission relayed one hop further — displaces it (the
/// Transport.py:1677-1679 alternative-route arm). The next report
/// leaves on the online carrier with the relay as next hop.
#[test]
fn relayed_answer_with_the_same_emission_displaces_the_offline_entry() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let lora = add_iface(&mut node, "lora_sx1262", 1);

    let t114 = [0xC7u8; TRUNCATED_HASHBYTES];
    let mut phone = make_peer("offline4");
    let raw = phone.direct_announce(TEST_TIME_MS);
    let _ = node.handle_packet(InterfaceId(ble), &raw);
    node.set_interface_online(ble, false);

    // The sender's no-path arm solicits; the solicitation marks the
    // offline entry unresponsive.
    let _ = node.request_path(&phone.dest_hash);

    // The relay answers with its CACHED copy of the same announce:
    // identical emission, one hop further, its own id as transport_id.
    let mut relayed = Packet::unpack(&raw).unwrap();
    relayed.flags.header_type = crate::packet::HeaderType::Type2;
    relayed.flags.transport_type = crate::packet::TransportType::Transport;
    relayed.hops = 1;
    relayed.transport_id = Some(t114);
    let mut buf = [0u8; MTU];
    let len = relayed.pack(&mut buf).unwrap();
    let _ = node.handle_packet(InterfaceId(lora), &buf[..len]);

    let entry = node
        .transport
        .get_path_clone(phone.dest_hash.as_bytes())
        .expect("path present");
    assert_eq!(
        entry.interface_index, lora,
        "the same-emission relayed answer must displace the offline \
         direct entry (alternative-route arm, Transport.py:1677-1679)"
    );
    assert_eq!(entry.hops, 2, "one relay in between");

    let (_, out) = node
        .send_single_packet(&phone.dest_hash, b"position report")
        .expect("send over the relayed path");
    let to_lora = data_sends_to(&out, phone.dest_hash.as_bytes())
        .iter()
        .filter(|t| **t == Some(InterfaceId(lora)))
        .count();
    assert!(to_lora > 0, "the report leaves on the online carrier");
}

/// The switch is a mirror, not a cull: the entry survives the offline
/// window, and the moment the driver reports the interface back online
/// the same path routes again — no announce, no path request needed.
#[test]
fn path_over_a_reonlined_interface_routes_again() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);

    let mut phone = make_peer("offline3");
    let _ = node.handle_packet(InterfaceId(ble), &phone.direct_announce(TEST_TIME_MS));
    node.set_interface_online(ble, false);
    assert!(!node.has_path(&phone.dest_hash));

    node.set_interface_online(ble, true);
    assert!(
        node.has_path(&phone.dest_hash),
        "the entry must survive the offline window untouched"
    );
    let (_, out) = node
        .send_single_packet(&phone.dest_hash, b"position report")
        .expect("send with the interface back online");
    let to_ble = data_sends_to(&out, phone.dest_hash.as_bytes())
        .iter()
        .filter(|t| **t == Some(InterfaceId(ble)))
        .count();
    assert!(to_ble > 0, "the report leaves on the re-onlined interface");
}
