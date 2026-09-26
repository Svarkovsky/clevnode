//! #171 mvr: a path request for a destination hosted on a shared-instance
//! client must be answered with the CLIENT's fresh response, not the cache.
//!
//! The reference remembers the requesting interface in
//! `Transport.pending_local_path_requests` when the requested destination's
//! path points at a local client interface (`Transport.path_request`,
//! Transport.py:2924-2932), and when the client's own PATH_RESPONSE arrives
//! it is scheduled for one immediate rebroadcast that reaches the requester
//! (Transport.py:1910-1930, fired by the job loop at :589-622 as a plain
//! announce: `block_rebroadcasts` stays False, `attached_interface` None).
//! The client only sees the request in the first place because the reference
//! forwards path requests to local clients when no other branch answered
//! (Transport.py:3043-3048).
//!
//! We answered from the cached announce (cases 2a/2b of
//! `handle_path_request`) and never forwarded the client's fresh response.
//! Where cache and fresh response differ — rotated ratchet, changed
//! app_data, newer emission timestamp — the requester learned stale data.
//! The emission timestamp is the discriminator the cache genuinely cannot
//! fake: the cached raw bytes carry the OLD emission seconds
//! (payload[74..84] is the random hash, its last 5 bytes the emission
//! timestamp, Transport.py:1755 + :1799), and a stale value can lose the
//! newer-emission comparison at the requester against a copy it already
//! holds, so the request fails while looking answered.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::announce::build_announce_payload;
use crate::constants::{MTU, PATH_REQUEST_GRACE_MS, TRUNCATED_HASHBYTES};
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

/// Emission seconds baked into the client's REGISTRATION announce (the copy
/// the shared instance caches).
const CACHED_EMISSION_SECS: u64 = 5_000;
/// Emission seconds baked into the client's fresh PATH_RESPONSE. Strictly
/// newer than the cached copy — the field the cache cannot fake.
const FRESH_EMISSION_SECS: u64 = 6_000;

type Node = NodeCore<OsRng, MockClock, MemoryStorage>;

/// Shared instance with one network interface and one local client
/// interface.
fn make_shared_instance(enable_transport: bool) -> (Node, usize, usize) {
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node: Node = NodeCoreBuilder::new()
        .enable_transport(enable_transport)
        .build(OsRng, clock, MemoryStorage::with_defaults());
    let net = node
        .transport
        .register_interface(Box::new(MockInterface::new("net", 0)));
    node.set_interface_name(net, String::from("tcp_server/0.0.0.0:4242"));
    let client = node
        .transport
        .register_interface(Box::new(MockInterface::new("client", 1)));
    node.set_interface_name(client, String::from("Local[rns/default]/0"));
    node.transport.set_local_client(client, true);
    (node, net, client)
}

/// The destination the local client hosts. We hold its identity, playing
/// the client side of the IPC.
fn make_client_destination() -> Destination {
    let identity = Identity::generate(&mut OsRng);
    Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "pendingpr",
        &["client", "hosted"],
    )
    .unwrap()
}

/// Raw announce bytes as the client emits them over the IPC: Header1,
/// hops=0, with a controlled emission timestamp in the random hash.
fn make_client_announce(dest: &Destination, emission_secs: u64, context: PacketContext) -> Vec<u8> {
    let identity = dest.identity().unwrap();
    let payload = build_announce_payload(
        identity,
        dest.hash().as_bytes(),
        dest.name_hash(),
        None,
        Some(b"fresh-app-data"),
        &mut OsRng,
        emission_secs,
    )
    .unwrap();
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Announce,
        },
        hops: 0,
        transport_id: None,
        destination_hash: dest.hash().into_bytes(),
        context,
        data: PacketData::Owned(payload),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// 32-byte path request payload (dest_hash + tag) from a non-transport
