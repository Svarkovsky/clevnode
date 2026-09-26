//! mvr: a transport path RESPONSE to a network PEER must emit the STORED path
//! count, not the pre-increment wire hop byte (#38, D3).
//!
//! ## The defect
//!
//! When a transport node answers a path request from a network peer
//! (`handle_path_request` case 2b, `transport.rs`), it builds the path-response
//! announce entry from `cached_packet.hops`. `cached_packet` is
//! `Packet::unpack(&cached_raw)`, and `cached_raw` is the AS-RECEIVED wire bytes
//! (`set_announce_cache`). Its hop byte is the PRE-increment value, `stored - 1`
//! (the in-memory `packet.hops += 1` at receipt never touches the raw buffer).
//! The rebroadcast then puts that value on the wire verbatim. Python emits the
//! receipt-incremented STORED count instead (`Transport.py:2956`
//! `packet.hops = path_table[dest][IDX_PT_HOPS]`), so every path learned via our
//! transport path response is one hop too small, and it COMPOUNDS on each
//! re-learn through a leviculum transport.
//!
//! ## The honest topology
//!
//! ```text
//!   D --announce(wire 0)--> M1 --wire 1--> M2 --wire 2--> R          P --req--> R
//!                                                          (records hops_to(D)=3)
//! ```
//!
//! R learns D over a two-relay arm, recording `hops_to(D) = S = 3`; its cached
//! raw carries the pre-increment wire byte 2. A network peer P then requests the
//! path for D from R. R answers via case 2b.
//!
//! Python semantics: the responder emits its STORED count S; the requester adds
//! its own receipt hop. So `P.hops_to(D)` must equal `S + 1 = 4`.
//!
//! On master R emits `S - 1 = 2`, P adds one, and `P.hops_to(D) = S = 3` — one
//! hop too small. That is the RED assertion below.
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

/// All bytes an output wants to put on the wire this step.
fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

/// Single outbound packet; panics if not exactly one.
fn one_packet(output: &TickOutput) -> Vec<u8> {
    let data = action_data(output);
    assert_eq!(
        data.len(),
        1,
        "expected exactly one outbound packet, got {}",
        data.len()
    );
    data.into_iter().next().unwrap()
}

/// Feed an announce into a relay, advance its clock past the rebroadcast delay,
/// and collect the forwarded announce bytes.
fn forward_announce(relay: &mut TransportNode, in_iface: usize, raw: &[u8]) -> Vec<Vec<u8>> {
    let _ = relay.handle_packet(InterfaceId(in_iface), raw);
    let now = relay.transport().clock().now_ms();
    relay.transport().clock().set(now + 100_000);
    let out = relay.handle_timeout();
    action_data(&out)
}

