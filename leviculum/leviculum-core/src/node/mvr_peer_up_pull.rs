//! mvr: peer-up pull (Codeberg #365) — a node that gains a direct peer
//! asks it for its delivery path, IF configured to.
//!
//! When an interface reports "peer with identity I is up" and
//! `peer_up_pull_names` is configured, the core derives the peer's
//! delivery destination
//! `D = truncated_hash(sha256("lxmf.delivery")[..10] || I)` and, unless
//! it already holds a DIRECT entry for `D` on that interface, sends one
//! ordinary path request for `D` on that interface only.
//!
//! The default is EMPTY (field finding 2026-09-08, ledger 365): `I` is
//! the handshake identity, which against Columba is the phone's
//! TRANSPORT identity (the ble-reticulum reference checkout,
//! `BLEInterface._start_advertising_when_identity_ready`, advertises
//! `Transport.identity.hash`; Python-RNS creates that identity as a
//! standalone keypair at `transport_identity`, Transport.py:218-225)
//! — unrelated to the LXMF
//! identity `lxmf.delivery` is actually derived from, so every default
//! pull asked the phone for a destination it does not have and
//! `paths=0` held for 70 s. The bench proofs passed only because our
//! own stacks hand their single node identity to the handshake. The
//! peer-link re-origination (`mvr_peer_link_reorigination`) replaces
//! the pull's recovery job; the pull stays available for fleets whose
//! peers are known to be single-identity
//! (`NodeCoreBuilder::peer_up_pull_names`).
//!
//! Pinned here: the empty default is quiet on peer-up, and for a
//! configured list: the derivation against the two full identity→lxmf
//! pairs from the ledger (the field boards' banners), the
//! one-request-on-the-reporting-interface-only shape, the direct-entry
//! guard (relink churn must not spray requests), and that a RELAYED
//! entry does NOT suppress the pull — a stale relayed route is exactly
//! the trap state the pull exists to displace
//! (`mvr_relay_pr_from_next_hop` walks that full chain).

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, PATH_REQUEST_MIN_INTERVAL_MS, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction};
use crate::embedded_storage::EmbeddedStorage;
use crate::identity::Identity;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{HeaderType, Packet, PacketType, TransportType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId, TickOutput};

/// Both boards' exact shape: `EmbeddedStorage`, transport enabled.
type EmbeddedNode = NodeCore<OsRng, MockClock, EmbeddedStorage>;

/// A node with the pull explicitly configured — the opt-in shape for a
/// single-identity fleet (the default list is empty, pinned below).
fn make_node() -> Box<EmbeddedNode> {
    NodeCoreBuilder::new()
        .enable_transport(true)
        .peer_up_pull_names(std::vec![String::from("lxmf.delivery")])
        .build_boxed(OsRng, MockClock::new(TEST_TIME_MS), EmbeddedStorage::new())
}