/// network peer.
fn make_path_request(target: &[u8; TRUNCATED_HASHBYTES], tag_byte: u8) -> Vec<u8> {
    let mut pr_data = Vec::new();
    pr_data.extend_from_slice(target);
    pr_data.extend_from_slice(&[tag_byte; TRUNCATED_HASHBYTES]);
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
        destination_hash: PATH_REQUEST_DEST_KAT,
        context: PacketContext::None,
        data: PacketData::Owned(pr_data),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// Independent recomposition of the emission timestamp from raw announce
/// bytes: payload offset 64+10 is the 10-byte random hash
/// (`Transport.py:1755`), whose last 5 bytes are the big-endian emission
/// seconds (`Transport.py:1799`). Deliberately does NOT call the writer's
/// helper (wire-field-semantics rule).
fn emission_secs_of(raw: &[u8]) -> Option<u64> {
    let parsed = Packet::unpack(raw).ok()?;
    if parsed.flags.packet_type != PacketType::Announce {
        return None;
    }
    let payload = parsed.data.as_slice();
    let ts = payload.get(64 + 10 + 5..64 + 10 + 10)?;
    Some(
        ((ts[0] as u64) << 32)
            | ((ts[1] as u64) << 24)
            | ((ts[2] as u64) << 16)
            | ((ts[3] as u64) << 8)
            | (ts[4] as u64),
    )
}

/// Emission timestamps of every announce for `dest` that reaches the given
/// network interface, across SendPacket and Broadcast actions.
fn emissions_reaching(
    actions: &[Action],
    iface: usize,
    dest: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<u64> {
    let mut out = Vec::new();
    for action in actions {
        let data = match action {
            Action::SendPacket { iface: i, data, .. } if i.0 == iface => data,
            Action::Broadcast {
                data,
                exclude_iface,
                exclude_ifaces,
            } if exclude_iface.map(|e| e.0) != Some(iface)
                && !exclude_ifaces.iter().any(|e| e.0 == iface) =>
            {
                data
            }
            _ => continue,
        };
        if let Ok(parsed) = Packet::unpack(data) {
            if parsed.destination_hash == *dest {
                if let Some(secs) = emission_secs_of(data) {
                    out.push(secs);
                }
            }
        }
    }
    out
}

/// Register the client's destination on the shared instance: deliver the
/// registration announce, then advance well past the local-registration
/// delay and the announce rate limit and drain the resulting one-shot
/// rebroadcast.
fn register_client_destination(node: &mut Node, client: usize, dest: &Destination) {
    let raw = make_client_announce(dest, CACHED_EMISSION_SECS, PacketContext::None);
    let _ = node.handle_packet(InterfaceId(client), &raw);
    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 5_000);
    let _ = node.handle_timeout();
}

/// The #171 red test: a network peer requests the path to a client-hosted
/// destination; the client's fresh PATH_RESPONSE arrives within the response
/// grace. The requester must receive the FRESH announce
/// (Transport.py:1910-1930) — and must NOT be served the stale cached copy,
/// which can lose the newer-emission comparison at the requester and make
/// the answered request fail.
#[test]
fn network_request_for_client_destination_gets_the_clients_fresh_response() {
    let (mut node, net, client) = make_shared_instance(true);
    let dest = make_client_destination();
    let dest_hash = dest.hash().into_bytes();
    register_client_destination(&mut node, client, &dest);

    let mut actions: Vec<Action> = Vec::new();
    assert_eq!(node.transport().pending_local_path_request_count(), 0);

    // Path request from the network peer.
    let request = make_path_request(&dest_hash, 0xC1);
    let out = node.handle_packet(InterfaceId(net), &request);
    actions.extend(out.actions);
    assert_eq!(
        node.transport().pending_local_path_request_count(),
        1,
        "the requesting interface must be recorded against the destination \
         (Transport.py:2926-2932)"
    );

    // The client's fresh PATH_RESPONSE arrives over the IPC 50 ms later —
    // well inside the PATH_REQUEST_GRACE the cached answer waits out.
    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 50);
    let fresh = make_client_announce(&dest, FRESH_EMISSION_SECS, PacketContext::PathResponse);
    let out = node.handle_packet(InterfaceId(client), &fresh);
    actions.extend(out.actions);
    assert_eq!(
        node.transport().pending_local_path_request_count(),
        0,
        "the arriving PATH_RESPONSE must consume the pending entry \
         (Transport.py:1916 pops it)"
    );

    // Drive the scheduler: once right after the response, once past the
    // grace (where the stale cached entry would fire), once much later.
    for delta in [10, PATH_REQUEST_GRACE_MS + 100, 5_000] {
        let now = node.transport().clock().now_ms();
        node.transport().clock().set(now + delta);
        let out = node.handle_timeout();
        actions.extend(out.actions);
    }

    let emissions = emissions_reaching(&actions, net, &dest_hash);
    assert!(
        emissions.contains(&FRESH_EMISSION_SECS),
        "the requester must receive the CLIENT's fresh response \
         (Transport.py:2924-2932 records the requesting interface, \
         :1910-1930 forwards the client's PATH_RESPONSE when it arrives); \
         emissions actually sent toward the requester: {emissions:?}"
    );
    assert!(
        !emissions.contains(&CACHED_EMISSION_SECS),
        "the stale cached copy must not answer once the client's fresh \
         response arrived within the grace: a stale emission can lose the \
         newer-emission comparison at the requester, so the request would \
         fail while looking answered; emissions: {emissions:?}"
    );
}

