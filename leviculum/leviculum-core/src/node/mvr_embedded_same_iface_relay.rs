//! #344 mvr: does an `EmbeddedStorage`-backed transport node forward a
//! HEADER_2 data packet addressed to itself, on the interface it arrived on?
//!
//! ## The observation this answers
//!
//! On the bench a RAK4631 sends an LXMF report to a destination D whose path
//! it holds via a T114. The T114 is measurably not deaf — 17 transmissions,
//! 34 of 34 heard by the other two boards over 5.5 h — and it demonstrably
//! relays ANNOUNCES for D on that same LoRa interface (12 relayed announces
//! at hops=2). It never once relays the report: over 5.5 h the receiver
//! logged 13 arrivals, all at hops=1, and the 723 ms transmission that a
//! relayed report would be does not occur a single time.
//!
//! The firmware could not say why. `leviculum-nrf/Cargo.toml:17` pulls
//! `leviculum-core` with `default-features = false`, so the `tracing` feature
//! is off and every `debug!`/`trace!` in the core is a no-op on the board
//! (`leviculum-core/src/lib.rs:83-100`): `NoPath`, `Duplicate` and
//! "forwarded into a silent interface" are indistinguishable from the outside.
//!
//! ## What is pinned
//!
//! The core half of that question, stated so a test can answer it: a
//! transport node N with `EmbeddedStorage` and ONE interface, holding a path
//! to D learned from a RELAYED announce (so `PathEntry::needs_relay()` holds
//! — the shape the bench is in, not a directly-connected path), fed a
//! HEADER_2 `Data` packet for D whose `transport_id` is N's own identity
//! hash, must emit a forward on that same interface with the next hop
//! rewritten.
//!
//! Three controls, so a red main assertion cannot be read as anything else:
//!
//! - a FOREIGN transport id on the same packet: no forward, and
//!   `drops_overheard_transport_id` accounts for it;
//! - NO path for D: no forward, and `drops_no_path` accounts for it. The
//!   hardware may be in exactly this state, so the test has to be able to
//!   tell the two apart by counter, not by silence;
//! - the ANNOUNCE case on the same single interface, which works on
//!   hardware. It must come out green: a red there means the rig is
//!   miswired, not that the code is wrong.
//!
//! Sans-I/O: no LoRa, no Docker, no Python, sub-second wall clock.

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
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId, TickOutput};

/// The board under test: the firmware's exact type parameters bar the RNG and
/// clock, which are the deterministic test doubles.
type EmbeddedNode = NodeCore<OsRng, MockClock, EmbeddedStorage>;

/// The upstream relay whose id sits in the announce's transport header, i.e.
/// the next hop N must stamp into a forward for D.
const UPSTREAM: [u8; TRUNCATED_HASHBYTES] = [0x5A; TRUNCATED_HASHBYTES];

/// A transport node with `EmbeddedStorage`, built exactly as the firmware
/// builds it (`bin/t114.rs:142`) — `build_boxed`, because a by-value
/// `NodeCore` with inline storage is >40 KB of frame.
fn make_node() -> Box<EmbeddedNode> {
    NodeCoreBuilder::new().enable_transport(true).build_boxed(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        EmbeddedStorage::new(),
    )
}

/// Register the single interface. One interface is the point: the bench T114
/// hears the report and would have to relay it back out on the same LoRa
/// radio.
fn add_lora(node: &mut EmbeddedNode) -> usize {
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new("lora_sx1262", 1)));
    node.set_interface_name(idx, String::from("lora_sx1262"));
    idx
}

/// Destination D and one direct (wire hops 0) announce for it.
fn make_destination() -> (crate::DestinationHash, Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["embrelay"],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    let announce = dest
        .announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000)
        .unwrap();
    let mut buf = [0u8; MTU];
    let len = announce.pack(&mut buf).unwrap();
    (dest_hash, buf[..len].to_vec())
}

