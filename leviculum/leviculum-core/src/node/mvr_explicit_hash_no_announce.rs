//! mvr (PR #254): announce-shaped bytes for an explicit-hash destination
//! must never reach the wire.
//!
//! `Destination::with_explicit_hash` indexes a Single destination under a
//! caller-supplied 16-byte hash instead of
//! `truncated_hash(name_hash || identity_hash)`. Every Python-RNS peer
//! recomputes that structural hash when validating an announce
//! (`Identity.validate_announce`, Identity.py:584-587: `expected_hash =
//! full_hash(name_hash + identity.hash)[:16]`, mismatch -> reject), so an
//! announce for such a destination is unverifiable on the air by
//! construction. The refusal must therefore hold on EVERY path that can put
//! an announce for a local destination on the wire:
//!
//!   * the manual announce API (`NodeCore::announce_destination`),
//!   * the path-response generator — the reference answers a path request
//!     for a local destination by regenerating the announce
//!     (`Transport.path_request`, Transport.py:2940 calls
//!     `destination.announce(path_response=True)`); ours must answer with
//!     silence instead for an explicit-hash destination,
//!   * the management announce cycle and the interface-up re-announce
//!     (both skip via `Destination::is_explicit_hash`).
//!
//! At the same time the destination must remain a perfectly ordinary Single
//! destination for everything that is NOT an announce: a LINK_REQUEST
//! addressed to the explicit hash resolves by bare table lookup and proves
//! with the destination's real identity.
//!
//! Mutation contract: reverting the refusal (e.g. `explicit_hash: false` in
//! `with_explicit_hash`, or removing the guard in `Destination::announce`)
//! must turn `path_request_for_explicit_hash_gets_silence` and
//! `announce_api_refuses_explicit_hash` red.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::announce::AnnounceError;
use crate::constants::TRUNCATED_HASHBYTES;
use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::{Link, LinkState};
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId, TickOutput};

/// KAT from the vendored reference:
/// RNS.Destination.hash(None, "rnstransport", "path", "request").
const PATH_REQUEST_DEST_KAT: [u8; TRUNCATED_HASHBYTES] = [
    0x6b, 0x9f, 0x66, 0x01, 0x4d, 0x98, 0x53, 0xfa, 0xab, 0x22, 0x0f, 0xba, 0x47, 0xd0, 0x27, 0x61,
];

/// The federation-style override hash the tests register under.
const EXPLICIT_HASH: [u8; TRUNCATED_HASHBYTES] = [
    0xFE, 0xDE, 0x0A, 0x71, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB,
];

type TestNode = NodeCore<OsRng, MockClock, MemoryStorage>;

fn make_node() -> (TestNode, usize) {
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node: TestNode =
        NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults());
    let iface = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new("if0", 0)));
    node.set_interface_name(iface, String::from("if0"));
    (node, iface)
}

/// Register an explicit-hash Single destination; returns its signing key.
fn register_explicit_dest(node: &mut TestNode) -> [u8; 32] {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let mut dest = Destination::with_explicit_hash(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["explicit"],
        EXPLICIT_HASH,
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    assert_eq!(dest.hash().as_bytes(), &EXPLICIT_HASH);
    node.register_destination(dest);
    signing_key
}

/// Register a normal (derived-hash) Single destination; returns its hash.
fn register_normal_dest(node: &mut TestNode) -> crate::DestinationHash {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["derived"],
    )
    .unwrap();
    let hash = *dest.hash();
    node.register_destination(dest);
    hash
}

/// Wire-format path request for `target`, tagged so grace dedup cannot merge
/// the two requests the silence test sends.
fn path_request_raw(target: &[u8; TRUNCATED_HASHBYTES], tag: u8) -> Vec<u8> {
    let mut pr_data = Vec::new();
    pr_data.extend_from_slice(target);
    pr_data.extend_from_slice(&[tag; TRUNCATED_HASHBYTES]);
    let request = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Plain,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: PATH_REQUEST_DEST_KAT,
        context: PacketContext::None,
        data: PacketData::Owned(pr_data),
    };
    let mut buf = [0u8; crate::constants::MTU];
    let len = request.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// All bytes the node wants to put on the wire this step.
fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

/// Every outbound announce whose destination field is `target`.
fn announces_for(frames: &[Vec<u8>], target: &[u8; TRUNCATED_HASHBYTES]) -> usize {
    frames
        .iter()
        .filter_map(|raw| Packet::unpack(raw).ok())
        .filter(|p| p.flags.packet_type == PacketType::Announce && &p.destination_hash == target)
        .count()
}