/// Companion red test, the path that lets the client see the request at
/// all: a shared instance with transport DISABLED (the stock `rnsd`
/// drop-in configuration) must forward a network path request to its local
/// clients (Transport.py:3043-3048). Without this the request is dropped
/// outright and the pending-forward mechanism above is unreachable.
#[test]
fn non_transport_shared_instance_forwards_path_request_to_local_clients() {
    let (mut node, net, client) = make_shared_instance(false);
    let dest = make_client_destination();
    let dest_hash = dest.hash().into_bytes();
    register_client_destination(&mut node, client, &dest);

    let request = make_path_request(&dest_hash, 0xC2);
    let out = node.handle_packet(InterfaceId(net), &request);

    let forwarded_to_client = out.actions.iter().any(|a| match a {
        Action::SendPacket { iface, data, .. } if iface.0 == client => Packet::unpack(data)
            .map(|p| {
                p.destination_hash == PATH_REQUEST_DEST_KAT
                    && p.data.as_slice().starts_with(&dest_hash)
            })
            .unwrap_or(false),
        _ => false,
    });
    assert!(
        forwarded_to_client,
        "a non-transport shared instance must forward the path request to \
         its local clients (Transport.py:3043-3048); dropping it leaves \
         client-hosted destinations unreachable from the network"
    );
}

