//! mvr: a shared-instance client must insert the transport header for a 1-hop
//! destination (#119).
//!
//! ## The defect
//!
//! A node that is a CLIENT of a local shared instance (transport disabled,
//! `shared_instance_interface` set) and holds a path with `hops == 1` to a
//! destination one hop beyond that instance sends its link request as a plain
//! HEADER_1 packet: `PathEntry::needs_relay()` is `hops > 1 && next_hop`, so
//! neither `connect` nor `send_to_destination` converts the packet to HEADER_2.
//! A Python `rnsd` shared instance receives a HEADER_1 packet not addressed to
//! itself, does not transport it, and the link never establishes.
//!
//! Python converts it: `Transport.outbound` inserts the HEADER_2 transport
//! header for a 1-hop destination when `is_connected_to_shared_instance`
//! (reference/Reticulum/RNS/Transport.py:1148-1166).
//!
//! ## The scenario
//!
//! ```text
//!   client --IPC uplink--> shared instance --network--> D (hops_to(D) = 1)
//! ```
//!
//! The client's path table holds D at `hops == 1` with `next_hop` set (the
//! announce for D reached the client through the instance, which stamped its
//! transport_id). `connect(D)` must emit a HEADER_2 link request carrying that
//! next_hop as transport_id. On master it emits HEADER_1 — the RED assertion.
//!
//! Controls: a regular (non-shared-client) node with the same 1-hop path keeps
//! HEADER_1 (no over-routing), and the hops > 1 conversion is unchanged.
//!
//! Sans-I/O: no LoRa, no Docker, no Python, sub-second wall clock.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::TRUNCATED_HASHBYTES;
use crate::destination::{Destination, DestinationType, Direction};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{HeaderType, PacketFlags, TransportType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::transport::{Action, InterfaceId, PathEntry, TickOutput};
use crate::DestinationHash;

type ClientNode = NodeCore<OsRng, MockClock, MemoryStorage>;

/// The transport_id the shared instance stamped into the announce for D.
const NEXT_HOP: [u8; TRUNCATED_HASHBYTES] = [0xAA; TRUNCATED_HASHBYTES];

/// A non-transport node with one uplink interface. When `shared_client` is
/// set, the uplink is marked as the connection to a local shared instance
/// (mirrors the `connect_to_shared_instance` driver path).
fn make_node(shared_client: bool) -> (ClientNode, usize) {
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults());
    let iface = node
        .transport
        .register_interface(Box::new(MockInterface::new("uplink", 0)));
    node.set_interface_name(iface, String::from("LocalClient[default]"));
    if shared_client {
        node.transport.set_shared_instance_interface(Some(iface));
    }
    (node, iface)
}

/// A destination we can `connect` to: its hash and Ed25519 verifying key.
fn make_destination() -> (DestinationHash, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["shared1hop"],
    )
    .unwrap();
    (*dest.hash(), signing_key)
}

fn install_path(node: &mut ClientNode, dest: &DestinationHash, hops: u8, iface: usize) {
    node.transport.insert_path(
        *dest.as_bytes(),
        PathEntry {
            hops,
            expires_ms: u64::MAX,
            interface_index: iface,
            random_blobs: Vec::new(),
            next_hop: Some(NEXT_HOP),
            via_peer: None,
        },
    );
}

/// The single routed SendPacket this output emitted; panics on broadcasts or
/// packet counts != 1 (the path is known, so nothing may fall back to
/// broadcast).
fn one_send_packet(output: &TickOutput) -> (InterfaceId, Vec<u8>) {
    let mut sends = Vec::new();
    for action in &output.actions {
        match action {
            Action::SendPacket { iface, data, .. } => sends.push((*iface, data.clone())),
            Action::Broadcast { .. } => panic!("routed send must not broadcast"),
        }
    }
    assert_eq!(
        sends.len(),
        1,
        "expected exactly one outbound packet, got {}",
        sends.len()
    );
    sends.pop().unwrap()
}

