//! mvr (Codeberg #332): relaying must not require understanding.
//!
//! The context byte is semantic, not routing information. Python stores it as
//! a raw int with no validation (`reference/Reticulum/RNS/Packet.py:258,263`)
//! and neither `Transport.packet_filter` (Transport.py:1336-1387) nor the
//! forwarding path ever rejects a value it does not know — a Python relay
//! forwards traffic from a newer RNS or a third implementation unchanged.
//!
//! Our `Packet::unpack` used to map an unrecognised context byte to
//! `PacketError::InvalidContext`, and `Transport::process_incoming_inner`'s
//! `Packet::unpack(&raw)?` aborted the whole inbound handler on it — before
//! dedup, before delivery, before forwarding. A transport node was therefore a
//! silent black hole for any context byte it did not know.
//!
//! Three behaviours pin the split between "can be relayed" and "can be
//! interpreted", each with a positive control, because an assertion that
//! cannot fail on the broken path measures nothing:
//!
//! 1. relay: an unknown-context packet arriving on interface A for a
//!    destination reachable via interface B is forwarded on B, byte-for-byte
//!    including the context byte (control: a known-context packet of the same
//!    shape is forwarded — the harness does relay);
//! 2. delivery: the same packet addressed to US is dropped with the NAMED
//!    reason `unknown-context` and counted, never interpreted (control: a
//!    known-context packet to the same destination delivers and leaves the
//!    counter at zero);
//! 3. dedup: an unknown-context packet participates in duplicate suppression
//!    exactly like a known one (control: a *different* unknown-context packet
//!    is still forwarded, so the suppression is dedup and not a blanket drop).

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, NoStorage};
use crate::transport::{Action, InterfaceId, TickOutput};
use crate::DestinationHash;

type TransportNode = NodeCore<OsRng, MockClock, MemoryStorage>;
type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

/// A context byte no Reticulum version assigns a meaning to today. The point
/// of the batch is that this node must not need to know what it means.
const UNKNOWN_CTX: u8 = 0x42;
/// A second unknown value, for the "different packet still relays" control.
const OTHER_UNKNOWN_CTX: u8 = 0x43;

fn add_iface<C, S>(node: &mut NodeCore<OsRng, C, S>, name: &'static str) -> usize
where
    C: Clock,
    S: crate::traits::Storage,
{
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn make_relay() -> TransportNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        clock,
        MemoryStorage::with_defaults(),
    )
}

/// An endpoint node holding a plain (non-ratchet) Single destination, plus the
/// announce bytes that teach a relay the path to it and the public identity
/// view a sender would encrypt with.
fn make_endpoint() -> (EndpointNode, DestinationHash, Identity, Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let pub_view =
        Identity::from_public_key_bytes(&identity.public_key_bytes()).expect("public view");
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["unknownctx"],
    )
    .expect("destination");
    let dest_hash = *dest.hash();
    let announce = dest
        .announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000)
        .expect("announce");
    let mut buf = [0u8; crate::constants::MTU];
    let len = announce.pack(&mut buf).expect("pack announce");
    let announce_raw = buf[..len].to_vec();

    node.register_destination(dest);
    (node, dest_hash, pub_view, announce_raw)
}

/// Wire bytes for a Type1 broadcast Data packet with an arbitrary context byte.
///
/// Built through `Packet::pack` so the context round-trips through the same
/// encoder production uses; the caller's byte lands at wire offset 18.
fn data_packet(hash: &DestinationHash, context: u8, payload: Vec<u8>) -> Vec<u8> {
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: hash.into_bytes(),
        context: PacketContext::from_byte(context),
        data: PacketData::Owned(payload),
    };
    let mut buf = [0u8; crate::constants::MTU];
    let len = packet.pack(&mut buf).expect("pack");
    let raw = buf[..len].to_vec();
    assert_eq!(
        raw[18], context,
        "scaffold: the context byte must survive pack() verbatim"
    );
    raw
}

