//! mvr: a path request must NOT be answered from an orphaned announce cache
//! (#117 residual).
//!
//! ## The defect
//!
//! `handle_path_request` (cases 2a and 2b, `transport.rs`) answers a path
//! request from the announce cache WITHOUT checking that the path table still
//! holds the destination. Dropping a path (`remove_path`) or letting it expire
//! (`expire_paths`) removes ONLY the path-table entry; the SEPARATE announce
//! cache stays intact. The daemon then answers the requester with a stale
//! announce and returns, never forwarding the request and never repopulating
//! its own path table. The client sees `has_path = true`, calls
//! `get_next_hop`, the daemon path table returns None, and the client fails
//! with "Invalid path data returned".
//!
//! Python (`Transport.py:2943`) gates the equivalent answer on
//! `destination_hash in Transport.path_table` and fetches the cached packet
//! from INSIDE the table entry, so a dropped path removes both atomically.
//!
//! ## Expected behavior (matches Python)
//!
//! With the path table entry gone but the announce cache intact:
//! - a LOCAL client's request is not answered; it falls through to case 4 and
//!   is forwarded to the network interfaces;
//! - a NETWORK peer's request on a discovering interface is not answered; it
//!   falls through to case 3 and re-originates discovery.
//!
//! With BOTH path table and cache present, the request is answered from the
//! cache exactly as before (positive control against over-forwarding).
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

fn add_iface(node: &mut TransportNode, name: &'static str, local_client: bool) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    if local_client {
        node.set_interface_local_client(idx, true);
    }
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

/// Build a destination X and one direct (wire 0) announce packet for it.
fn make_destination() -> (crate::DestinationHash, Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["orphancache"],
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
    dest: &crate::DestinationHash,
    requester_id: &[u8; TRUNCATED_HASHBYTES],
    tag: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<u8> {
    let mut data = Vec::with_capacity(48);
    data.extend_from_slice(dest.as_bytes());
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

/// True if `data` is an announce for `dest` (any header type).
fn is_announce_for(data: &[u8], dest: &crate::DestinationHash) -> bool {
    match Packet::unpack(data) {
        Ok(p) => {
            p.flags.packet_type == PacketType::Announce && p.destination_hash == *dest.as_bytes()
        }
        Err(_) => false,
    }
}

/// True if `data` is a path request whose requested destination is `dest`.
fn is_path_request_for(
    data: &[u8],
    path_req_hash: &[u8; TRUNCATED_HASHBYTES],
    dest: &crate::DestinationHash,
) -> bool {
    match Packet::unpack(data) {
        Ok(p) => {
            p.flags.packet_type == PacketType::Data
                && p.destination_hash == *path_req_hash
                && p.data.as_slice().len() >= TRUNCATED_HASHBYTES
                && &p.data.as_slice()[..TRUNCATED_HASHBYTES] == dest.as_bytes()
        }
        Err(_) => false,
    }
}

/// All targeted sends: (interface index, wire bytes).
fn send_packets(output: &TickOutput) -> Vec<(usize, Vec<u8>)> {
    output
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::SendPacket { iface, data, .. } => Some((iface.0, data.clone())),
            Action::Broadcast { .. } => None,
        })
        .collect()
}

/// All broadcasts: (excluded interface index, wire bytes).
fn broadcasts(output: &TickOutput) -> Vec<(Option<usize>, Vec<u8>)> {
    output
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Broadcast {
                data,
                exclude_iface,
                ..
            } => Some((exclude_iface.map(|i| i.0), data.clone())),
            Action::SendPacket { .. } => None,
        })
        .collect()
}

/// Node R (transport enabled) with a network interface A and a second
/// interface B (local client or network, per `b_local`). An announce for X
/// arrives on A, populating BOTH the path table and the announce cache; the
/// pending announce rebroadcast is drained so later assertions only see
/// path-request traffic.
fn setup_node(b_local: bool) -> (TransportNode, usize, usize, crate::DestinationHash) {
    let (dest_x, announce_raw) = make_destination();

    let mut node = make_transport_node();
    let iface_a = add_iface(&mut node, "A_network", false);
    let iface_b = add_iface(
        &mut node,
        if b_local { "B_local" } else { "B_network" },
        b_local,
    );

    let _ = node.handle_packet(InterfaceId(iface_a), &announce_raw);
    assert_eq!(
        node.hops_to(&dest_x),
        Some(1),
        "precondition: R records X at 1 hop via A"
    );

    // Drain the scheduled announce rebroadcast so it cannot be mistaken for a
    // path response later.
    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 100_000);
    let _ = node.handle_timeout();

    (node, iface_a, iface_b, dest_x)
}

