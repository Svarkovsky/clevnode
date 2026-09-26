//! #170 mvr: a path request must NOT cancel a pending announce rebroadcast.
//!
//! ## The defect
//!
//! When a transport node answers a path request for a destination whose
//! announce is still sitting in its rebroadcast grace, the reference HOLDS
//! the in-flight `announce_table` entry before inserting the path-response
//! entry and reinserts it once the response has been served
//! (`Transport.path_request`, Transport.py:2991-2999 holds;
//! `Transport.jobs`, Transport.py:630-633 reinserts at fire time). We do
//! not: the `set_announce` for the path response overwrites the pending
//! entry, so the targeted response to ONE interface silently replaces the
//! network-wide rebroadcast. Nothing observes the loss — the node cannot
//! see that a scheduled rebroadcast never went out, and the peers that
//! never learned the path cannot tell it from a lossy link.
//!
//! ## What is pinned
//!
//! Both transmissions AND their order: the reference reinserts the held
//! entry only when the response fires, so the targeted response precedes
//! the restored rebroadcast. A test that merely counted two transmissions
//! would stay green under a fix that reversed them.
//!
//! Adverse cases pinned here:
//! - two path requests for the same destination inside one grace window
//!   (transport relay AND local-destination variants),
//! - the request arriving on the same interface the pending announce
//!   arrived on,
//! - a pending entry whose due time expires while it is held (it must
//!   fire on the first scheduler pass after the response, not be lost).
//!
//! Sans-I/O: no LoRa, no Docker, no Python, sub-second wall clock.

extern crate std;

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
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId, TickOutput};

type TransportNode = NodeCore<OsRng, MockClock, MemoryStorage>;

fn add_iface(node: &mut TransportNode, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn make_transport_node() -> TransportNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        clock,
        MemoryStorage::with_defaults(),
    )
}

/// Build a destination D and one direct (wire hops 0) announce packet for it.
fn make_destination() -> (crate::DestinationHash, Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["annhold"],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    let ann = dest
        .announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000)
        .unwrap();
    let mut buf = [0u8; MTU];
    let len = ann.pack(&mut buf).unwrap();
    (dest_hash, buf[..len].to_vec())
}

