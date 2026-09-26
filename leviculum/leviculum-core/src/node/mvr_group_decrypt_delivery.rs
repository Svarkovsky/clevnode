//! mvr (L-0014): a packet addressed to a registered GROUP destination must be
//! decrypted with the shared group token before delivery — not handed to the
//! application as raw ciphertext.
//!
//! Background: the node's inbound dispatch decrypted only for Single
//! destinations and passed everything else through as "Plain", so a GROUP
//! destination received the RNS Token wire bytes (IV || AES-CBC || HMAC) as
//! if they were payload. The capability existed one call away —
//! `Destination::decrypt` already dispatches GROUP to the shared token — the
//! inbound path just never used it.
//!
//! Python parity (reference/Reticulum 1.3.5): `Destination.receive` routes
//! every non-LINKREQUEST packet through `self.decrypt` (Destination.py:
//! 403-410), which for GROUP decrypts with the shared Token
//! (Destination.py:645-651); a failed decrypt returns None and `receive`
//! drops the packet without delivering (Destination.py:410). There is no
//! case in which a GROUP destination receives unencrypted data — the PLAIN
//! passthrough exists only for PLAIN destinations (Destination.py:618-619).
//!
//! Three cases:
//!
//! 1. a group-token-encrypted packet delivers the PLAINTEXT to the
//!    application;
//! 2. a payload the group key cannot decrypt is dropped, counted under
//!    `group-decrypt-fail`, and delivers nothing (Python: silent drop);
//! 3. the send path is the same mechanism: `send_single_packet` to a
//!    registered GROUP destination must encrypt with the group token, put
//!    GROUP in the wire flags, broadcast (group destinations have no paths),
//!    and round-trip to a receiving peer holding the same key.

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
use crate::transport::{Action, InterfaceId};
use crate::DestinationHash;

type TestNode = NodeCore<OsRng, MockClock, MemoryStorage>;

const PLAINTEXT: &[u8] = b"group plaintext probe";

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

/// A GROUP destination scoped by `identity` (all members share the identity
/// so their destination hashes match, like Python's auto-scope aspect).
fn group_dest(identity: &Identity) -> Destination {
    let id_view =
        Identity::from_public_key_bytes(&identity.public_key_bytes()).expect("public view");
    Destination::new(
        Some(id_view),
        Direction::In,
        DestinationType::Group,
        "mvrapp",
        &["groupdec"],
    )
    .expect("group destination")
}

/// Register a keyed GROUP destination on `node`; returns its hash, the
/// shared key, and the scoping identity.
fn register_group_dest(node: &mut TestNode) -> (DestinationHash, [u8; 64], Identity) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = group_dest(&identity);
    dest.create_group_key(&mut OsRng).expect("create group key");
    let key = *dest.group_key().expect("key present");
    let hash = *dest.hash();
    node.register_destination(dest);
    (hash, key, identity)
}

/// A sender's view of the same group: same scoping identity, same shared key.
fn sender_view(identity: &Identity, key: &[u8; 64]) -> Destination {
    let mut dest = group_dest(identity);
    dest.load_group_key(key).expect("load group key");
    dest
}

/// Wire bytes for a Type1 broadcast GROUP Data packet to `hash`.
fn group_data_packet(hash: &DestinationHash, payload: Vec<u8>) -> Vec<u8> {
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Group,
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

/// Case 1: a group-token-encrypted packet delivers the plaintext, not the
/// token ciphertext, and the decrypt-fail counter stays untouched.
#[test]
fn group_packet_delivers_plaintext_not_ciphertext() {
    let (mut node, iface) = make_node();
    let (hash, key, identity) = register_group_dest(&mut node);

    let sender = sender_view(&identity, &key);
    let ciphertext = sender
        .encrypt(PLAINTEXT, None, &mut OsRng)
        .expect("group encrypt");
    assert!(
        !ciphertext.windows(PLAINTEXT.len()).any(|w| w == PLAINTEXT),
        "scaffold: the token ciphertext must not contain the plaintext"
    );

    let raw = group_data_packet(&hash, ciphertext.clone());
    let out = node.handle_packet(InterfaceId(iface), &raw);

    let delivered = delivered_payloads(&out.events);
    assert_eq!(
        delivered,
        std::vec![PLAINTEXT.to_vec()],
        "a GROUP packet must deliver the decrypted plaintext \
         {PLAINTEXT:02x?}; the application got {delivered:02x?} \
         (token ciphertext was {ciphertext:02x?})"
    );
    assert_eq!(
        node.transport.stats().drops_group_decrypt_fail(),
        0,
        "the counter must not fire on the good path"
    );
}

/// Case 2: a payload the group key cannot decrypt is dropped and counted,
/// and delivers nothing — Python's `Destination.receive` returns False on a
/// None decrypt (Destination.py:410) and never invokes the callback.
#[test]
fn undecryptable_group_packet_is_counted_not_delivered() {
    let (mut node, iface) = make_node();
    let (hash, _key, identity) = register_group_dest(&mut node);

    // Same group scope, DIFFERENT key: the token HMAC cannot verify.
    let mut wrong = group_dest(&identity);
    wrong.create_group_key(&mut OsRng).expect("wrong key");
    let ciphertext = wrong
        .encrypt(PLAINTEXT, None, &mut OsRng)
        .expect("group encrypt");

    let raw = group_data_packet(&hash, ciphertext);
    let out = node.handle_packet(InterfaceId(iface), &raw);

    let delivered = delivered_payloads(&out.events);
    assert!(
        delivered.is_empty(),
        "an undecryptable GROUP packet must not deliver; got {delivered:02x?}"
    );
    assert_eq!(
        node.transport.stats().drops_group_decrypt_fail(),
        1,
        "the decrypt miss must be counted, not dropped silently"
    );
}

/// Case 3 (send path, same mechanism): sending to a registered GROUP
/// destination must encrypt with the group token, mark the wire flags GROUP,
/// broadcast, and deliver the plaintext to a peer holding the same key.
#[test]
fn group_send_encrypts_broadcasts_and_round_trips() {
    let (mut sender, _iface_s) = make_node();
    let (hash, key, identity) = register_group_dest(&mut sender);

    let (_hash_tx, out) = sender
        .send_single_packet(&hash, PLAINTEXT)
        .expect("send to a registered keyed GROUP destination must succeed");

    // Pathless group send must broadcast (Python Transport.outbound
    // broadcasts when the destination has no path), never SendPacket.
    let raw = out
        .actions
        .iter()
        .find_map(|a| match a {
            Action::Broadcast { data, .. } => Some(data.clone()),
            Action::SendPacket { .. } => None,
        })
        .expect("group send must produce a Broadcast action");

    let packet = Packet::unpack(&raw).expect("unpack");
    assert_eq!(
        packet.flags.dest_type,
        DestinationType::Group,
        "the wire flags must carry GROUP (Python get_packed_flags packs \
         destination.type, Packet.py:174)"
    );
    let payload = packet.data.as_slice();
    assert!(
        !payload.windows(PLAINTEXT.len()).any(|w| w == PLAINTEXT),
        "the broadcast payload must be group-token ciphertext, not \
         plaintext; got {payload:02x?}"
    );

    // Round-trip: a second node in the same group decrypts it.
    let (mut receiver, iface_r) = make_node();
    receiver.register_destination(sender_view(&identity, &key));
    let out = receiver.handle_packet(InterfaceId(iface_r), &raw);
    assert_eq!(
        delivered_payloads(&out.events),
        std::vec![PLAINTEXT.to_vec()],
        "the peer holding the shared key must receive the plaintext"
    );
}