/// THE bug (local requester): after `remove_path` orphans the announce cache,
/// a local client's path request must be FORWARDED to the network (case 4),
/// not answered with a stale announce from the cache (case 2a). RED on master:
/// case 2a answers stale and never forwards.
#[test]
fn orphaned_cache_local_request_forwards_instead_of_stale_answer() {
    let (mut node, iface_a, iface_b, dest_x) = setup_node(true);

    assert!(
        node.remove_path(dest_x.as_bytes()),
        "path entry must exist to remove"
    );
    assert_eq!(
        node.hops_to(&dest_x),
        None,
        "precondition: path table entry gone after remove_path"
    );

    let path_req_hash = *node.transport().path_request_hash();
    let requester_id = [0xBBu8; TRUNCATED_HASHBYTES];
    let tag = [0xAAu8; TRUNCATED_HASHBYTES];
    let request = build_path_request(&path_req_hash, &dest_x, &requester_id, &tag);

    let out = node.handle_packet(InterfaceId(iface_b), &request);
    let sends = send_packets(&out);

    // The request must be forwarded to the network interface A (case 4).
    assert!(
        sends
            .iter()
            .any(|(i, d)| *i == iface_a && is_path_request_for(d, &path_req_hash, &dest_x)),
        "the path request must be forwarded to network interface A; master \
         answers from the orphaned cache and never forwards"
    );

    // No stale announce back to the requesting local client B, neither
    // immediately (case 2a) ...
    assert!(
        !sends
            .iter()
            .any(|(i, d)| *i == iface_b && is_announce_for(d, &dest_x)),
        "no stale announce may be sent back to the local client B; master \
         answers with the orphaned cache entry"
    );

    // ... nor via a deferred rebroadcast (a case 2b style scheduled answer).
    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 100_000);
    let deferred = node.handle_timeout();
    assert!(
        !send_packets(&deferred)
            .iter()
            .any(|(i, d)| *i == iface_b && is_announce_for(d, &dest_x)),
        "no deferred stale announce may reach the local client B"
    );
}

/// #117 REGRESSION (Full-mode relay, no local clients): #117 gated the case 2b
/// cache answer on a live path entry. A `Full`-mode relay
/// (`discovers_paths() == false`) with NO local clients then hits neither case
/// 2b (path gone) nor the old case 3 (`active_discovery == false`,
/// `has_locals == false`) and SILENTLY DROPS a request whose path expired but
/// whose announce cache survives, breaking downstream discovery under load.
/// The fix re-originates a fresh hops=0 discovery for a destination we have
/// provably seen (a cached announce) but hold no path to, WITHOUT re-serving
/// the stale cache. RED on master (pre-fix): no re-originated broadcast, no
/// path entry, silent drop. GREEN after the fix: a case 3 broadcast excludes N.
#[test]
fn orphaned_cache_full_mode_relay_rediscovers_instead_of_silent_drop() {
    // setup_node(false) leaves iface_n in the default Full mode (no
    // set_interface_mode call): discovers_paths() == false, and there are no
    // local clients on this node.
    let (mut node, iface_a, iface_n, dest_x) = setup_node(false);
    assert!(
        !crate::traits::InterfaceMode::Full.discovers_paths(),
        "precondition: the requester interface is a non-discovering Full mode"
    );

    assert!(
        node.remove_path(dest_x.as_bytes()),
        "path entry must exist to remove"
    );
    assert_eq!(node.hops_to(&dest_x), None);

    let path_req_hash = *node.transport().path_request_hash();
    let requester_id = [0x11u8; TRUNCATED_HASHBYTES];
    let tag = [0x22u8; TRUNCATED_HASHBYTES];
    let request = build_path_request(&path_req_hash, &dest_x, &requester_id, &tag);

    let out = node.handle_packet(InterfaceId(iface_n), &request);

    // The fix re-originates discovery: a fresh path request is broadcast on all
    // interfaces except the requester N (reaching iface_a). RED on master: the
    // Full relay drops the request with no broadcast at all.
    assert!(
        broadcasts(&out)
            .iter()
            .any(|(excl, d)| *excl == Some(iface_n)
                && is_path_request_for(d, &path_req_hash, &dest_x)),
        "a Full-mode relay with a cached-but-pathless destination must \
         re-originate discovery (case 3); master silently drops the request"
    );
    let _ = iface_a;

    // The re-origination must NOT resurrect a path or answer from the stale
    // cache (guards against reintroducing the #117 bug): no path entry is
    // created, and no announce is sent toward the requester N, immediately ...
    assert_eq!(
        node.hops_to(&dest_x),
        None,
        "re-origination must not create a path table entry from the stale cache"
    );
    assert!(
        !send_packets(&out)
            .iter()
            .any(|(i, d)| *i == iface_n && is_announce_for(d, &dest_x)),
        "no stale announce may be served to the requester N"
    );
    // ... nor as a deferred case 2b rebroadcast.
    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 100_000);
    let deferred = node.handle_timeout();
    assert!(
        !send_packets(&deferred)
            .iter()
            .any(|(i, d)| *i == iface_n && is_announce_for(d, &dest_x)),
        "no deferred stale announce may reach the requester N"
    );
}