/// Re-stamp a direct announce as one that reached us THROUGH `transport_id`
/// at `wire_hops`. The receipt increment makes the stored path
/// `wire_hops + 1`, and the transport header becomes the path's next hop
/// (`transport.rs:4329`) — which is what `needs_relay()` needs.
fn announce_via(raw: &[u8], transport_id: [u8; TRUNCATED_HASHBYTES], wire_hops: u8) -> Vec<u8> {
    let mut packet = Packet::unpack(raw).unwrap();
    packet.flags.header_type = HeaderType::Type2;
    packet.flags.transport_type = TransportType::Transport;
    packet.transport_id = Some(transport_id);
    packet.hops = wire_hops;
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// A HEADER_2 `Data` packet for `dest`, addressed to the transport node
/// `transport_id`. This is the shape of the report the RAK put on the air.
fn data_via(
    transport_id: [u8; TRUNCATED_HASHBYTES],
    dest: &crate::DestinationHash,
    wire_hops: u8,
) -> Vec<u8> {
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type2,
            context_flag: false,
            transport_type: TransportType::Transport,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Data,
        },
        hops: wire_hops,
        transport_id: Some(transport_id),
        destination_hash: *dest.as_bytes(),
        context: PacketContext::None,
        data: PacketData::Owned(std::vec![0xA5; 48]),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// Every packet this output puts on the wire, in any action form, that is a
/// `Data` packet for `dest`.
fn forwarded_data(out: &TickOutput, dest: &crate::DestinationHash) -> Vec<(Option<usize>, Packet)> {
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

/// Any announce for `dest` this output puts on the wire, in any action form.
fn announce_tx(out: &TickOutput, dest: &crate::DestinationHash) -> usize {
    out.actions
        .iter()
        .filter(|action| {
            let data = match action {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
            };
            Packet::unpack(data)
                .map(|p| {
                    p.flags.packet_type == PacketType::Announce
                        && &p.destination_hash == dest.as_bytes()
                })
                .unwrap_or(false)
        })
        .count()
}

/// THE question (#344): N holds a relayed path to D on its only interface and
/// is handed a HEADER_2 data packet for D addressed to itself. Does the core
/// forward it back out on that interface with the next hop rewritten?
#[test]
fn embedded_transport_forwards_data_back_out_the_arrival_interface() {
    let (dest, announce_raw) = make_destination();
    let mut node = make_node();
    let lora = add_lora(&mut node);

    // The path, learned the way the bench learned it: a RELAYED announce, so
    // the entry carries a next hop and `needs_relay()` holds.
    let _ = node.handle_packet(InterfaceId(lora), &announce_via(&announce_raw, UPSTREAM, 1));
    assert_eq!(
        node.transport().hops_to(dest.as_bytes()),
        Some(2),
        "the relayed announce must install a 2-hop path (wire hops 1 + receipt \
         increment); without hops > 1 needs_relay() is false and this test \
         would be measuring the directly-connected case instead"
    );

    let own_id = *node.identity().hash();
    let before = node.transport_stats();
    let out = node.handle_packet(InterfaceId(lora), &data_via(own_id, &dest, 1));
    let after = node.transport_stats();

    let forwards = forwarded_data(&out, &dest);
    assert_eq!(
        forwards.len(),
        1,
        "a transport node handed a HEADER_2 data packet addressed to ITSELF, \
         for a destination it holds a path to, must forward it exactly once; \
         got {} (drops this packet: nopath={} dup={} overheard={} maxhops={})",
        forwards.len(),
        after.drops_no_path() - before.drops_no_path(),
        after.drops_duplicate() - before.drops_duplicate(),
        after.drops_overheard_transport_id() - before.drops_overheard_transport_id(),
        after.drops_forward_max_hops() - before.drops_forward_max_hops(),
    );

    let (iface, packet) = &forwards[0];
    assert_eq!(
        *iface,
        Some(lora),
        "the forward goes out on the interface the path names — which is the \
         interface the packet arrived on; there is no same-interface guard on \
         the data path"
    );
    assert_eq!(
        packet.flags.header_type,
        HeaderType::Type2,
        "a relay with a next hop keeps the transport header (transport.rs:5833-5814)"
    );
    assert_eq!(
        packet.transport_id,
        Some(UPSTREAM),
        "the next hop must be rewritten to the path's next hop, not left as \
         the relay's own id"
    );
    assert_eq!(
        after.packets_forwarded() - before.packets_forwarded(),
        1,
        "the counter the firmware now prints as `fwd=` must account for it"
    );
}

/// Control 1: the same packet addressed to SOMEONE ELSE. No forward, and the
/// overheard counter — the one that carries the high-volume shared-medium
/// traffic — is what accounts for the drop.
#[test]
fn control_foreign_transport_id_is_not_forwarded_and_counts_as_overheard() {
    let (dest, announce_raw) = make_destination();
    let mut node = make_node();
    let lora = add_lora(&mut node);
    let _ = node.handle_packet(InterfaceId(lora), &announce_via(&announce_raw, UPSTREAM, 1));

    let foreign = [0xC3u8; TRUNCATED_HASHBYTES];
    assert_ne!(&foreign, node.identity().hash(), "control must be foreign");

    let before = node.transport_stats();
    let out = node.handle_packet(InterfaceId(lora), &data_via(foreign, &dest, 1));
    let after = node.transport_stats();

    assert!(
        forwarded_data(&out, &dest).is_empty(),
        "a packet addressed to another transport node must not be forwarded"
    );
    assert_eq!(
        after.drops_overheard_transport_id() - before.drops_overheard_transport_id(),
        1,
        "the drop must be attributed to overheard-transport-id"
    );
    assert_eq!(
        after.packets_forwarded() - before.packets_forwarded(),
        0,
        "nothing forwarded"
    );
}

/// Control 2: the packet is addressed to us, but we hold no path for D. This
/// is the state the hardware may be in, so the counters must separate it from
/// control 1 and from a successful forward.
#[test]
fn control_no_path_is_not_forwarded_and_counts_as_nopath() {
    let (dest, _announce_raw) = make_destination();
    let mut node = make_node();
    let lora = add_lora(&mut node);
    // Deliberately NO announce fed: the path table stays empty.
    assert_eq!(node.path_count(), 0, "control must start with no path");

    let own_id = *node.identity().hash();
    let before = node.transport_stats();
    let out = node.handle_packet(InterfaceId(lora), &data_via(own_id, &dest, 1));
    let after = node.transport_stats();

    assert!(
        forwarded_data(&out, &dest).is_empty(),
        "with no path there is nowhere to forward"
    );
    assert_eq!(
        after.drops_no_path() - before.drops_no_path(),
        1,
        "the drop must be attributed to no-path — this is precisely the \
         distinction the `[TRANSPORT] nopath=` / `paths=` pair exists to make"
    );
    assert_eq!(
        after.drops_overheard_transport_id() - before.drops_overheard_transport_id(),
        0,
        "and NOT to overheard-transport-id: the packet was addressed to us"
    );
}

/// Control 3: the announce case, on the same single interface. This is what
/// the bench T114 demonstrably does 12 times over. A red here means the
/// harness is wrong, not the code.
#[test]
fn control_announce_is_relayed_on_the_same_single_interface() {
    let (dest, announce_raw) = make_destination();
    let mut node = make_node();
    let lora = add_lora(&mut node);

    let out = node.handle_packet(InterfaceId(lora), &announce_via(&announce_raw, UPSTREAM, 1));
    assert_eq!(
        announce_tx(&out, &dest),
        0,
        "the rebroadcast is scheduled, not immediate (jitter window)"
    );

    // Past the jitter window, the retry scheduler fires it.
    let past_jitter =
        node.transport().clock().now_ms() + node.transport().announce_jitter_max_ms() + 100;
    node.transport().clock().set(past_jitter);
    let out = node.handle_timeout();
    assert_eq!(
        announce_tx(&out, &dest),
        1,
        "a transport node with ONE interface relays an announce back out on \
         it — the behaviour measured on the bench (12 relayed announces at \
         hops=2). If this is red the rig is miswired, not the code."
    );
}
