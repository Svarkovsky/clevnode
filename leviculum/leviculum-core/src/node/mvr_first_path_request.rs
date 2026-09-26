//! #169 mvr: a local destination that has NEVER announced must answer its
//! FIRST path request.
//!
//! The reference answers a path request for a local destination
//! unconditionally by regenerating the announce
//! (`Transport.path_request`, Transport.py:2938-2941 calls
//! `destination.announce(path_response=True)`) — there is no
//! announce-history precondition. The sibling pin
//! `own_destination_path_response_is_a_fresh_regeneration`
//! (mvr_generated_field_pins.rs) covers the post-announce case and is NOT a
//! substitute for this one: the defect (#169) was that the response
//! scheduling was gated on an announce-cache entry, so a destination that
//! had not announced since process start answered its first request with
//! silence, and only the retry — served from the cache the first request
//! populated — succeeded. The announce cache is not persisted, so before
//! the fix this recurred after every daemon restart.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::TRUNCATED_HASHBYTES;
use crate::destination::{Destination, DestinationType, Direction};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId};

/// KAT from the vendored reference:
/// RNS.Destination.hash(None, "rnstransport", "path", "request").
const PATH_REQUEST_DEST_KAT: [u8; TRUNCATED_HASHBYTES] = [
    0x6b, 0x9f, 0x66, 0x01, 0x4d, 0x98, 0x53, 0xfa, 0xab, 0x22, 0x0f, 0xba, 0x47, 0xd0, 0x27, 0x61,
];

#[test]
fn first_path_request_before_any_announce_is_answered() {
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node: NodeCore<OsRng, MockClock, MemoryStorage> =
        NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults());
    let iface = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new("if0", 0)));
    node.set_interface_name(iface, String::from("if0"));

    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "firstpr",
        &["never", "announced"],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    // Deliberately NO announce_destination() call: the destination has never
    // announced, the announce cache is empty.

    let mut pr_data = Vec::new();
    pr_data.extend_from_slice(dest_hash.as_bytes());
    pr_data.extend_from_slice(&[0xC4u8; TRUNCATED_HASHBYTES]); // tag
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
    let _ = node.handle_packet(InterfaceId(iface), &buf[..len]);

    // The answer is deferred by the path-request grace, then targeted at the
    // requesting interface — same schedule as the post-announce case.
    let now = node.transport().clock().now_ms();
    node.transport()
        .clock()
        .set(now + crate::constants::PATH_REQUEST_GRACE_MS + 1);
    let out = node.handle_timeout();
    let response = out
        .actions
        .iter()
        .find_map(|a| match a {
            Action::SendPacket { iface: i, data, .. } if i.0 == iface => Some(data.clone()),
            _ => None,
        })
        .expect(
            "the FIRST path request for a never-announced local destination \
             must be answered (Transport.py:2938-2941 regenerates \
             unconditionally); silence here is Codeberg #169",
        );

    let parsed = Packet::unpack(&response).unwrap();
    assert_eq!(parsed.flags.packet_type, PacketType::Announce);
    assert_eq!(
        parsed.context,
        PacketContext::PathResponse,
        "peers suppress rebroadcast on this context byte (Transport.py:1886)"
    );
    assert_eq!(
        parsed.flags.header_type,
        HeaderType::Type1,
        "an origin's own announce is not transport-routed"
    );
    assert_eq!(parsed.hops, 0, "the origin's own hop count is zero");
    assert_eq!(
        &parsed.destination_hash,
        dest_hash.as_bytes(),
        "the response must announce the requested destination"
    );
}
