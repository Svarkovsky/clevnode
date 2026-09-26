//! mvr (rotation investigation 2026-08-21): a Single-destination packet that
//! reaches its registered destination but fails to decrypt must be counted
//! and journey-logged as `single-decrypt-fail`, not vanish.
//!
//! Background: the rig's `lora_ratchet_rotation` scenario lost exactly one of
//! ten post-rotation single packets in two consecutive full corpus runs while
//! every transport drop counter stayed at zero — because a decrypt miss at
//! the node's destination layer had no counter and no event. Whatever the
//! radio-side mechanism turns out to be, the drop class itself must be
//! observable first.
//!
//! Three cases pin the indicator from both sides (a diagnostic that cannot
//! fire on the good path AND fire on the bad one measures nothing):
//!
//! 1. an undecryptable payload is dropped, counted, and emits no
//!    `PacketReceived`;
//! 2. a payload encrypted under the CURRENT ratchet is delivered and does
//!    not touch the counter;
//! 3. a payload encrypted under the PREVIOUS ratchet, sent after the
//!    destination rotated, is still delivered (retained-key decrypt) and
//!    does not touch the counter — this is the protocol-layer refutation of
//!    the naive "receiver has not switched yet" window: rotation keeps the
//!    old private key, so old-key traffic survives a rotation by design
//!    (Python parity: `Identity.decrypt` walks `Destination.ratchets`).

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
use crate::transport::InterfaceId;
use crate::DestinationHash;

type TestNode = NodeCore<OsRng, MockClock, MemoryStorage>;

fn make_node() -> (TestNode, usize) {
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node: TestNode =
        NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults());
    let iface = node
        .transport
        .register_interface(Box::new(MockInterface::new("if0", 0)));
    node.set_interface_name(iface, String::from("if0"));
    (node, iface)
}

/// Register a ratchet-enabled Single destination; returns its hash, its
/// public identity (the sender's encryption view) and the initial ratchet
/// public key.
fn register_ratchet_dest(node: &mut TestNode) -> (DestinationHash, Identity, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let pub_view = Identity::from_public_key_bytes(&identity.public_key_bytes())
        .expect("public identity view");
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["decrypt"],
    )
    .expect("destination");
    dest.enable_ratchets(&mut OsRng, TEST_TIME_MS)
        .expect("enable ratchets");
    dest.set_ratchet_interval(1000);
    let ratchet_pub = dest.current_ratchet_public().expect("initial ratchet");
    let hash = *dest.hash();
    node.register_destination(dest);
    (hash, pub_view, ratchet_pub)
}

/// Wire bytes for a Type1 broadcast Data packet to `hash` carrying `payload`.
fn data_packet(hash: &DestinationHash, payload: Vec<u8>) -> Vec<u8> {
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
        context: PacketContext::None,
        data: PacketData::Owned(payload),
    };
    let mut buf = [0u8; crate::constants::MTU];
    let len = packet.pack(&mut buf).expect("pack");
    buf[..len].to_vec()
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

/// Case 1: an undecryptable Single-destination payload is dropped, counted
/// under `single-decrypt-fail`, and delivers nothing.
#[test]
fn undecryptable_single_packet_is_counted_not_silent() {
    let (mut node, iface) = make_node();
    let (hash, _pub_view, _ratchet) = register_ratchet_dest(&mut node);

    assert_eq!(node.transport.stats().drops_single_decrypt_fail(), 0);

    // 128 bytes of junk: long enough to parse as ephemeral-key + token,
    // decryptable by nothing.
    let raw = data_packet(&hash, std::vec![0x42u8; 128]);
    let out = node.handle_packet(InterfaceId(iface), &raw);

    assert!(
        delivered_payloads(&out.events).is_empty(),
        "an undecryptable packet must not deliver"
    );
    assert_eq!(
        node.transport.stats().drops_single_decrypt_fail(),
        1,
        "the decrypt miss must be counted, not dropped silently"
    );
}

/// Case 2 (good-path control): a payload encrypted under the current ratchet
/// delivers and leaves the counter untouched.
#[test]
fn current_ratchet_packet_delivers_and_counts_nothing() {
    let (mut node, iface) = make_node();
    let (hash, pub_view, ratchet_pub) = register_ratchet_dest(&mut node);

    let payload = pub_view
        .encrypt_for_destination(b"current-key", Some(&ratchet_pub), &mut OsRng)
        .expect("encrypt");
    let raw = data_packet(&hash, payload);
    let out = node.handle_packet(InterfaceId(iface), &raw);

    assert_eq!(
        delivered_payloads(&out.events),
        std::vec![b"current-key".to_vec()],
        "a current-ratchet packet must deliver"
    );
    assert_eq!(
        node.transport.stats().drops_single_decrypt_fail(),
        0,
        "the counter must not fire on the good path"
    );
}

/// Case 3 (retention): after the destination rotates its ratchet, a packet
/// still encrypted under the PREVIOUS ratchet key delivers via the retained
/// key — the protocol keeps no window in which old-key traffic dies.
#[test]
fn old_ratchet_packet_after_rotation_still_delivers() {
    let (mut node, iface) = make_node();
    let (hash, pub_view, old_ratchet_pub) = register_ratchet_dest(&mut node);

    // Let the 1 s interval expire and rotate via the announce path, exactly
    // as the selftest's re-announce does (Destination::announce ->
    // rotate_ratchet_if_needed).
    node.transport.clock().advance(1500);
    let _ = node
        .announce_destination(&hash, None)
        .expect("re-announce rotates");
    let new_ratchet_pub = node
        .destination_ratchet_public(&hash)
        .expect("ratchet present");
    assert_ne!(
        new_ratchet_pub, old_ratchet_pub,
        "scaffold: the re-announce must have rotated the ratchet"
    );

    let payload = pub_view
        .encrypt_for_destination(b"old-key", Some(&old_ratchet_pub), &mut OsRng)
        .expect("encrypt");
    let raw = data_packet(&hash, payload);
    let out = node.handle_packet(InterfaceId(iface), &raw);

    assert_eq!(
        delivered_payloads(&out.events),
        std::vec![b"old-key".to_vec()],
        "an old-ratchet packet must still decrypt after rotation (retained keys)"
    );
    assert_eq!(
        node.transport.stats().drops_single_decrypt_fail(),
        0,
        "retained-key delivery must not count as a decrypt fail"
    );
}