/// The manual announce API must refuse the explicit-hash destination.
#[test]
fn announce_api_refuses_explicit_hash() {
    let (mut node, _iface) = make_node();
    register_explicit_dest(&mut node);

    let result = node.announce_destination(&crate::DestinationHash::new(EXPLICIT_HASH), None);
    assert!(
        matches!(result, Err(AnnounceError::ExplicitHashCannotAnnounce)),
        "announce_destination for an explicit-hash destination must refuse \
         (Identity.py:584-587 rejects the mismatched hash on every Python \
         peer), got {result:?}",
    );
}

/// A path request for the explicit hash is answered with silence, while a
/// path request for a normal destination on the SAME node is answered with a
/// fresh path-response announce (positive control: the silence is the guard,
/// not a broken responder).
#[test]
fn path_request_for_explicit_hash_gets_silence() {
    let (mut node, iface) = make_node();
    register_explicit_dest(&mut node);
    let normal_hash = register_normal_dest(&mut node);

    let _ = node.handle_packet(InterfaceId(iface), &path_request_raw(&EXPLICIT_HASH, 0xC4));
    let _ = node.handle_packet(
        InterfaceId(iface),
        &path_request_raw(normal_hash.as_bytes(), 0xC5),
    );

    // Both answers would be scheduled behind the path-request grace.
    let now = node.transport().clock().now_ms();
    node.transport()
        .clock()
        .set(now + crate::constants::PATH_REQUEST_GRACE_MS + 1);
    let out = node.handle_timeout();
    let frames = action_data(&out);

    assert_eq!(
        announces_for(&frames, normal_hash.as_bytes()),
        1,
        "positive control: the normal destination must answer its path \
         request (Transport.py:2940 regenerates unconditionally)",
    );
    assert_eq!(
        announces_for(&frames, &EXPLICIT_HASH),
        0,
        "an explicit-hash destination must answer a path request with \
         SILENCE: a path-response announce for it would carry a hash no \
         Python peer can validate (Identity.py:584-587)",
    );
}

/// Beyond the deferred window: no later poll may emit an announce for the
/// explicit hash either (retry machinery must not have scheduled anything).
#[test]
fn no_deferred_announce_for_explicit_hash_after_path_request() {
    let (mut node, iface) = make_node();
    register_explicit_dest(&mut node);

    let _ = node.handle_packet(InterfaceId(iface), &path_request_raw(&EXPLICIT_HASH, 0xC6));

    for step in 1..=5u64 {
        let now = node.transport().clock().now_ms();
        node.transport().clock().set(now + step * 10_000);
        let out = node.handle_timeout();
        assert_eq!(
            announces_for(&action_data(&out), &EXPLICIT_HASH),
            0,
            "no poll after the path request may leak an announce for the \
             explicit hash",
        );
    }
}

/// The explicit-hash destination stays an ordinary Single destination for
/// everything that is not an announce: a LINK_REQUEST addressed to the
/// explicit hash resolves by table lookup and proves with the real identity.
#[test]
fn link_request_to_explicit_hash_establishes() {
    let (mut node, iface) = make_node();
    let signing_key = register_explicit_dest(&mut node);

    // Consenting initiator: knows the hash and the identity out-of-band.
    let mut link = Link::new_outgoing(crate::DestinationHash::new(EXPLICIT_HASH), &mut OsRng);
    link.set_destination_keys(&signing_key).unwrap();
    let request_data = link.create_link_request();

    let flags = PacketFlags {
        ifac_flag: false,
        header_type: HeaderType::Type1,
        context_flag: false,
        transport_type: TransportType::Broadcast,
        dest_type: DestinationType::Single,
        packet_type: PacketType::LinkRequest,
    };
    let mut raw = Vec::new();
    raw.push(flags.to_byte());
    raw.push(0); // hops
    raw.extend_from_slice(&EXPLICIT_HASH);
    raw.push(PacketContext::None.to_byte());
    raw.extend_from_slice(&request_data);
    let link_id = Link::calculate_link_id(&raw);
    link.set_link_id(link_id);

    let out = node.handle_packet(InterfaceId(iface), &raw);
    let frames = action_data(&out);
    let proof = frames
        .iter()
        .filter_map(|f| Packet::unpack(f).ok())
        .find(|p| {
            p.flags.packet_type == PacketType::Proof && p.destination_hash == *link_id.as_bytes()
        })
        .expect(
            "the responder must prove a link request addressed to the \
             explicit hash (bare table lookup, real identity crypto)",
        );

    link.process_proof(proof.data.as_slice())
        .expect("initiator must verify the LRPROOF against the real identity");
    assert_eq!(link.state(), LinkState::Active);
}