/// The full non-transport chain — the stock `rnsd` drop-in configuration
/// where the reference mechanism actually operates end to end: request in
/// from the network, forwarded to the client (Transport.py:3043-3048),
/// client's fresh PATH_RESPONSE back, forwarded to the requester
/// (Transport.py:1910-1930) as a PLAIN announce (`block_rebroadcasts`
/// stays False at :1871, so the job loop stamps context NONE, :596-597).
/// Without transport there is no cached-answer branch at all, so ONLY the
/// fresh response may reach the requester — the reference agrees exactly.
#[test]
fn non_transport_full_chain_serves_fresh_response_to_requester() {
    let (mut node, net, client) = make_shared_instance(false);
    let dest = make_client_destination();
    let dest_hash = dest.hash().into_bytes();
    register_client_destination(&mut node, client, &dest);

    let mut actions: Vec<Action> = Vec::new();
    let request = make_path_request(&dest_hash, 0xC3);
    let out = node.handle_packet(InterfaceId(net), &request);
    actions.extend(out.actions);

    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 50);
    let fresh = make_client_announce(&dest, FRESH_EMISSION_SECS, PacketContext::PathResponse);
    let out = node.handle_packet(InterfaceId(client), &fresh);
    actions.extend(out.actions);

    for delta in [10, PATH_REQUEST_GRACE_MS + 100, 5_000] {
        let now = node.transport().clock().now_ms();
        node.transport().clock().set(now + delta);
        let out = node.handle_timeout();
        actions.extend(out.actions);
    }

    let emissions = emissions_reaching(&actions, net, &dest_hash);
    assert!(
        emissions.contains(&FRESH_EMISSION_SECS),
        "non-transport shared instance must relay the client's fresh \
         response to the requester (Transport.py:1910-1930); emissions: \
         {emissions:?}"
    );
    assert!(
        !emissions.contains(&CACHED_EMISSION_SECS),
        "without transport there is no cached-answer branch; only the fresh \
         response may go out (reference branch order, Transport.py:2943); \
         emissions: {emissions:?}"
    );

    // The forwarded response is a plain announce, not PATH_RESPONSE
    // context: the reference's pending branch leaves block_rebroadcasts
    // False (Transport.py:1871), so the rebroadcast is stamped context
    // NONE (:596-597).
    let forwarded = actions
        .iter()
        .find_map(|a| {
            let data = match a {
                Action::SendPacket { iface, data, .. } if iface.0 == net => data,
                Action::Broadcast { data, .. } => data,
                _ => return None,
            };
            let parsed = Packet::unpack(data).ok()?;
            (parsed.destination_hash == dest_hash
                && emission_secs_of(data) == Some(FRESH_EMISSION_SECS))
            .then_some(parsed)
        })
        .expect("fresh response packet located above");
    assert_eq!(
        forwarded.context,
        PacketContext::None,
        "the pending forward goes out as a plain announce \
         (Transport.py:1871 + :596-597)"
    );
}

/// Adverse case: the client never answers. The pending entry must expire
/// instead of leaking (deliberate bounded-map deviation, see
/// `PENDING_LOCAL_PR_EXPIRY_MS`; the reference culls only on interface
/// departure, Transport.py:645-655), and the requester still gets the
/// cached answer at the grace — today's behaviour as graceful fallback.
#[test]
fn pending_entry_expires_and_cache_answers_when_client_is_silent() {
    let (mut node, net, client) = make_shared_instance(true);
    let dest = make_client_destination();
    let dest_hash = dest.hash().into_bytes();
    register_client_destination(&mut node, client, &dest);

    let request = make_path_request(&dest_hash, 0xC4);
    let _ = node.handle_packet(InterfaceId(net), &request);
    assert_eq!(node.transport().pending_local_path_request_count(), 1);

    // Cached answer fires at the grace.
    let now = node.transport().clock().now_ms();
    node.transport()
        .clock()
        .set(now + PATH_REQUEST_GRACE_MS + 100);
    let out = node.handle_timeout();
    let emissions = emissions_reaching(&out.actions, net, &dest_hash);
    assert!(
        emissions.contains(&CACHED_EMISSION_SECS),
        "with the client silent, the cached copy must still answer at the \
         grace (fallback); emissions: {emissions:?}"
    );
    assert_eq!(
        node.transport().pending_local_path_request_count(),
        1,
        "the pending entry outlives the cached fallback answer — the fresh \
         response may still arrive and supersede it at the requester"
    );

    // ... and the entry expires instead of leaking.
    let now = node.transport().clock().now_ms();
    node.transport()
        .clock()
        .set(now + crate::constants::PENDING_LOCAL_PR_EXPIRY_MS + 100);
    let _ = node.handle_timeout();
    assert_eq!(
        node.transport().pending_local_path_request_count(),
        0,
        "an unanswered pending entry must expire, not leak"
    );
}

