//! ble_reconnect mvr: a freshly rebooted transport node is handed traffic it
//! is the designated hop for, holds no path — does it stay silent?
//!
//! ## The observation this answers
//!
//! Rig scenario ble_reconnect, v10 run 2026-08-31T17-29 (ledger
//! `~/.claude/bugs/ble_reconnect-reboot-relay-amnesia.md`): the middle board
//! (pocket) is reset mid-scenario via the daemon route. Both BLE links
//! re-form within 10 s (`BLE_LINK_UP` on the host at 17:30:17.872, t114's
//! `BLE: connected` at t=71729). The host's cached 3-hop path to the far
//! probe destination is topologically still correct, so it forwards five
//! probes into the pocket — and every one dies there: the pocket rebooted
//! with `path_table_initial_len=0`, nothing re-announces the far destination
//! inside the 120 s window (only the rebooted board self-announces on boot),
//! and the pocket drops each probe as `NoPath` without telling anyone.
//! t114's only post-reset BLE RXes are the pocket's own 60 s periodic
//! announce — zero probes arrive. Sent 1 received 0, five times.
//!
//! ## What is pinned
//!
//! The transport rule that heals this without media awareness: a transport
//! node that drops a relayed packet (Data forward or LinkRequest) for lack
//! of a path SOLICITS the path — an ordinary hops=0 path-request broadcast
//! on all interfaces, rate-limited per destination by
//! `PATH_REQUEST_MIN_INTERVAL_MS`. The neighbour that still holds the path
//! (t114 here) answers with its cached announce; the probe retry ~27 s later
//! goes through. Python-RNS stays silent in this situation and waits for the
//! destination's next periodic announce; the deviation is wire- and
//! semantically compatible (an ordinary path request Python peers answer,
//! upstream loops broken by the requestor-is-next-hop guard,
//! `transport.rs` `handle_path_request`) and measurably improves Priority 1
//! — precedent: the #117 cached-announce re-origination block.
//!
//! The broadcast must include the ARRIVAL interface: the board's BLE is one
//! interface carrying both peers (host and t114), so an
//! all-except-the-requestor send would never reach the neighbour that can
//! answer.
//!
//! Controls pin the boundaries: overheard traffic (foreign transport id)
//! must not solicit, and the rate limit must hold within
//! `PATH_REQUEST_MIN_INTERVAL_MS` and re-arm after it.
//!
//! Sans-I/O: no BLE, no boards, sub-second wall clock.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, PATH_REQUEST_MIN_INTERVAL_MS, TRUNCATED_HASHBYTES};
use crate::destination::DestinationType;
use crate::embedded_storage::EmbeddedStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId, TickOutput};

/// The rebooted board's exact shape: `EmbeddedStorage`, transport enabled,
/// empty tables (`path_table_initial_len=0` is the boot banner's own words).
type EmbeddedNode = NodeCore<OsRng, MockClock, EmbeddedStorage>;

fn make_node() -> Box<EmbeddedNode> {
    NodeCoreBuilder::new().enable_transport(true).build_boxed(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        EmbeddedStorage::new(),
    )
}

/// One interface is the point: the pocket's BLE carries BOTH peers, so the
/// solicit must go back out on the interface the dropped packet arrived on.
fn add_ble(node: &mut EmbeddedNode) -> usize {
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new("ble_nrf", 1)));
    node.set_interface_name(idx, String::from("ble_nrf"));
    idx
}

/// The probe as the pocket sees it: a HEADER_2 `Data` packet for the far
/// destination, addressed to the pocket as designated hop. `seed` varies the
/// payload so a retry is a NEW packet, as on the wire — an identical repeat
/// would be dropped as a duplicate before ever reaching the forward path.
fn data_via(
    transport_id: [u8; TRUNCATED_HASHBYTES],
    dest: [u8; TRUNCATED_HASHBYTES],
    seed: u8,
) -> Vec<u8> {
    packet_via(transport_id, dest, PacketType::Data, seed)
}

/// The same shape one protocol step later: a relayed LinkRequest. Same
/// amnesia, same drop site, same required solicit.
fn link_request_via(
    transport_id: [u8; TRUNCATED_HASHBYTES],
    dest: [u8; TRUNCATED_HASHBYTES],
) -> Vec<u8> {
    packet_via(transport_id, dest, PacketType::LinkRequest, 0xA5)
}

fn packet_via(
    transport_id: [u8; TRUNCATED_HASHBYTES],
    dest: [u8; TRUNCATED_HASHBYTES],
    packet_type: PacketType,
    seed: u8,
) -> Vec<u8> {
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type2,
            context_flag: false,
            transport_type: TransportType::Transport,
            dest_type: DestinationType::Single,
            packet_type,
        },
        hops: 1,
        transport_id: Some(transport_id),
        destination_hash: dest,
        context: PacketContext::None,
        data: PacketData::Owned(std::vec![seed; 64]),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// Every path request for `dest` this output puts on the wire, in any action
/// form: a `Data` packet to the well-known path-request destination whose
/// payload names `dest` in its first 16 bytes.
fn path_requests_for(
    out: &TickOutput,
    pr_hash: &[u8; TRUNCATED_HASHBYTES],
    dest: &[u8; TRUNCATED_HASHBYTES],
) -> usize {
    out.actions
        .iter()
        .filter(|action| {
            let data = match action {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
            };
            Packet::unpack(data)
                .map(|p| {
                    p.flags.packet_type == PacketType::Data
                        && &p.destination_hash == pr_hash
                        && p.data.as_slice().len() >= TRUNCATED_HASHBYTES
                        && &p.data.as_slice()[..TRUNCATED_HASHBYTES] == dest
                })
                .unwrap_or(false)
        })
        .count()
}