/// Build a destination D and one direct (wire 0) announce packet for it.
fn make_destination() -> (crate::DestinationHash, Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["pathresp"],
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

struct Outcome {
    /// R's stored path length to D (the value it should emit).
    r_hops_to_d: Option<u8>,
    /// The wire hop byte R put on its path-response announce.
    response_wire_hops: u8,
    /// What the peer learned as its path length to D.
    p_hops_to_d: Option<u8>,
}

/// R learns D at S=3 hops over a two-relay arm, then answers a network peer's
/// path request for D. Returns R's stored count, the wire hop byte R emitted,
/// and what the peer learned.
fn run_scenario() -> Outcome {
    let (dest_d, announce_raw) = make_destination();

    let mut relay_m1 = make_transport_node();
    let mut relay_m2 = make_transport_node();
    let mut relay_r = make_transport_node();
    let mut peer_p = make_transport_node();

    // Interfaces (index order fixed by call order).
    let m1_in = add_iface(&mut relay_m1, "M1_in", false);
    let _m1_out = add_iface(&mut relay_m1, "M1_out", false);
    let m2_in = add_iface(&mut relay_m2, "M2_in", false);
    let _m2_out = add_iface(&mut relay_m2, "M2_out", false);
    let r_from_m2 = add_iface(&mut relay_r, "R_from_M2", false);
    let r_to_p = add_iface(&mut relay_r, "R_to_P", false);
    let p_to_r = add_iface(&mut peer_p, "P_to_R", false);

    // --- Path learning: D -> M1 -> M2 -> R -------------------------------
    // M1 forwards the direct announce (wire 0 -> wire 1).
    let m1_fwds = forward_announce(&mut relay_m1, m1_in, &announce_raw);
    assert_eq!(m1_fwds.len(), 1, "M1 forwards exactly one announce");
    // M2 forwards M1's announce (wire 1 -> wire 2).
    let m2_fwds = forward_announce(&mut relay_m2, m2_in, &m1_fwds[0]);
    assert_eq!(m2_fwds.len(), 1, "M2 forwards exactly one announce");
    // R receives M2's announce and records D at 3 hops via M2.
    let _ = relay_r.handle_packet(InterfaceId(r_from_m2), &m2_fwds[0]);
    let r_hops_to_d = relay_r.hops_to(&dest_d);

    // --- The network peer requests the path for D from R -----------------
    let path_req_hash = *relay_r.transport().path_request_hash();
    let requester_id = [0xBBu8; TRUNCATED_HASHBYTES];
    let tag = [0xAAu8; TRUNCATED_HASHBYTES];
    let request = build_path_request(&path_req_hash, &dest_d, &requester_id, &tag);

    // R answers via case 2b: schedules a deferred targeted path response.
    let _ = relay_r.handle_packet(InterfaceId(r_to_p), &request);
    // Fire the deferred rebroadcast toward the requesting interface.
    let rnow = relay_r.transport().clock().now_ms();
    relay_r.transport().clock().set(rnow + 100_000);
    let out = relay_r.handle_timeout();
    let response = one_packet(&out);
    let response_wire_hops = Packet::unpack(&response).unwrap().hops;

    // --- The peer processes the response and learns its path to D --------
    let _ = peer_p.handle_packet(InterfaceId(p_to_r), &response);
    let p_hops_to_d = peer_p.hops_to(&dest_d);

    Outcome {
        r_hops_to_d,
        response_wire_hops,
        p_hops_to_d,
    }
}

/// THE bug: a transport path response emits the pre-increment wire byte
/// (`S - 1`) instead of the STORED count (`S`), so a network peer learns `S`
/// where Python semantics require `S + 1` (responder emits S, requester adds its
/// receipt hop). On master this asserts RED with `P.hops_to(D) == S`.
#[test]
fn path_response_to_network_peer_emits_stored_count() {
    let o = run_scenario();

    // Precondition: R genuinely learned the long arm at S = 3 hops.
    assert_eq!(
        o.r_hops_to_d,
        Some(3),
        "R must record D at 3 hops (the two-relay arm)"
    );
    let s = 3u8;

    // The response must carry R's STORED count S on the wire, matching
    // Transport.py:2956 (path_table hops), NOT the pre-increment cached byte S-1.
    assert_eq!(
        o.response_wire_hops,
        s,
        "R's path response must put the STORED count ({s}) on the wire, not the \
         pre-increment cached byte ({}). Master emits S-1.",
        s - 1
    );

    // End to end: the peer adds its own receipt hop, so it must learn S + 1.
    assert_eq!(
        o.p_hops_to_d,
        Some(s + 1),
        "the peer must learn S+1 = {} (responder emits S, requester adds one). \
         Master yields S = {s} — one hop too small.",
        s + 1
    );
}

/// Guard: case 2a (the local-client answer, `transport.rs` `+1` arm) is
/// unaffected. A local client requesting the path gets the STORED count S on the
/// wire — the cached pre-increment byte (S-1) plus the explicit `+1`. This holds
/// on master AND after the case-2b fix; it documents that only the
/// transport->network-peer arm was wrong.
#[test]
fn path_response_to_local_client_emits_stored_count_unaffected() {
    let (dest_d, announce_raw) = make_destination();

    let mut relay_m1 = make_transport_node();
    let mut relay_m2 = make_transport_node();
    let mut relay_r = make_transport_node();

    let m1_in = add_iface(&mut relay_m1, "M1_in", false);
    let _m1_out = add_iface(&mut relay_m1, "M1_out", false);
    let m2_in = add_iface(&mut relay_m2, "M2_in", false);
    let _m2_out = add_iface(&mut relay_m2, "M2_out", false);
    let r_from_m2 = add_iface(&mut relay_r, "R_from_M2", false);
    // The requesting interface IS a local client (case 2a).
    let r_local = add_iface(&mut relay_r, "R_local_client", true);

    // R learns D at S = 3 over the two-relay arm.
    let m1_fwds = forward_announce(&mut relay_m1, m1_in, &announce_raw);
    let m2_fwds = forward_announce(&mut relay_m2, m2_in, &m1_fwds[0]);
    let _ = relay_r.handle_packet(InterfaceId(r_from_m2), &m2_fwds[0]);
    assert_eq!(relay_r.hops_to(&dest_d), Some(3), "R records D at 3 hops");
    let s = 3u8;

    // A local client requests the path. Case 2a answers immediately.
    let path_req_hash = *relay_r.transport().path_request_hash();
    let requester_id = [0xCCu8; TRUNCATED_HASHBYTES];
    let tag = [0xDDu8; TRUNCATED_HASHBYTES];
    let request = build_path_request(&path_req_hash, &dest_d, &requester_id, &tag);

    let out = relay_r.handle_packet(InterfaceId(r_local), &request);
    let response = one_packet(&out);
    let response_wire_hops = Packet::unpack(&response).unwrap().hops;

    assert_eq!(
        response_wire_hops,
        s,
        "case 2a must emit the STORED count ({s}) to the local client \
         (cached byte {} + 1); this arm is correct and unchanged.",
        s - 1
    );
}