/// RED on master: the shared-instance client emits the 1-hop link request as
/// HEADER_1 without a transport header, so the instance never forwards it
/// (#119). GREEN: HEADER_2 with the path's next_hop as transport_id.
#[test]
fn shared_client_1hop_link_request_is_type2_with_transport_id() {
    let (mut node, iface) = make_node(true);
    let (dest, signing_key) = make_destination();
    install_path(&mut node, &dest, 1, iface);

    let (_link, was_routed, out) = node.connect(dest, &signing_key);
    assert!(was_routed, "the known 1-hop path must route the request");

    let (send_iface, pkt) = one_send_packet(&out);
    assert_eq!(send_iface, InterfaceId(iface), "sent on the uplink");

    assert!(
        pkt[0] & 0x40 != 0,
        "a shared-instance client must send the 1-hop link request as \
         HEADER_2 (transport bit set) so the instance transports it \
         (Transport.py:1155); got flags {:#04x} (HEADER_1)",
        pkt[0]
    );
    let flags = PacketFlags::from_byte(pkt[0]).unwrap();
    assert_eq!(flags.header_type, HeaderType::Type2);
    assert_eq!(flags.transport_type, TransportType::Transport);
    assert_eq!(
        &pkt[2..2 + TRUNCATED_HASHBYTES],
        &NEXT_HOP,
        "transport_id must be the path's next_hop"
    );
    assert_eq!(
        &pkt[2 + TRUNCATED_HASHBYTES..2 + 2 * TRUNCATED_HASHBYTES],
        dest.as_bytes(),
        "addressed destination must follow the transport_id"
    );
}

/// CONTROL: the SAME 1-hop path on a node that is NOT a shared-instance
/// client stays HEADER_1 — a direct neighbor is reached directly, no
/// over-routing (the discriminator is the shared-instance marker, not the
/// path entry).
#[test]
fn regular_node_1hop_link_request_stays_type1() {
    let (mut node, iface) = make_node(false);
    let (dest, signing_key) = make_destination();
    install_path(&mut node, &dest, 1, iface);

    let (_link, was_routed, out) = node.connect(dest, &signing_key);
    assert!(was_routed, "the known 1-hop path must route the request");

    let (send_iface, pkt) = one_send_packet(&out);
    assert_eq!(send_iface, InterfaceId(iface), "sent on the interface");

    assert!(
        pkt[0] & 0x40 == 0,
        "a regular node must keep the 1-hop link request HEADER_1; got \
         flags {:#04x}",
        pkt[0]
    );
    assert_eq!(
        &pkt[2..2 + TRUNCATED_HASHBYTES],
        dest.as_bytes(),
        "destination must directly follow the header (no transport_id)"
    );
}

/// CONTROL / regression: the hops > 1 conversion is unchanged — a multi-hop
/// link request carries the transport header exactly as before.
#[test]
fn multihop_link_request_still_type2() {
    let (mut node, iface) = make_node(false);
    let (dest, signing_key) = make_destination();
    install_path(&mut node, &dest, 2, iface);

    let (_link, was_routed, out) = node.connect(dest, &signing_key);
    assert!(was_routed, "the known 2-hop path must route the request");

    let (send_iface, pkt) = one_send_packet(&out);
    assert_eq!(send_iface, InterfaceId(iface), "sent on the interface");

    let flags = PacketFlags::from_byte(pkt[0]).unwrap();
    assert_eq!(flags.header_type, HeaderType::Type2);
    assert_eq!(flags.transport_type, TransportType::Transport);
    assert_eq!(
        &pkt[2..2 + TRUNCATED_HASHBYTES],
        &NEXT_HOP,
        "transport_id must be the path's next_hop"
    );
    assert_eq!(
        &pkt[2 + TRUNCATED_HASHBYTES..2 + 2 * TRUNCATED_HASHBYTES],
        dest.as_bytes(),
        "addressed destination must follow the transport_id"
    );
}