/// POSITIVE CONTROL (green before and after the fix): with BOTH the path table
/// and the announce cache present, the same local request is answered with an
/// announce on the requester interface B and NOT forwarded to A. Guards
/// against over-forwarding.
#[test]
fn intact_path_local_request_answers_and_does_not_forward() {
    let (mut node, iface_a, iface_b, dest_x) = setup_node(true);

    let path_req_hash = *node.transport().path_request_hash();
    let requester_id = [0xCCu8; TRUNCATED_HASHBYTES];
    let tag = [0xDDu8; TRUNCATED_HASHBYTES];
    let request = build_path_request(&path_req_hash, &dest_x, &requester_id, &tag);

    let out = node.handle_packet(InterfaceId(iface_b), &request);
    let sends = send_packets(&out);

    assert!(
        sends
            .iter()
            .any(|(i, d)| *i == iface_b && is_announce_for(d, &dest_x)),
        "with an intact path the local client must get the cached announce"
    );
    assert!(
        !sends
            .iter()
            .any(|(i, d)| *i == iface_a && is_path_request_for(d, &path_req_hash, &dest_x)),
        "with an intact path the request must not be forwarded to A"
    );
}

/// THE bug (network requester): after `remove_path` orphans the cache, a path
/// request from a network peer on a discovering (Gateway) interface must
/// re-originate discovery (case 3), not schedule a stale deferred answer from
/// the cache (case 2b). RED on master: case 2b answers stale and never
/// discovers.
#[test]
fn orphaned_cache_network_request_rediscovers_instead_of_stale_answer() {
    let (mut node, _iface_a, iface_n, dest_x) = setup_node(false);
    node.set_interface_mode(iface_n, crate::traits::InterfaceMode::Gateway);

    assert!(
        node.remove_path(dest_x.as_bytes()),
        "path entry must exist to remove"
    );
    assert_eq!(node.hops_to(&dest_x), None);

    let path_req_hash = *node.transport().path_request_hash();
    let requester_id = [0xEEu8; TRUNCATED_HASHBYTES];
    let tag = [0xFFu8; TRUNCATED_HASHBYTES];
    let request = build_path_request(&path_req_hash, &dest_x, &requester_id, &tag);

    let out = node.handle_packet(InterfaceId(iface_n), &request);

    // Case 3 re-originates a fresh path request on all interfaces except N.
    assert!(
        broadcasts(&out)
            .iter()
            .any(|(excl, d)| *excl == Some(iface_n)
                && is_path_request_for(d, &path_req_hash, &dest_x)),
        "the request must be re-originated toward the network (case 3); \
         master schedules a stale answer from the orphaned cache instead"
    );

    // No stale announce toward the requester N, neither immediately nor as the
    // deferred case 2b rebroadcast.
    assert!(
        !send_packets(&out)
            .iter()
            .any(|(i, d)| *i == iface_n && is_announce_for(d, &dest_x)),
        "no immediate stale announce may reach the network requester N"
    );
    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 100_000);
    let deferred = node.handle_timeout();
    assert!(
        !send_packets(&deferred)
            .iter()
            .any(|(i, d)| *i == iface_n && is_announce_for(d, &dest_x)),
        "no deferred stale announce may reach the network requester N; \
         master fires the case 2b scheduled answer here"
    );
}