fn add_iface(node: &mut EmbeddedNode, name: &'static str, id: u8) -> usize {
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new(name, id)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn hex16(s: &str) -> [u8; TRUNCATED_HASHBYTES] {
    let mut out = [0u8; TRUNCATED_HASHBYTES];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// Raw bytes of every action in this output, tagged with the send
/// target (`None` = broadcast).
fn wire_out(out: &TickOutput) -> Vec<(Option<InterfaceId>, Vec<u8>)> {
    out.actions
        .iter()
        .map(|action| match action {
            Action::SendPacket { iface, data, .. } => (Some(*iface), data.clone()),
            Action::Broadcast { data, .. } => (None, data.clone()),
        })
        .collect()
}

/// The path requests in this output as `(target iface, requested dest)`.
fn path_requests(
    node: &EmbeddedNode,
    out: &TickOutput,
) -> Vec<(Option<InterfaceId>, [u8; TRUNCATED_HASHBYTES])> {
    let pr_hash = *node.transport().path_request_hash();
    wire_out(out)
        .into_iter()
        .filter_map(|(target, data)| {
            let p = Packet::unpack(&data).ok()?;
            if p.flags.packet_type != PacketType::Data || p.destination_hash != pr_hash {
                return None;
            }
            let payload = p.data.as_slice();
            (payload.len() >= TRUNCATED_HASHBYTES).then(|| {
                let mut dest = [0u8; TRUNCATED_HASHBYTES];
                dest.copy_from_slice(&payload[..TRUNCATED_HASHBYTES]);
                (target, dest)
            })
        })
        .collect()
}

/// The derivation, pinned against the two full identity→lxmf pairs from
/// the field boards' `[IDENTITY]` banners (ledger 365, 2026-09-04
/// board proof), end to end: the request the pull puts on the wire must
/// name exactly the destination the real phone/board holds. One request
/// per peer-up, on the reporting interface and nowhere else.
#[test]
fn peer_up_pulls_the_ledger_pair_destinations_on_the_reporting_interface_only() {
    let pairs = [
        // T114 identity → its lxmf.delivery destination
        (
            hex16("b2a8bea123f668be63e85be2374e26e5"),
            hex16("be26233976540d7d9e10faf5c396558a"),
        ),
        // Pocket identity → its lxmf.delivery destination
        (
            hex16("1d48253ff2dddd5f95e6ef6ce8302a62"),
            hex16("2f9a770aa734a6ab02c7e845583cf206"),
        ),
    ];

    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let _lora = add_iface(&mut node, "lora_sx1262", 1);

    for (i, (identity, lxmf)) in pairs.iter().enumerate() {
        // Past the path-request rate limiter, so each pull stands alone.
        let now = node.transport().clock().now_ms() + PATH_REQUEST_MIN_INTERVAL_MS + 1_000;
        node.transport().clock().set(now);

        let out = node.handle_interface_peer_up(InterfaceId(ble), *identity);
        let requests = path_requests(&node, &out);
        assert_eq!(
            requests.len(),
            1,
            "pair {i}: exactly one path request per peer-up event"
        );
        let (target, dest) = requests[0];
        assert_eq!(
            target,
            Some(InterfaceId(ble)),
            "pair {i}: the pull goes to the reporting interface, not broadcast"
        );
        assert_eq!(
            dest, *lxmf,
            "pair {i}: derived destination must match the board's banner"
        );
        assert_eq!(
            wire_out(&out).len(),
            1,
            "pair {i}: nothing else leaves on any interface"
        );
    }
}

/// The guard: once a direct announce for `D` has arrived on the
/// interface, a repeated peer-up (lnsd sees the phone's random-address
/// rotation relink every ~60 s) must not produce another request. The
/// clock is moved past the path-request rate limiter first, so what
/// silences the second pull is the direct entry, not the limiter.
#[test]
fn peer_up_after_a_direct_announce_is_quiet() {
    let phone_identity = Identity::generate(&mut OsRng);
    let phone_identity_hash = *phone_identity.hash();
    let mut phone_delivery = Destination::new(
        Some(phone_identity),
        Direction::In,
        DestinationType::Single,
        "lxmf",
        &["delivery"],
    )
    .unwrap();

    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let _lora = add_iface(&mut node, "lora_sx1262", 1);

    let out = node.handle_interface_peer_up(InterfaceId(ble), phone_identity_hash);
    let first = path_requests(&node, &out);
    assert_eq!(first.len(), 1, "no path yet: the first peer-up pulls");
    assert_eq!(
        first[0].1,
        *phone_delivery.hash().as_bytes(),
        "the pull names the peer's delivery destination"
    );

    // The phone's answer: an ordinary direct announce on the same link.
    let ts = node.transport().clock().now_ms();
    let ann = phone_delivery
        .announce(None, &mut OsRng, ts, ts / 1000)
        .unwrap();
    let mut buf = [0u8; MTU];
    let len = ann.pack(&mut buf).unwrap();
    let _ = node.handle_packet(InterfaceId(ble), &buf[..len]);
    assert!(
        node.has_path(phone_delivery.hash()),
        "the announce installs the direct entry"
    );

    let now = node.transport().clock().now_ms() + PATH_REQUEST_MIN_INTERVAL_MS + 1_000;
    node.transport().clock().set(now);
    let out = node.handle_interface_peer_up(InterfaceId(ble), phone_identity_hash);
    assert!(
        path_requests(&node, &out).is_empty(),
        "a peer-up with the direct entry standing must not pull again"
    );
}

/// A RELAYED entry for `D` must NOT suppress the pull: holding the
/// peer's destination via a third node while the peer itself is a
/// direct neighbour is exactly the #365 trap state, and displacing that
/// stale route is what the pull is for (fewer hops win when the direct
/// announce comes back).
#[test]
fn a_relayed_entry_does_not_suppress_the_pull() {
    let phone_identity = Identity::generate(&mut OsRng);
    let phone_identity_hash = *phone_identity.hash();
    let mut phone_delivery = Destination::new(
        Some(phone_identity),
        Direction::In,
        DestinationType::Single,
        "lxmf",
        &["delivery"],
    )
    .unwrap();

    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let lora = add_iface(&mut node, "lora_sx1262", 1);

    // D held via a third node over LoRa: the phone's announce one relay
    // later (HEADER_2, wire hops 1, the relay as transport_id).
    let ts = node.transport().clock().now_ms();
    let ann = phone_delivery
        .announce(None, &mut OsRng, ts, ts / 1000)
        .unwrap();
    let mut buf = [0u8; MTU];
    let len = ann.pack(&mut buf).unwrap();
    let mut relayed = Packet::unpack(&buf[..len]).unwrap();
    relayed.flags.header_type = HeaderType::Type2;
    relayed.flags.transport_type = TransportType::Transport;
    relayed.hops = 1;
    relayed.transport_id = Some([0x33; TRUNCATED_HASHBYTES]);
    let mut buf = [0u8; MTU];
    let len = relayed.pack(&mut buf).unwrap();
    let _ = node.handle_packet(InterfaceId(lora), &buf[..len]);
    let entry = node
        .transport
        .get_path_clone(phone_delivery.hash().as_bytes())
        .expect("relayed entry installed");
    assert!(!entry.is_direct(), "premise: the held route is relayed");

    let out = node.handle_interface_peer_up(InterfaceId(ble), phone_identity_hash);
    let requests = path_requests(&node, &out);
    assert_eq!(
        requests.len(),
        1,
        "a relayed entry is the trap state, the pull must still fire"
    );
    assert_eq!(requests[0].0, Some(InterfaceId(ble)));
}

/// The default list is empty, so a peer-up on an unconfigured node
/// emits NOTHING: the handshake identity is the peer's transport
/// identity, and `D("lxmf.delivery", transport identity)` names a
/// destination a reference peer does not hold — the 2026-09-08 field
/// finding this default closes (every pull to the Columba phone went
/// unanswered while its real destination resolved in 162 ms once the
/// correctly named request was forwarded). The report itself is still
/// processed; the recovery lives in `mvr_peer_link_reorigination`.
#[test]
fn the_default_pull_list_is_empty_and_peer_up_stays_quiet() {
    let phone_identity_hash = *Identity::generate(&mut OsRng).hash();

    let mut node = NodeCoreBuilder::new().enable_transport(true).build_boxed(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        EmbeddedStorage::new(),
    );
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let _lora = add_iface(&mut node, "lora_sx1262", 1);

    let out = node.handle_interface_peer_up(InterfaceId(ble), phone_identity_hash);
    assert!(
        wire_out(&out).is_empty(),
        "no configured pull names: a peer-up report must put nothing on \
         the wire — a default pull would ask for a destination derived \
         from the wrong identity"
    );
}
