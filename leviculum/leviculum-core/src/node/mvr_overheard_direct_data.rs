//! #383 mvr: does a transport node repeat a HEADER_1 data packet it merely
//! overhears, for a destination it happens to hold a path to?
//!
//! ## The observation this answers
//!
//! Three nodes share one medium. alpha probes charlie, one hop away and
//! within earshot. bravo is neither sender nor destination, but it holds a
//! path to charlie, so it repeats every probe it hears: 30 probes on the air
//! became 30 repeats, in the same millisecond charlie answered, and 24 of the
//! 30 proofs died in the collision (periculum #51, 3 of 30 delivered against
//! Python's 30 of 30 on the same lossless medium).
//!
//! ## The rule being pinned
//!
//! The reference path-forwards a data packet only when the packet NAMES this
//! node as the next hop: `if packet.transport_id != None and ... ==
//! Transport.identity.hash` (reference/Reticulum/RNS/Transport.py:1559-1560).
//! A destination one hop from the sender is sent as HEADER_1 with no
//! transport id at all (`Transport.outbound`, Transport.py:1134-1166), so no
//! transport node is addressed and none repeats it. The single exception is a
//! packet whose destination sits behind a local client, where the stripped
//! transport id is synthesized back (Transport.py:1547-1548).
//!
//! The discriminator this test exists to protect: the gate is "am I named",
//! NOT "is the destination more than one hop away". The last hop of a chain
//! A-B-C, where A cannot hear C, arrives at B with B's own id and a ONE hop
//! path onward, and must still be repeated. Control 2 is that packet, byte
//! for byte, and it must stay green.
//!
//! Sibling mvr `mvr_embedded_same_iface_relay` covers the HEADER_2 cases —
//! addressed to us, addressed to a foreigner, and no path — but every packet
//! it feeds carries a transport id, so the HEADER_1 case that costs the
//! delivery figure was never asked.
//!
//! Sans-I/O: no LoRa, no Docker, no Python, sub-second wall clock.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::transport::{Action, InterfaceId, PathEntry, TickOutput};
use crate::DestinationHash;

type Bravo = NodeCore<OsRng, MockClock, MemoryStorage>;

/// bravo: a transport node with one shared interface, the medium alpha,
/// bravo and charlie all sit on.
fn make_bravo() -> (Bravo, usize) {
    let mut node = NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        MemoryStorage::with_defaults(),
    );
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new("serial_0", 1)));
    node.set_interface_name(idx, String::from("serial_0"));
    (node, idx)
}

/// charlie's address.
fn make_charlie() -> DestinationHash {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["overheard"],
    )
    .unwrap();
    *dest.hash()
}

/// Install the path bravo holds. `hops == 1` with no next hop is what a
/// directly heard announce from charlie leaves behind, which is exactly
/// bravo's state in the measurement.
fn install_path(
    node: &mut Bravo,
    dest: &DestinationHash,
    hops: u8,
    iface: usize,
    next_hop: Option<[u8; TRUNCATED_HASHBYTES]>,
) {
    node.transport.insert_path(
        *dest.as_bytes(),
        PathEntry {
            hops,
            expires_ms: u64::MAX,
            interface_index: iface,
            random_blobs: Vec::new(),
            next_hop,
            via_peer: None,
        },
    );
}

/// The 131 byte probe alpha puts on the air for a destination it can reach
/// directly: HEADER_1, BROADCAST, no transport id.
fn probe_direct(dest: &DestinationHash) -> Vec<u8> {
    build(dest, HeaderType::Type1, TransportType::Broadcast, None)
}

/// The same probe as it arrives at the LAST hop of a chain: HEADER_2,
/// TRANSPORT, addressed at the relay by name.
fn probe_addressed_to(dest: &DestinationHash, relay: [u8; TRUNCATED_HASHBYTES]) -> Vec<u8> {
    build(
        dest,
        HeaderType::Type2,
        TransportType::Transport,
        Some(relay),
    )
}