/// The same packet as it reaches a transport node legitimately: HEADER_2,
/// addressed at the relay by name.
///
/// A HEADER_1 packet is never path-forwarded by a transport node (#383,
/// `Transport.py:1559-1560`): a destination one hop from the sender is
/// broadcast directly and every node in earshot that repeated it would only
/// bury the answer. So the scaffold for a relay test has to address the relay
/// — the context byte, not the transport header, is what these tests are
/// about.
fn relayed_data_packet(
    hash: &DestinationHash,
    relay_id: [u8; crate::constants::TRUNCATED_HASHBYTES],
    context: u8,
    payload: Vec<u8>,
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
        hops: 0,
        transport_id: Some(relay_id),
        destination_hash: hash.into_bytes(),
        context: PacketContext::from_byte(context),
        data: PacketData::Owned(payload),
    };
    let mut buf = [0u8; crate::constants::MTU];
    let len = packet.pack(&mut buf).expect("pack");
    let raw = buf[..len].to_vec();
    assert_eq!(
        raw[34], context,
        "scaffold: the context byte must survive pack() verbatim (HEADER_2 \
         puts it 16 bytes further in)"
    );
    raw
}

fn outbound(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

fn delivered_payloads(events: &[NodeEvent]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            NodeEvent::PacketReceived { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect()
}

/// A relay that knows a 1-hop path to `dest_hash` via `to_dest`, plus the
/// second interface a packet for that destination arrives on.
fn relay_with_path(announce_raw: &[u8]) -> (TransportNode, usize, usize) {
    let mut relay = make_relay();
    let to_dest = add_iface(&mut relay, "R_to_dest");
    let from_far = add_iface(&mut relay, "R_from_far");
    let _ = relay.handle_packet(InterfaceId(to_dest), announce_raw);
    (relay, to_dest, from_far)
}

/// Behaviour 1: a packet whose header parses is relayable regardless of its
/// context byte. Pre-fix this test fails at the unpack inside
/// `process_incoming_inner`: no forward, no drop counter, nothing.
#[test]
fn unknown_context_packet_is_relayed() {
    let (_endpoint, dest_hash, _pub_view, announce_raw) = make_endpoint();
    let (mut relay, _to_dest, from_far) = relay_with_path(&announce_raw);
    assert_eq!(
        relay.hops_to(&dest_hash),
        Some(1),
        "scaffold: the relay must have learned the path from the announce"
    );

    let relay_id = *relay.identity().hash();
    let raw = relayed_data_packet(&dest_hash, relay_id, UNKNOWN_CTX, std::vec![0xAAu8; 64]);
    let sent = outbound(&relay.handle_packet(InterfaceId(from_far), &raw));

    assert_eq!(
        sent.len(),
        1,
        "an unknown context byte must not stop the relay: expected one forward, got {}",
        sent.len()
    );
    let forwarded = Packet::unpack(&sent[0]).expect("the forward must still parse");
    assert_eq!(
        forwarded.context,
        PacketContext::Unknown(UNKNOWN_CTX),
        "the relay must forward the context byte verbatim, not normalise it"
    );
    assert_eq!(
        sent[0][18], UNKNOWN_CTX,
        "the context byte must be unchanged on the wire"
    );
    assert_eq!(
        forwarded.data.as_slice(),
        &[0xAAu8; 64],
        "the payload must be forwarded untouched"
    );
    assert_eq!(
        relay.transport.stats().drops_unknown_context(),
        0,
        "relaying must never count as an unknown-context drop"
    );
}

/// Behaviour 1, positive control: the identical packet shape with a context
/// byte we DO know relays. Without this, "one forward" above could be
/// asserting a property of a harness that forwards everything or nothing.
#[test]
fn known_context_packet_is_relayed_control() {
    let (_endpoint, dest_hash, _pub_view, announce_raw) = make_endpoint();
    let (mut relay, _to_dest, from_far) = relay_with_path(&announce_raw);

    let relay_id = *relay.identity().hash();
    let raw = relayed_data_packet(
        &dest_hash,
        relay_id,
        PacketContext::None.to_byte(),
        std::vec![
            0xAAu8;
            64
        ],
    );
    let sent = outbound(&relay.handle_packet(InterfaceId(from_far), &raw));

    assert_eq!(sent.len(), 1, "control: a known-context packet must relay");
    assert_eq!(sent[0][18], PacketContext::None.to_byte());
}

/// Behaviour 2: addressed to us, an unknown context byte has no
/// interpretation, so we abstain — with a named, counted drop rather than
/// silence or a guess ("plain link data" would be a guess).
#[test]
fn unknown_context_packet_for_us_is_dropped_with_named_reason() {
    let (mut endpoint, dest_hash, _pub_view, _announce_raw) = make_endpoint();
    let iface = add_iface(&mut endpoint, "E_mesh");

    assert_eq!(endpoint.transport.stats().drops_unknown_context(), 0);
    let dropped_before = endpoint.transport.stats().packets_dropped();

    let raw = data_packet(&dest_hash, UNKNOWN_CTX, std::vec![0xBBu8; 96]);
    let out = endpoint.handle_packet(InterfaceId(iface), &raw);

    assert!(
        delivered_payloads(&out.events).is_empty(),
        "an uninterpretable packet must not be delivered to the application"
    );
    assert_eq!(
        endpoint.transport.stats().drops_unknown_context(),
        1,
        "the abstention must be counted under its own named reason"
    );
    assert_eq!(
        endpoint.transport.stats().packets_dropped(),
        dropped_before + 1,
        "the named drop must also reach the grand total (record_drop invariant)"
    );
    assert_eq!(
        crate::transport::DropReason::UnknownContext.kebab(),
        "unknown-context",
        "the reason must have a stable name for the PKT_DROP event catalogue"
    );
}

/// Behaviour 2, positive control: a KNOWN-context packet to the same
/// destination delivers and leaves the new counter at zero. A drop counter
/// that also fires on the good path is not an indicator.
#[test]
fn known_context_packet_for_us_delivers_control() {
    let (mut endpoint, dest_hash, pub_view, _announce_raw) = make_endpoint();
    let iface = add_iface(&mut endpoint, "E_mesh");

    let payload = pub_view
        .encrypt_for_destination(b"interpretable", None, &mut OsRng)
        .expect("encrypt");
    let raw = data_packet(&dest_hash, PacketContext::None.to_byte(), payload);
    let out = endpoint.handle_packet(InterfaceId(iface), &raw);

    assert_eq!(
        delivered_payloads(&out.events),
        std::vec![b"interpretable".to_vec()],
        "control: a known-context packet for us must still be delivered"
    );
    assert_eq!(
        endpoint.transport.stats().drops_unknown_context(),
        0,
        "the unknown-context counter must not fire on the good path"
    );
}

/// Behaviour 3: dedup, hop accounting and rate limiting are bookkeeping over
/// the raw packet, not interpretation; only the semantic layer abstains.
/// So the second copy of an unknown-context packet must be suppressed as a
/// duplicate, exactly as a known-context packet would be.
#[test]
fn unknown_context_packet_participates_in_dedup() {
    let (_endpoint, dest_hash, _pub_view, announce_raw) = make_endpoint();
    let (mut relay, _to_dest, from_far) = relay_with_path(&announce_raw);

    let relay_id = *relay.identity().hash();
    let raw = relayed_data_packet(&dest_hash, relay_id, UNKNOWN_CTX, std::vec![0xCCu8; 64]);

    let first = outbound(&relay.handle_packet(InterfaceId(from_far), &raw));
    assert_eq!(first.len(), 1, "scaffold: the first copy must be forwarded");
    let dup_before = relay.transport.stats().drops_duplicate();

    let second = outbound(&relay.handle_packet(InterfaceId(from_far), &raw));
    assert!(
        second.is_empty(),
        "the duplicate must be suppressed, not forwarded a second time"
    );
    assert_eq!(
        relay.transport.stats().drops_duplicate(),
        dup_before + 1,
        "the duplicate must be counted under `duplicate`, not under a context reason"
    );
    assert_eq!(
        relay.transport.stats().drops_unknown_context(),
        0,
        "dedup must not be attributed to the context byte"
    );
}

/// Behaviour 3, positive control: a DIFFERENT unknown-context packet after the
/// duplicate is still forwarded. Without this, the empty result above would
/// also be satisfied by a relay that has simply stopped forwarding.
#[test]
fn distinct_unknown_context_packet_still_relays_control() {
    let (_endpoint, dest_hash, _pub_view, announce_raw) = make_endpoint();
    let (mut relay, _to_dest, from_far) = relay_with_path(&announce_raw);

    let relay_id = *relay.identity().hash();
    let first = relayed_data_packet(&dest_hash, relay_id, UNKNOWN_CTX, std::vec![0xCCu8; 64]);
    let _ = relay.handle_packet(InterfaceId(from_far), &first);
    let _ = relay.handle_packet(InterfaceId(from_far), &first);

    // Different context byte AND different payload: a genuinely new packet.
    let other = relayed_data_packet(
        &dest_hash,
        relay_id,
        OTHER_UNKNOWN_CTX,
        std::vec![0xDDu8; 64],
    );
    let sent = outbound(&relay.handle_packet(InterfaceId(from_far), &other));

    assert_eq!(
        sent.len(),
        1,
        "control: a distinct packet must still relay after a duplicate was suppressed"
    );
    assert_eq!(sent[0][18], OTHER_UNKNOWN_CTX);
}
