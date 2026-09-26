//! mvr: the peer-loss cull must attribute a path learned through a
//! peer's link even when that peer's link identity is not the identity
//! that signed the announce (Codeberg #365).
//!
//! ## The desk failure this reproduces
//!
//! Desk test 2026-09-08 17:17:43 (Pocket V2 on 2e4840b): `--set-media
//! ble=off` dropped both BLE links, `BLE peer lost, 0 paths culled`
//! twice, and `[TRANSPORT] paths=1` stood — the phone's
//! `lxmf.delivery` entry survived and ate the 17:20:17 report.
//! `drop_paths_via_peer` attributes a direct entry by recalling the
//! identity from the cached announce and comparing it against the
//! peer id the interface reported (transport.rs, the
//! `recall_identity_hash(hash) == Some(*peer)` arm). Columba presents
//! one identity in the BLE handshake (the reported peer id,
//! `b99af2ec…` in the desk logs) and signs its `lxmf.delivery`
//! announce with another (`5302e370…`), so the comparison can never
//! match and the phone's delivery entry is permanently uncullable.
//! LNode and lnsd peers use one identity for both, which is why every
//! bench proof of the cull was green while the phone's entry survived.
//!
//! ## Reference semantics
//!
//! Python culls a path whose receiving interface no longer exists
//! (Transport.py:784-785); its unit of loss is the whole interface, so
//! "learned through the departed carrier" is exactly the attribution
//! it applies — identity never enters it. On a multi-peer BLE domain
//! the departing unit is one peer link, and the equivalent statement
//! is "learned through the departed peer's link". Only the interface
//! knows which link a packet arrived on (interface isolation), so it
//! reports the ingress peer per packet and the transport stamps it on
//! the entries it installs.
//!
//! ## Shape
//!
//! 1 node, 1 mock interface, deterministic, sub-second. An announce
//! for D (signed by identity I) arrives through peer P's link with
//! hash(I) != P. P is lost. The entry must go.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction};
use crate::embedded_storage::EmbeddedStorage;
use crate::identity::Identity;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::transport::{InterfaceId, TickOutput};

/// The Pocket's exact shape: `EmbeddedStorage`, transport enabled.
type EmbeddedNode = NodeCore<OsRng, MockClock, EmbeddedStorage>;

fn make_node() -> Box<EmbeddedNode> {
    NodeCoreBuilder::new().enable_transport(true).build_boxed(
        OsRng,
        MockClock::new(TEST_TIME_MS),
        EmbeddedStorage::new(),
    )
}

fn add_iface(node: &mut EmbeddedNode, name: &'static str, id: u8) -> usize {
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new(name, id)));
    node.set_interface_name(idx, String::from(name));
    idx
}

/// A destination whose announce identity is freshly generated — and
/// therefore never equal to the arbitrary link identity the tests
/// report as the ingress peer.
struct Peer {
    dest_hash: crate::DestinationHash,
    dest: Destination,
}

fn make_peer(app: &'static str) -> Peer {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &[app],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    Peer { dest_hash, dest }
}

impl Peer {
    fn direct_announce(&mut self, ts: u64) -> Vec<u8> {
        let ann = self.dest.announce(None, &mut OsRng, ts, ts / 1000).unwrap();
        let mut buf = [0u8; MTU];
        let len = ann.pack(&mut buf).unwrap();
        buf[..len].to_vec()
    }
}

fn path_lost_count(out: &TickOutput, dest: &crate::DestinationHash) -> usize {
    out.events
        .iter()
        .filter(
            |e| matches!(e, NodeEvent::PathLost { destination_hash } if destination_hash == dest),
        )
        .count()
}

/// THE desk scenario: the announce identity and the link identity
/// differ (Columba), the link dies, the entry must die with it.
#[test]
fn losing_the_ingress_peer_culls_the_entry_despite_identity_mismatch() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);

    // The link identity Columba presented in the handshake — NOT the
    // identity that signs the announce below.
    let link_peer = [0xB9u8; TRUNCATED_HASHBYTES];

    let mut phone = make_peer("columba");
    let _ = node.handle_packet_from_peer(
        InterfaceId(ble),
        link_peer,
        &phone.direct_announce(TEST_TIME_MS),
    );
    assert!(
        node.has_path(&phone.dest_hash),
        "announce through the peer's link must install a path"
    );

    let out = node.handle_interface_peer_lost(InterfaceId(ble), link_peer);

    assert!(
        !node.has_path(&phone.dest_hash),
        "the entry learned through the lost peer's link survived the \
         cull — the identity-mismatch blindness of desk test \
         2026-09-08 17:17:43 (`BLE peer lost, 0 paths culled`, paths=1)"
    );
    assert_eq!(
        path_lost_count(&out, &phone.dest_hash),
        1,
        "the cull must be announced as PathLost"
    );
}

/// Scope: losing one peer takes only that peer's entries — a second
/// destination learned through a DIFFERENT link on the same interface
/// survives, identity mismatch or not.
#[test]
fn cull_by_ingress_peer_is_scoped_to_that_peer() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);

    let lost_link = [0xB9u8; TRUNCATED_HASHBYTES];
    let other_link = [0xC7u8; TRUNCATED_HASHBYTES];

    let mut via_lost = make_peer("vialost");
    let mut via_other = make_peer("viaother");
    let _ = node.handle_packet_from_peer(
        InterfaceId(ble),
        lost_link,
        &via_lost.direct_announce(TEST_TIME_MS),
    );
    let _ = node.handle_packet_from_peer(
        InterfaceId(ble),
        other_link,
        &via_other.direct_announce(TEST_TIME_MS),
    );
    assert_eq!(node.path_count(), 2, "both paths installed");

    let _ = node.handle_interface_peer_lost(InterfaceId(ble), lost_link);

    assert!(
        !node.has_path(&via_lost.dest_hash),
        "the lost link's entry must be culled"
    );
    assert!(
        node.has_path(&via_other.dest_hash),
        "an entry learned through a different link must survive"
    );
}

/// A later announce for the same destination that arrives WITHOUT peer
/// context (another carrier) overwrites the stamp: the entry no longer
/// belongs to the BLE link and the peer loss must leave it alone.
#[test]
fn relearning_without_peer_context_clears_the_attribution() {
    let mut node = make_node();
    let ble = add_iface(&mut node, "ble_nrf", 0);
    let lora = add_iface(&mut node, "lora_sx1262", 1);

    let link_peer = [0xB9u8; TRUNCATED_HASHBYTES];
    let mut phone = make_peer("relearn");
    let _ = node.handle_packet_from_peer(
        InterfaceId(ble),
        link_peer,
        &phone.direct_announce(TEST_TIME_MS),
    );
    // Newer emission over LoRa displaces the BLE entry.
    let _ = node.handle_packet(
        InterfaceId(lora),
        &phone.direct_announce(TEST_TIME_MS + 2_000),
    );
    let entry = node
        .transport
        .get_path_clone(phone.dest_hash.as_bytes())
        .expect("path present");
    assert_eq!(entry.interface_index, lora, "the LoRa announce won");

    let out = node.handle_interface_peer_lost(InterfaceId(ble), link_peer);
    assert_eq!(
        path_lost_count(&out, &phone.dest_hash),
        0,
        "an entry relearned over another carrier no longer belongs to \
         the lost BLE link"
    );
    assert!(node.has_path(&phone.dest_hash), "the LoRa path survives");
}