/// Build a path-request packet addressed to `path_req_hash`, requesting `dest`.
fn build_path_request(
    path_req_hash: &[u8; TRUNCATED_HASHBYTES],
    dest: &[u8; TRUNCATED_HASHBYTES],
    requester_id: &[u8; TRUNCATED_HASHBYTES],
    tag: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<u8> {
    let mut data = Vec::with_capacity(48);
    data.extend_from_slice(dest);
    data.extend_from_slice(requester_id);
    data.extend_from_slice(tag);

    let packet = Packet {
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
        destination_hash: *path_req_hash,
        context: PacketContext::None,
        data: PacketData::Owned(data),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// Announces for `dest` sent as a TARGETED path response on `iface`.
fn targeted_responses(out: &TickOutput, iface: usize, dest: &[u8; TRUNCATED_HASHBYTES]) -> usize {
    out.actions
        .iter()
        .filter(|a| match a {
            Action::SendPacket { iface: i, data, .. } if i.0 == iface => Packet::unpack(data)
                .map(|p| {
                    p.flags.packet_type == PacketType::Announce
                        && p.context == PacketContext::PathResponse
                        && &p.destination_hash == dest
                })
                .unwrap_or(false),
            _ => false,
        })
        .count()
}

/// Broadcast (network-wide) announce rebroadcasts for `dest`: context None,
/// returned with the excluded interface.
fn broadcast_rebroadcasts(
    out: &TickOutput,
    dest: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<(Packet, Option<usize>)> {
    out.actions
        .iter()
        .filter_map(|a| match a {
            Action::Broadcast {
                data,
                exclude_iface,
                ..
            } => Packet::unpack(data).ok().and_then(|p| {
                (p.flags.packet_type == PacketType::Announce
                    && p.context == PacketContext::None
                    && &p.destination_hash == dest)
                    .then_some((p, exclude_iface.map(|i| i.0)))
            }),
            _ => None,
        })
        .collect()
}

/// Any announce for `dest` in the output, in any form.
fn any_announce_tx(out: &TickOutput, dest: &[u8; TRUNCATED_HASHBYTES]) -> usize {
    out.actions
        .iter()
        .filter(|a| {
            let data = match a {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
            };
            Packet::unpack(data)
                .map(|p| p.flags.packet_type == PacketType::Announce && &p.destination_hash == dest)
                .unwrap_or(false)
        })
        .count()
}

/// THE bug (#170): an announce pending rebroadcast on relay R is displaced by
/// a path response and the network-wide rebroadcast never goes out. The
/// reference serves the targeted response FIRST and the restored rebroadcast
/// AFTER it (hold: Transport.py:2991-2999, reinsert at response fire time:
/// Transport.py:630-633).
#[test]
fn path_response_precedes_and_preserves_pending_rebroadcast() {
    let (dest_d, announce_raw) = make_destination();
    let dest = *dest_d.as_bytes();

    let mut relay = make_transport_node();
    let iface_a = add_iface(&mut relay, "A_announce_in");
    let iface_b = add_iface(&mut relay, "B_requester");

    let t0 = relay.transport().clock().now_ms();

    // The announce arrives on A and is queued for network-wide rebroadcast
    // (retransmit within the jitter window).
    let _ = relay.handle_packet(InterfaceId(iface_a), &announce_raw);

    // Inside the grace window, B requests the path for D.
    let path_req_hash = *relay.transport().path_request_hash();
    let request = build_path_request(&path_req_hash, &dest, &[0xBB; 16], &[0xA1; 16]);
    let _ = relay.handle_packet(InterfaceId(iface_b), &request);

    // At grace expiry the TARGETED response fires first — and ONLY the
    // response: the pending rebroadcast is held, not racing it (order pin).
    relay
        .transport()
        .clock()
        .set(t0 + crate::constants::PATH_REQUEST_GRACE_MS + 1);
    let out = relay.handle_timeout();
    assert_eq!(
        targeted_responses(&out, iface_b, &dest),
        1,
        "the targeted path response must fire on the requesting interface \
         at grace expiry"
    );
    assert!(
        broadcast_rebroadcasts(&out, &dest).is_empty(),
        "the response goes FIRST: no network-wide rebroadcast may fire in \
         the same pass (Transport.py:630-633 reinserts only at response \
         fire time)"
    );

    // Afterwards the HELD rebroadcast still goes out to the whole network.
    relay.transport().clock().set(t0 + 100_000);
    let out = relay.handle_timeout();
    let broadcasts = broadcast_rebroadcasts(&out, &dest);
    assert_eq!(
        broadcasts.len(),
        1,
        "the pending network-wide rebroadcast must survive the path \
         response (Transport.py:2991-2999 holds the entry; on master the \
         response's set_announce clobbers it and the mesh never learns \
         the path) — Codeberg #170"
    );
    let (packet, exclude) = &broadcasts[0];
    assert_eq!(
        packet.hops, 1,
        "rebroadcast carries the receipt-incremented hop count"
    );
    assert_eq!(
        *exclude, None,
        "rebroadcast goes on ALL interfaces (Python Transport.outbound \
         iterates every interface and relies on packet_hashlist to absorb \
         the echo, Transport.py:1227)"
    );
}

/// Two path requests for the same destination inside one grace window: the
/// second must not lose the held rebroadcast the first displaced.
///
/// Deliberate deviation from the reference shape, pinned: Python overwrites
/// `held_announces[dest]` with the FIRST response entry when the second
/// request arrives (Transport.py:2997-2999), so the network-wide rebroadcast
/// is lost and only the two targeted responses go out. We keep the genuine
/// rebroadcast in the held slot instead and drop the displaced first
/// response: the restored broadcast goes on ALL interfaces, so the first
/// requester's interface receives it too — every peer Python serves is
/// still served AND the rest of the mesh learns the path. Wire format
/// untouched; measurably better propagation (deviation rule,
/// docs/src/concepts/python-rns-compatibility.md).
#[test]
fn second_request_in_grace_keeps_held_rebroadcast() {
    let (dest_d, announce_raw) = make_destination();
    let dest = *dest_d.as_bytes();

    let mut relay = make_transport_node();
    let iface_a = add_iface(&mut relay, "A_announce_in");
    let iface_b = add_iface(&mut relay, "B_requester1");
    let iface_c = add_iface(&mut relay, "C_requester2");

    let t0 = relay.transport().clock().now_ms();
    let _ = relay.handle_packet(InterfaceId(iface_a), &announce_raw);

    let path_req_hash = *relay.transport().path_request_hash();
    let req1 = build_path_request(&path_req_hash, &dest, &[0xB1; 16], &[0xA1; 16]);
    let req2 = build_path_request(&path_req_hash, &dest, &[0xC2; 16], &[0xA2; 16]);
    let _ = relay.handle_packet(InterfaceId(iface_b), &req1);
    let _ = relay.handle_packet(InterfaceId(iface_c), &req2);

    // The latest response fires at grace expiry, targeted at C.
    relay
        .transport()
        .clock()
        .set(t0 + crate::constants::PATH_REQUEST_GRACE_MS + 1);
    let out = relay.handle_timeout();
    assert_eq!(
        targeted_responses(&out, iface_c, &dest),
        1,
        "the second request's targeted response must fire"
    );

    // And the held rebroadcast still goes out afterwards — covering B, which
    // lost its targeted response to the second request.
    relay.transport().clock().set(t0 + 100_000);
    let out = relay.handle_timeout();
    let broadcasts = broadcast_rebroadcasts(&out, &dest);
    assert_eq!(
        broadcasts.len(),
        1,
        "a second path request inside the grace window must not lose the \
         held network-wide rebroadcast (Codeberg #170)"
    );
    assert_eq!(
        broadcasts[0].1, None,
        "the restored rebroadcast goes on all interfaces, so the first \
         requester's interface receives it"
    );
}

/// Two path requests for a LOCAL destination of ours: both requesters must be
/// answered. The pending entry here is a targeted response (Block A,
/// node/mod.rs), not a relayed rebroadcast — holding must preserve it so the
/// first requester is not starved. The reference answers every local-dest
/// request immediately (Transport.py:2938-2941 regenerates unconditionally),
/// so "answer count == request count" is the reference semantics; our
/// deferred schedule must not silently drop one.
#[test]
fn local_destination_two_requests_both_answered() {
    let mut node = make_transport_node();
    let iface_b = add_iface(&mut node, "B_requester1");
    let iface_c = add_iface(&mut node, "C_requester2");

    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["annhold", "local"],
    )
    .unwrap();
    let dest_hash = *dest.hash().as_bytes();
    node.register_destination(dest);

    let t0 = node.transport().clock().now_ms();
    let path_req_hash = *node.transport().path_request_hash();
    let req1 = build_path_request(&path_req_hash, &dest_hash, &[0xB1; 16], &[0xA1; 16]);
    let req2 = build_path_request(&path_req_hash, &dest_hash, &[0xC2; 16], &[0xA2; 16]);
    let _ = node.handle_packet(InterfaceId(iface_b), &req1);
    let _ = node.handle_packet(InterfaceId(iface_c), &req2);

    // The second (latest) response fires at its grace expiry, targeted at C;
    // the displaced first response is reinserted at that moment.
    node.transport()
        .clock()
        .set(t0 + crate::constants::PATH_REQUEST_GRACE_MS + 1);
    let out = node.handle_timeout();
    assert_eq!(
        targeted_responses(&out, iface_c, &dest_hash),
        1,
        "the second requester must get its targeted response"
    );

    // The reinserted first response (its grace long past) fires on the next
    // scheduler pass, targeted at B.
    node.transport()
        .clock()
        .set(t0 + crate::constants::PATH_REQUEST_GRACE_MS + 2);
    let out = node.handle_timeout();
    assert_eq!(
        targeted_responses(&out, iface_b, &dest_hash),
        1,
        "the FIRST requester must still be answered: a second request for a \
         local destination must not clobber the pending response \
         (Codeberg #170, Block A variant)"
    );
}

/// The request arrives on the SAME interface the pending announce arrived on.
/// The targeted response goes back on that interface; the restored
/// rebroadcast still goes out network-wide afterwards.
#[test]
fn request_on_announce_interface_still_rebroadcasts_elsewhere() {
    let (dest_d, announce_raw) = make_destination();
    let dest = *dest_d.as_bytes();

    let mut relay = make_transport_node();
    let iface_a = add_iface(&mut relay, "A_announce_and_request");
    let _iface_b = add_iface(&mut relay, "B_rest_of_mesh");

    let t0 = relay.transport().clock().now_ms();
    let _ = relay.handle_packet(InterfaceId(iface_a), &announce_raw);

    let path_req_hash = *relay.transport().path_request_hash();
    let request = build_path_request(&path_req_hash, &dest, &[0xBB; 16], &[0xA1; 16]);
    let _ = relay.handle_packet(InterfaceId(iface_a), &request);

    relay
        .transport()
        .clock()
        .set(t0 + crate::constants::PATH_REQUEST_GRACE_MS + 1);
    let out = relay.handle_timeout();
    assert_eq!(
        targeted_responses(&out, iface_a, &dest),
        1,
        "the targeted response fires on the shared interface"
    );

    relay.transport().clock().set(t0 + 100_000);
    let out = relay.handle_timeout();
    let broadcasts = broadcast_rebroadcasts(&out, &dest);
    assert_eq!(
        broadcasts.len(),
        1,
        "the rebroadcast must survive a request from the announce's own \
         arrival interface (Codeberg #170)"
    );
}

/// A pending entry whose due time expires while it is held: the announce's
/// retransmit was already due when the path request displaced it (request
/// delivered after the full jitter window, before any scheduler pass). The
/// held entry must fire on the FIRST scheduler pass after the response —
/// expired-while-held is not lost and does not jump the response.
#[test]
fn held_entry_expired_while_serving_fires_after_response() {
    let (dest_d, announce_raw) = make_destination();
    let dest = *dest_d.as_bytes();

    let mut relay = make_transport_node();
    let iface_a = add_iface(&mut relay, "A_announce_in");
    let iface_b = add_iface(&mut relay, "B_requester");

    let t0 = relay.transport().clock().now_ms();
    let _ = relay.handle_packet(InterfaceId(iface_a), &announce_raw);

    // Past the whole jitter window: the rebroadcast is DUE but no scheduler
    // pass has run. The request displaces an already-expired entry.
    let past_jitter = t0 + relay.transport().announce_jitter_max_ms() + 100;
    relay.transport().clock().set(past_jitter);
    let path_req_hash = *relay.transport().path_request_hash();
    let request = build_path_request(&path_req_hash, &dest, &[0xBB; 16], &[0xA1; 16]);
    let out = relay.handle_packet(InterfaceId(iface_b), &request);
    assert_eq!(
        any_announce_tx(&out, &dest),
        0,
        "serving the request emits nothing by itself"
    );

    // The response fires first, alone.
    relay
        .transport()
        .clock()
        .set(past_jitter + crate::constants::PATH_REQUEST_GRACE_MS + 1);
    let out = relay.handle_timeout();
    assert_eq!(
        targeted_responses(&out, iface_b, &dest),
        1,
        "the targeted response fires at grace expiry"
    );
    assert!(
        broadcast_rebroadcasts(&out, &dest).is_empty(),
        "the expired held entry must not jump ahead of the response"
    );

    // The expired held entry fires on the very next pass.
    relay
        .transport()
        .clock()
        .set(past_jitter + crate::constants::PATH_REQUEST_GRACE_MS + 2);
    let out = relay.handle_timeout();
    assert_eq!(
        broadcast_rebroadcasts(&out, &dest).len(),
        1,
        "an entry whose due time expired while held must fire on the first \
         scheduler pass after the response, not be lost (Codeberg #170)"
    );
}