fn build(
    dest: &DestinationHash,
    header_type: HeaderType,
    transport_type: TransportType,
    transport_id: Option<[u8; TRUNCATED_HASHBYTES]>,
) -> Vec<u8> {
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type,
            context_flag: false,
            transport_type,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id,
        destination_hash: *dest.as_bytes(),
        context: PacketContext::None,
        data: PacketData::Owned(std::vec![0xA5; 48]),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// Every data packet for `dest` this output puts on the wire, in any action
/// form, with the interface it goes out on.
fn transmitted(out: &TickOutput, dest: &DestinationHash) -> Vec<(Option<usize>, Packet)> {
    out.actions
        .iter()
        .filter_map(|action| {
            let (iface, data) = match action {
                Action::SendPacket { iface, data, .. } => (Some(iface.0), data),
                Action::Broadcast { data, .. } => (None, data),
            };
            Packet::unpack(data).ok().and_then(|p| {
                (p.flags.packet_type == PacketType::Data && &p.destination_hash == dest.as_bytes())
                    .then_some((iface, p))
            })
        })
        .collect()
}

/// THE question (#383): bravo overhears a probe for charlie, one hop away,
/// and holds a path to charlie. Does it put a second copy on the air?
#[test]
fn overheard_direct_data_is_not_repeated() {
    let charlie = make_charlie();
    let (mut bravo, serial0) = make_bravo();
    install_path(&mut bravo, &charlie, 1, serial0, None);

    let before = bravo.transport_stats();
    let out = bravo.handle_packet(InterfaceId(serial0), &probe_direct(&charlie));
    let after = bravo.transport_stats();

    let sent = transmitted(&out, &charlie);
    assert!(
        sent.is_empty(),
        "a node that is neither sender nor destination must transmit nothing \
         for a packet no transport header addressed to it; got {} frame(s) \
         (drops this packet: nopath={} dup={} overheard={} maxhops={})",
        sent.len(),
        after.drops_no_path() - before.drops_no_path(),
        after.drops_duplicate() - before.drops_duplicate(),
        after.drops_overheard_transport_id() - before.drops_overheard_transport_id(),
        after.drops_forward_max_hops() - before.drops_forward_max_hops(),
    );
    assert_eq!(
        after.packets_forwarded() - before.packets_forwarded(),
        0,
        "and nothing may be counted as forwarded"
    );
    assert_eq!(
        after.drops_overheard_transport_id() - before.drops_overheard_transport_id(),
        1,
        "the drop belongs to the overheard counter, the same account the \
         HEADER_2 copies bound elsewhere land in; a no-path attribution here \
         would be a lie, bravo knows the path perfectly well"
    );
}

/// Control 1: the same overheard probe with a path that is TWO hops onward.
/// Still no transport header naming bravo, so still no repeat. Without this
/// control the main assertion reads as "one hop destinations are not
/// repeated", which is not the rule.
#[test]
fn control_overheard_direct_data_with_multihop_path_is_not_repeated_either() {
    let charlie = make_charlie();
    let (mut bravo, serial0) = make_bravo();
    install_path(
        &mut bravo,
        &charlie,
        3,
        serial0,
        Some([0x5A; TRUNCATED_HASHBYTES]),
    );

    let out = bravo.handle_packet(InterfaceId(serial0), &probe_direct(&charlie));
    assert!(
        transmitted(&out, &charlie).is_empty(),
        "hop count is not the question; being named as the next hop is"
    );
}

/// Control 2: the chain A-B-C where A cannot hear C. The probe reaches bravo
/// as HEADER_2 with bravo's own id, and bravo's path onward is ONE hop. It
/// must be repeated, stripped back to HEADER_1. A fix that keys on the path's
/// hop count instead of on the transport id turns this red.
#[test]
fn control_chain_last_hop_is_still_repeated() {
    let charlie = make_charlie();
    let (mut bravo, serial0) = make_bravo();
    install_path(&mut bravo, &charlie, 1, serial0, None);

    let own_id = *bravo.identity().hash();
    let before = bravo.transport_stats();
    let out = bravo.handle_packet(InterfaceId(serial0), &probe_addressed_to(&charlie, own_id));
    let after = bravo.transport_stats();

    let sent = transmitted(&out, &charlie);
    assert_eq!(
        sent.len(),
        1,
        "the last hop of a chain must still be repeated, on the interface the \
         path names, which is the one it arrived on (drops: nopath={} dup={} \
         overheard={} maxhops={})",
        after.drops_no_path() - before.drops_no_path(),
        after.drops_duplicate() - before.drops_duplicate(),
        after.drops_overheard_transport_id() - before.drops_overheard_transport_id(),
        after.drops_forward_max_hops() - before.drops_forward_max_hops(),
    );
    let (iface, packet) = &sent[0];
    assert_eq!(*iface, Some(serial0), "back out on the shared medium");
    assert_eq!(
        packet.flags.header_type,
        HeaderType::Type1,
        "one hop onward, so the transport header is stripped \
         (Transport.py:1573-1577)"
    );
    assert_eq!(
        packet.transport_id, None,
        "and the next hop field with it — charlie receives it as a direct packet"
    );
}

/// Control 3: the destination sits behind a local client, at `hops == 0`.
/// The reference synthesizes the stripped transport id back in exactly this
/// case (Transport.py:1547-1548), so a HEADER_1 packet overheard on the
/// medium must still be handed down the client leg. This is the arm the gate
/// must not close.
#[test]
fn control_destination_behind_local_client_is_still_delivered() {
    let charlie = make_charlie();
    let (mut bravo, serial0) = make_bravo();
    let ipc = bravo
        .transport
        .register_interface(Box::new(MockInterface::new("ipc_0", 1)));
    bravo.set_interface_name(ipc, String::from("ipc_0"));
    bravo.transport.set_local_client(ipc, true);
    // hops == 0 is what a path behind a local client looks like.
    install_path(&mut bravo, &charlie, 0, ipc, None);

    let out = bravo.handle_packet(InterfaceId(serial0), &probe_direct(&charlie));
    let sent = transmitted(&out, &charlie);
    assert_eq!(
        sent.len(),
        1,
        "a packet for a destination behind a local client is not overheard \
         traffic, it is ours to deliver"
    );
    assert_eq!(
        sent[0].0,
        Some(ipc),
        "down the client leg, not back onto the air"
    );
}