/// Adverse case: two network peers request the same client destination.
/// Like the reference, the pending slot is one per destination (a dict
/// keyed by destination hash, Transport.py:2932 overwrites) — but the
/// forwarded fresh response goes out with `attached_interface` None
/// (:1930), i.e. as a broadcast, so BOTH requesters receive it.
#[test]
fn two_requesters_both_learn_the_fresh_response() {
    let (mut node, net, client) = make_shared_instance(true);
    let net2 = node
        .transport
        .register_interface(Box::new(MockInterface::new("net2", 2)));
    node.set_interface_name(net2, String::from("tcp_server/0.0.0.0:4243"));
    let dest = make_client_destination();
    let dest_hash = dest.hash().into_bytes();
    register_client_destination(&mut node, client, &dest);

    let mut actions: Vec<Action> = Vec::new();
    let out = node.handle_packet(InterfaceId(net), &make_path_request(&dest_hash, 0xC5));
    actions.extend(out.actions);
    let out = node.handle_packet(InterfaceId(net2), &make_path_request(&dest_hash, 0xC6));
    actions.extend(out.actions);
    assert_eq!(
        node.transport().pending_local_path_request_count(),
        1,
        "one pending slot per destination, last requester wins the slot \
         (Transport.py:2932 dict overwrite) — the broadcast forward serves \
         both regardless"
    );

    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 50);
    let fresh = make_client_announce(&dest, FRESH_EMISSION_SECS, PacketContext::PathResponse);
    let out = node.handle_packet(InterfaceId(client), &fresh);
    actions.extend(out.actions);

    for delta in [10, PATH_REQUEST_GRACE_MS + 100, 5_000] {
        let now = node.transport().clock().now_ms();
        node.transport().clock().set(now + delta);
        let out = node.handle_timeout();
        actions.extend(out.actions);
    }

    for (iface, name) in [(net, "first requester"), (net2, "second requester")] {
        let emissions = emissions_reaching(&actions, iface, &dest_hash);
        assert!(
            emissions.contains(&FRESH_EMISSION_SECS),
            "{name} must receive the fresh response via the broadcast \
             forward (attached_interface None, Transport.py:1930); \
             emissions: {emissions:?}"
        );
    }
    // NOTE deliberately unasserted: the first requester may additionally
    // receive the first request's cached targeted response, restored from
    // the #170 held slot after the fresh forward fires. The reference
    // behaves identically (held_announces dance, Transport.py:2991-2999 +
    // :630-633), and the stale copy loses the newer-emission comparison at
    // the requester.
}

/// Adverse case: the requesting interface goes down before the client
/// answers. The pending entry is culled with it — the reference's only
/// cleanup of this map (Transport.py:645-655).
#[test]
fn requester_interface_down_culls_pending_entry() {
    let (mut node, net, client) = make_shared_instance(true);
    let dest = make_client_destination();
    let dest_hash = dest.hash().into_bytes();
    register_client_destination(&mut node, client, &dest);

    let _ = node.handle_packet(InterfaceId(net), &make_path_request(&dest_hash, 0xC7));
    assert_eq!(node.transport().pending_local_path_request_count(), 1);

    let _ = node.handle_interface_down(InterfaceId(net));
    assert_eq!(
        node.transport().pending_local_path_request_count(),
        0,
        "an entry whose requesting interface disappeared must be culled \
         (Transport.py:645-655)"
    );
}