/// THE question: the rebooted relay drops a Data forward for lack of a path.
/// It must still drop the packet (Python parity: no store-and-forward) but
/// may no longer stay silent about it — one path request for the destination
/// goes out, and the 48-byte transport form at that, so the upstream
/// requestor-is-next-hop guard can hold.
#[test]
fn relay_no_path_data_drop_solicits_the_path() {
    let mut node = make_node();
    let ble = add_ble(&mut node);
    assert_eq!(node.path_count(), 0, "a rebooted board holds no paths");

    let far_dest = [0x7Du8; TRUNCATED_HASHBYTES];
    let own_id = *node.identity().hash();
    let pr_hash = *node.transport().path_request_hash();

    let before = node.transport_stats();
    let out = node.handle_packet(InterfaceId(ble), &data_via(own_id, far_dest, 0x01));
    let after = node.transport_stats();

    assert_eq!(
        after.drops_no_path() - before.drops_no_path(),
        1,
        "the relayed packet itself is still dropped — the solicit heals the \
         NEXT attempt, it does not store-and-forward this one"
    );
    assert_eq!(
        path_requests_for(&out, &pr_hash, &far_dest),
        1,
        "a transport node that dropped a packet it was the designated hop \
         for must solicit the path it is missing; silence here is the \
         ble_reconnect 100%-loss mechanism"
    );

    // The 48-byte transport form carries our own id as requestor, so the
    // upstream node whose next hop IS this relay refuses to answer with the
    // route through us (handle_path_request requestor guard).
    let pr_payload: Vec<u8> = out
        .actions
        .iter()
        .find_map(|action| {
            let data = match action {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
            };
            Packet::unpack(data)
                .ok()
                .and_then(|p| (p.destination_hash == pr_hash).then(|| p.data.as_slice().to_vec()))
        })
        .unwrap();
    assert_eq!(
        pr_payload.len(),
        3 * TRUNCATED_HASHBYTES,
        "transport-node path request: dest + transport id + tag"
    );
    assert_eq!(
        &pr_payload[TRUNCATED_HASHBYTES..2 * TRUNCATED_HASHBYTES],
        &own_id,
        "the requestor field must be this relay's own transport id"
    );
}

/// Same mechanism one protocol step later: the relayed LinkRequest hitting
/// the empty table must solicit too, or the cell dies again at link setup
/// right after the probe finally passes.
#[test]
fn relay_no_path_link_request_drop_solicits_the_path() {
    let mut node = make_node();
    let ble = add_ble(&mut node);

    let far_dest = [0x7Eu8; TRUNCATED_HASHBYTES];
    let own_id = *node.identity().hash();
    let pr_hash = *node.transport().path_request_hash();

    let before = node.transport_stats();
    let out = node.handle_packet(InterfaceId(ble), &link_request_via(own_id, far_dest));
    let after = node.transport_stats();

    assert_eq!(
        after.drops_no_path() - before.drops_no_path(),
        1,
        "the LinkRequest is still dropped (no path, nothing to anchor)"
    );
    assert_eq!(
        path_requests_for(&out, &pr_hash, &far_dest),
        1,
        "and the missing path is solicited, same as the Data case"
    );
}

/// Control 1: overheard traffic — a packet whose designated hop is SOMEONE
/// ELSE — must not solicit. On a shared medium the overheard volume is the
/// high-frequency case; soliciting for it would turn every neighbour's
/// conversation into our path-request traffic.
#[test]
fn control_overheard_no_path_does_not_solicit() {
    let mut node = make_node();
    let ble = add_ble(&mut node);

    let far_dest = [0x7Fu8; TRUNCATED_HASHBYTES];
    let foreign = [0xC3u8; TRUNCATED_HASHBYTES];
    assert_ne!(&foreign, node.identity().hash(), "control must be foreign");
    let pr_hash = *node.transport().path_request_hash();

    let out = node.handle_packet(InterfaceId(ble), &data_via(foreign, far_dest, 0x01));
    assert_eq!(
        path_requests_for(&out, &pr_hash, &far_dest),
        0,
        "an overheard packet is not ours to route; no solicit"
    );
}

/// Control 2: the rate limit. Five probe retries ~27 s apart must not become
/// five broadcasts — `PATH_REQUEST_MIN_INTERVAL_MS` (20 s) holds within the
/// window and re-arms after it, so a destination that stays unanswered is
/// re-solicited on the retry cadence, not per packet.
#[test]
fn control_solicit_is_rate_limited_per_destination() {
    let mut node = make_node();
    let ble = add_ble(&mut node);

    let far_dest = [0x71u8; TRUNCATED_HASHBYTES];
    let own_id = *node.identity().hash();
    let pr_hash = *node.transport().path_request_hash();

    let out = node.handle_packet(InterfaceId(ble), &data_via(own_id, far_dest, 0x01));
    assert_eq!(
        path_requests_for(&out, &pr_hash, &far_dest),
        1,
        "first drop solicits"
    );

    // Immediate retry (duplicate-hash-safe: a probe retry is a NEW packet).
    let out = node.handle_packet(InterfaceId(ble), &data_via(own_id, far_dest, 0x02));
    assert_eq!(
        path_requests_for(&out, &pr_hash, &far_dest),
        0,
        "second drop inside the min interval must not solicit again"
    );

    // Past the min interval the solicit re-arms.
    let later = node.transport().clock().now_ms() + PATH_REQUEST_MIN_INTERVAL_MS + 1_000;
    node.transport().clock().set(later);
    let out = node.handle_packet(InterfaceId(ble), &data_via(own_id, far_dest, 0x03));
    assert_eq!(
        path_requests_for(&out, &pr_hash, &far_dest),
        1,
        "a still-unanswered destination is re-solicited after the interval"
    );
}