/// Adverse case: the client disconnects between request and response. The
/// fresh response can no longer arrive; the already-scheduled cached
/// answer still serves the requester, and the pending entry drains via
/// the time bound.
#[test]
fn client_disconnect_after_request_still_answers_from_cache() {
    let (mut node, net, client) = make_shared_instance(true);
    let dest = make_client_destination();
    let dest_hash = dest.hash().into_bytes();
    register_client_destination(&mut node, client, &dest);

    let _ = node.handle_packet(InterfaceId(net), &make_path_request(&dest_hash, 0xC8));
    assert_eq!(node.transport().pending_local_path_request_count(), 1);

    let _ = node.handle_interface_down(InterfaceId(client));

    let now = node.transport().clock().now_ms();
    node.transport()
        .clock()
        .set(now + PATH_REQUEST_GRACE_MS + 100);
    let out = node.handle_timeout();
    let emissions = emissions_reaching(&out.actions, net, &dest_hash);
    assert!(
        emissions.contains(&CACHED_EMISSION_SECS),
        "the cached answer scheduled before the client vanished must still \
         serve the requester; emissions: {emissions:?}"
    );

    let now = node.transport().clock().now_ms();
    node.transport()
        .clock()
        .set(now + crate::constants::PENDING_LOCAL_PR_EXPIRY_MS + 100);
    let _ = node.handle_timeout();
    assert_eq!(
        node.transport().pending_local_path_request_count(),
        0,
        "with the client gone, the pending entry drains via the time bound"
    );
}

/// Deliberate non-behaviour, pinned per
/// `docs/src/concepts/wire-field-semantics.md` ("deliberate non-behaviours
/// get pins too"): we do NOT skip the response grace when the next hop
/// toward the requested destination sits on a local-client interface.
///
/// The reference does skip it. `Transport.path_request` checks
/// `Transport.is_local_client_interface(Transport.next_hop_interface(
/// destination_hash))` and sets `retransmit_timeout = now`, logging
/// "destination is on a local client interface, rebroadcasting immediately"
/// (Transport.py:2978-2981). We apply the full `PATH_REQUEST_GRACE` in that
/// case, exactly as for any other next hop (Codeberg #172).
///
/// Why the deviation is the better behaviour here, and not merely different:
/// this is precisely the case in which the destination's own host — our
/// shared-instance client — is about to answer for itself. #171 forwards the
/// request to that client, and the client's fresh PATH_RESPONSE displaces the
/// cached entry inside the grace window; the sibling test
/// `network_request_for_client_destination_gets_the_clients_fresh_response`
/// pins that the requester then gets the FRESH announce and not the stale
/// cached copy. Firing immediately would put the cached copy on the wire
/// first, whose older emission timestamp can lose the newer-emission
/// comparison at the requester (#155) and fail a request that looks
/// answered. The deviation rule holds on all three counts: the packet is
/// byte-identical (timing only), the requester is still answered far inside
/// its path-request timeout, and it measurably improves what the requester
/// ends up believing.
///
/// If a later change makes the grace unnecessary here, this test is the
/// place to argue with — not a silent edit.
#[test]
fn client_hosted_destination_keeps_the_response_grace() {
    let (mut node, net, client) = make_shared_instance(true);
    let dest = make_client_destination();
    let dest_hash = dest.hash().into_bytes();
    register_client_destination(&mut node, client, &dest);

    let mut actions: Vec<Action> = Vec::new();
    let out = node.handle_packet(InterfaceId(net), &make_path_request(&dest_hash, 0xD7));
    actions.extend(out.actions);

    // Inside the grace nothing has answered yet. With the reference's
    // `retransmit_timeout = now` the cached copy would already be out.
    let now = node.transport().clock().now_ms();
    node.transport()
        .clock()
        .set(now + PATH_REQUEST_GRACE_MS - 10);
    let out = node.handle_timeout();
    actions.extend(out.actions);
    let early = emissions_reaching(&actions, net, &dest_hash);
    assert!(
        early.is_empty(),
        "the answer for a client-hosted destination waits out the grace; \
         emissions already sent toward the requester: {early:?}"
    );

    // Past the grace it does fire — the check above could go red, it is not
    // a check that can never fail (evidence-and-honesty).
    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 20);
    let out = node.handle_timeout();
    actions.extend(out.actions);
    let late = emissions_reaching(&actions, net, &dest_hash);
    assert_eq!(
        late,
        std::vec![CACHED_EMISSION_SECS],
        "once the grace elapsed with no fresh client response, the cached \
         answer serves the requester"
    );
}
