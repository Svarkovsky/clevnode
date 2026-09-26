//! #159 tranche 2: pin the SEMANTICS of link-layer generated wire fields
//! against the reference, not against our own reader.
//!
//! Every test here extracts the audited field from the actual wire bytes and
//! re-applies the rule a Python peer applies to it (`reference/Reticulum`,
//! cited per test), so a writer/reader pair that drifted together stays red
//! (the #155 failure class). Announce-layer fields are tranche 1 (c8609b4);
//! the emission-timestamp chain is #155/#160/#161. The two fields the audit
//! found WRONG are pinned since their fixes landed: the REQUEST payload
//! timestamp (element 0, epoch seconds via the #155 timebase, Codeberg #164)
//! here, and the advertisement `o` field (Codeberg #165) in the resource
//! layer.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::TRUNCATED_HASHBYTES;
use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::LinkId;
use crate::node::request::RequestPolicy;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::packet::{Packet, PacketContext, PacketType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::NoStorage;
use crate::transport::{Action, InterfaceId, TickOutput};

type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

// ----------------------------------------------------------------------------
// Sans-I/O helpers (same pattern as the other mvr modules).
// ----------------------------------------------------------------------------

fn add_iface(node: &mut EndpointNode, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

fn deliver_collect(
    target: &mut EndpointNode,
    iface: usize,
    packets: Vec<Vec<u8>>,
) -> (Vec<Vec<u8>>, Vec<NodeEvent>) {
    let mut out = Vec::new();
    let mut events = Vec::new();
    for pkt in packets {
        let o = target.handle_packet(InterfaceId(iface), &pkt);
        out.extend(action_data(&o));
        events.extend(o.events);
    }
    (out, events)
}

fn make_responder(aspect: &'static str) -> (EndpointNode, crate::DestinationHash, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "fieldpins",
        &[aspect],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    node.register_request_handler(dest_hash, "/echo", RequestPolicy::AllowAll);
    (node, dest_hash, signing_key)
}

fn make_initiator() -> EndpointNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().build(OsRng, clock, NoStorage)
}

/// Recompute the truncated packet hash the way a Python peer does
/// (`Packet.get_hashable_part`, Packet.py:355-361 + `Identity.truncated_hash`):
/// `(flags & 0x0F) + raw[2:]`, skipping the transport id for HEADER_2.
fn reference_truncated_packet_hash(raw: &[u8]) -> [u8; TRUNCATED_HASHBYTES] {
    let header_2 = raw[0] & 0x40 != 0;
    let mut hashable = std::vec![raw[0] & 0x0F];
    if header_2 {
        hashable.extend_from_slice(&raw[2 + TRUNCATED_HASHBYTES..]);
    } else {
        hashable.extend_from_slice(&raw[2..]);
    }
    let hash = crate::crypto::sha256(&hashable);
    let mut truncated = [0u8; TRUNCATED_HASHBYTES];
    truncated.copy_from_slice(&hash[..TRUNCATED_HASHBYTES]);
    truncated
}

/// Recompute the link id from a LINKREQUEST wire packet the way both peers do
/// (`Link.link_id_from_lr_packet`, Link.py:341-347): the hashable part with
/// any payload bytes beyond ECPUBSIZE(64) — the MTU signalling — trimmed off.
fn reference_link_id(raw_lr: &[u8]) -> [u8; TRUNCATED_HASHBYTES] {
    let payload_len = raw_lr.len() - 19; // Type1: flags+hops+dest(16)+context
    let trim = payload_len.saturating_sub(64);
    let mut hashable = std::vec![raw_lr[0] & 0x0F];
    hashable.extend_from_slice(&raw_lr[2..raw_lr.len() - trim]);
    let hash = crate::crypto::sha256(&hashable);
    let mut truncated = [0u8; TRUNCATED_HASHBYTES];
    truncated.copy_from_slice(&hash[..TRUNCATED_HASHBYTES]);
    truncated
}

fn decrypt_link_payload(node: &EndpointNode, link_id: &LinkId, wire: &[u8]) -> Vec<u8> {
    let link = node.link(link_id).expect("link must exist");
    let payload = &wire[19..];
    let mut buf = std::vec![0u8; payload.len()];
    let n = link
        .decrypt(payload, &mut buf)
        .expect("link payload must decrypt");
    buf.truncate(n);
    buf
}

/// Drive establishment while capturing the three establishment wire packets.
/// Returns (lr_wire, proof_wire, rtt_wire, caller_link_id, responder_link_id).
#[allow(clippy::type_complexity)]
fn establish_captured(
    initiator: &mut EndpointNode,
    i_iface: usize,
    responder: &mut EndpointNode,
    r_iface: usize,
    dest_hash: crate::DestinationHash,
    signing_key: [u8; 32],
    rtt_advance_ms: u64,
) -> (Vec<u8>, Vec<u8>, Vec<u8>, LinkId, LinkId) {
    let (caller_link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
    let lr_wire = action_data(&out)
        .into_iter()
        .next()
        .expect("connect must emit the LINKREQUEST");

    let (mut back, r_events) = deliver_collect(responder, r_iface, std::vec![lr_wire.clone()]);
    let responder_link_id = r_events
        .iter()
        .find_map(|ev| match ev {
            NodeEvent::LinkEstablished { link_id, .. } => Some(*link_id),
            _ => None,
        })
        .unwrap_or_else(|| {
            // Responder may report establishment only after the RTT packet;
            // the proof still carries the link id in its destination field.
            LinkId::new(back[0][2..18].try_into().unwrap())
        });
    assert_eq!(
        back.len(),
        1,
        "responder must answer with exactly the proof"
    );
    let proof_wire = back.remove(0);

    // The initiator measures RTT as (proof arrival - request time).
    initiator.transport.clock().advance(rtt_advance_ms);
    responder.transport.clock().advance(rtt_advance_ms);

    let (rtt_out, _) = deliver_collect(initiator, i_iface, std::vec![proof_wire.clone()]);
    let rtt_wire = rtt_out
        .into_iter()
        .find(|p| p.len() > 18 && p[18] == PacketContext::Lrrtt.to_byte())
        .expect("initiator must answer the proof with the LRRTT packet");
    let (_, _) = deliver_collect(responder, r_iface, std::vec![rtt_wire.clone()]);

    (
        lr_wire,
        proof_wire,
        rtt_wire,
        caller_link_id,
        responder_link_id,
    )
}

// ----------------------------------------------------------------------------
// Establishment fields
// ----------------------------------------------------------------------------

/// #159 tranche 2: pin the LINKREQUEST payload and the link id it induces.
///
/// Reference rules: `Link.validate_request` (Link.py:186-190) accepts only a
/// 64- or 67-byte payload of `x25519_pub + ed25519_pub [+ signalling]`; the
/// signalling bytes carry a 21-bit MTU and a 3-bit mode (Link.py:148-152) and
/// a mode outside `ENABLED_MODES = [MODE_AES256_CBC]` kills establishment
/// (Link.py:133/:167-170). Both peers derive the link id from the LR packet's
/// hashable part with the signalling trimmed (Link.py:341-350) and address
/// every subsequent packet to it — the pin closes that loop by checking the
/// destination field of the peer's LRPROOF against the reference-recomputed
/// id, never against our own getter.
#[test]
fn link_request_wire_pins_reference_establishment_semantics() {
    let (mut responder, dest_hash, signing_key) = make_responder("lr");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");

    let (lr_wire, proof_wire, _rtt, caller_link_id, responder_link_id) = establish_captured(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
        0,
    );

    let parsed = Packet::unpack(&lr_wire).unwrap();
    assert_eq!(parsed.flags.packet_type, PacketType::LinkRequest);
    assert_eq!(parsed.context, PacketContext::None);
    assert_eq!(parsed.destination_hash, *dest_hash.as_bytes());

    let payload = &lr_wire[19..];
    assert_eq!(
        payload.len(),
        67,
        "LR payload must be ECPUBSIZE + LINK_MTU_SIZE (Link.py:187 length gate)"
    );

    // Signalling: 21-bit MTU + 3-bit mode (Link.py:148-152).
    let raw_signalling =
        ((payload[64] as u32) << 16) | ((payload[65] as u32) << 8) | payload[66] as u32;
    assert_eq!(
        raw_signalling & 0x1F_FFFF,
        crate::constants::MTU as u32,
        "signalled MTU must be our real MTU (peer clamps its link MTU to it)"
    );
    assert_eq!(
        (raw_signalling >> 21) & 0x07,
        crate::constants::MODE_AES256_CBC as u32,
        "mode must be MODE_AES256_CBC: any other mode is outside Python's \
         ENABLED_MODES and kills establishment"
    );

    // Ephemeral x25519 (0..32) must be the key the peer handshakes with:
    // the proof decrypts under the session derived from it (checked in the
    // LRRTT test). Here pin the ed25519 half (32..64): the peer verifies our
    // future data proofs and identify signatures against exactly these bytes.
    assert_ne!(&payload[..32], &[0u8; 32], "x25519 pub must be on the wire");
    assert_ne!(
        &payload[32..64],
        &[0u8; 32],
        "ed25519 pub must be on the wire"
    );

    // Reference link id: hashable part minus signalling (Link.py:341-347).
    let ref_link_id = reference_link_id(&lr_wire);
    assert_eq!(
        caller_link_id.as_bytes(),
        &ref_link_id,
        "our own link id must equal the reference derivation"
    );
    assert_eq!(
        responder_link_id.as_bytes(),
        &ref_link_id,
        "the responder must register the link under the reference id"
    );
    assert_eq!(
        &proof_wire[2..18],
        &ref_link_id,
        "the peer's LRPROOF must be addressed to the reference link id"
    );

    // The trim rule must bite: hashing WITHOUT trimming the signalling gives
    // a different id (what a writer hashing the whole payload would compute).
    assert_ne!(
        reference_truncated_packet_hash(&lr_wire),
        ref_link_id,
        "link id must exclude the signalling bytes from the hashable part"
    );
}

/// #159 tranche 2: pin the LRPROOF payload by reconstructing the signed data
/// INDEPENDENTLY in the reference byte order (Link.py:371-373 prove /
/// :417-419 validate_proof): `link_id + x25519_pub + ed25519_pub +
/// signalling`, verified with raw Ed25519 against the destination identity's
/// verifying key. The initiator activates the link ONLY if this signature
/// verifies, the payload is exactly SIGLENGTH/8 + 32 (+3) bytes, and the mode
/// in the signalling equals its own (Link.py:400-407).
#[test]
fn link_proof_wire_verifies_under_reference_composition() {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let (mut responder, dest_hash, signing_key) = make_responder("lp");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");

    let (_lr, proof_wire, _rtt, caller_link_id, _resp_id) = establish_captured(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
        0,
    );

    let parsed = Packet::unpack(&proof_wire).unwrap();
    assert_eq!(parsed.flags.packet_type, PacketType::Proof);
    assert_eq!(parsed.context, PacketContext::Lrproof);

    let payload = &proof_wire[19..];
    assert_eq!(
        payload.len(),
        99,
        "LRPROOF payload must be signature(64) + x25519_pub(32) + signalling(3) \
         (Link.py:400-405 length gate)"
    );
    let signature = &payload[..64];
    let responder_pub = &payload[64..96];
    let signalling = &payload[96..99];

    // Mode equality rule (Link.py:403): a differing mode raises and closes.
    let raw_signalling =
        ((signalling[0] as u32) << 16) | ((signalling[1] as u32) << 8) | signalling[2] as u32;
    assert_eq!(
        (raw_signalling >> 21) & 0x07,
        crate::constants::MODE_AES256_CBC as u32,
        "proof mode must equal the requested mode or the initiator closes"
    );
    assert_eq!(
        raw_signalling & 0x1F_FFFF,
        crate::constants::MTU as u32,
        "confirmed MTU must be the negotiated link MTU"
    );

    // Reference signed data (Link.py:417): link_id + peer_pub + DESTINATION
    // identity ed25519 pub (taken from the known identity, not the wire).
    let mut signed = Vec::new();
    signed.extend_from_slice(caller_link_id.as_bytes());
    signed.extend_from_slice(responder_pub);
    signed.extend_from_slice(&signing_key);
    signed.extend_from_slice(signalling);

    let vk = VerifyingKey::from_bytes(&signing_key).unwrap();
    let sig = Signature::from_bytes(signature.try_into().unwrap());
    vk.verify(&signed, &sig)
        .expect("reference-composed LRPROOF signed data must verify");

    // The pin must bite: a signature that did not cover the signalling
    // (what a writer signing only link_id+keys would produce) must fail.
    assert!(
        vk.verify(&signed[..signed.len() - 3], &sig).is_err(),
        "proof signature must actually cover the signalling bytes"
    );
}

/// #159 tranche 2: pin the LRRTT payload semantics: an encrypted msgpack
/// float64 carrying the initiator's measured RTT in SECONDS (Link.py:434-436).
/// The responder adopts `max(own_measurement, value)` (Link.py:534-540) and
/// derives keepalive/stale/timeout arithmetic from it — a value in the wrong
/// unit silently reshapes every liveness timer on the peer.
///
/// Documented deviation: we floor the wire value at 0.5 s
/// (`CHANNEL_DEFAULT_RTT_MS`, link_management.rs) so a sub-ms measurement
/// cannot collapse the responder's channel window tier into a retransmit
/// storm. The floor preserves the unit and direction of the reference rule
/// (the responder's `max()` only ever raises its estimate).
#[test]
fn lrrtt_wire_is_msgpack_float64_of_link_rtt_seconds() {
    // Case 1: measured RTT 2000 ms — above the floor, must ride unmodified.
    let (mut responder, dest_hash, signing_key) = make_responder("rtt1");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");

    let (_lr, _proof, rtt_wire, caller_link_id, _resp) = establish_captured(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
        2000,
    );

    let plaintext = decrypt_link_payload(&initiator, &caller_link_id, &rtt_wire);
    assert_eq!(plaintext.len(), 9, "msgpack float64 is 1 marker + 8 bytes");
    assert_eq!(plaintext[0], 0xCB, "RTT must be a msgpack float64");
    let rtt_secs = f64::from_be_bytes(plaintext[1..9].try_into().unwrap());
    assert_eq!(
        rtt_secs, 2.0,
        "a 2000 ms measurement must ride as 2.0 SECONDS (a millisecond \
         writer would emit 2000.0 and stretch the peer's timers 1000x)"
    );

    // Case 2: instantaneous handshake — the 0.5 s floor applies.
    let (mut responder2, dest_hash2, signing_key2) = make_responder("rtt2");
    let r2 = add_iface(&mut responder2, "r0");
    let mut initiator2 = make_initiator();
    let i2 = add_iface(&mut initiator2, "i0");
    let (_lr2, _proof2, rtt_wire2, caller2, _resp2) = establish_captured(
        &mut initiator2,
        i2,
        &mut responder2,
        r2,
        dest_hash2,
        signing_key2,
        0,
    );
    let plaintext2 = decrypt_link_payload(&initiator2, &caller2, &rtt_wire2);
    assert_eq!(plaintext2[0], 0xCB);
    let rtt_secs2 = f64::from_be_bytes(plaintext2[1..9].try_into().unwrap());
    assert_eq!(
        rtt_secs2,
        crate::constants::CHANNEL_DEFAULT_RTT_MS as f64 / 1000.0,
        "a 0 ms measurement must be floored, never sent as 0.0 (deviation \
         documented in link_management.rs)"
    );
}

// ----------------------------------------------------------------------------
// Established-link fields
// ----------------------------------------------------------------------------

fn establish(
    initiator: &mut EndpointNode,
    i_iface: usize,
    responder: &mut EndpointNode,
    r_iface: usize,
    dest_hash: crate::DestinationHash,
    signing_key: [u8; 32],
) -> (LinkId, LinkId) {
    let (_lr, _proof, _rtt, caller, resp) = establish_captured(
        initiator,
        i_iface,
        responder,
        r_iface,
        dest_hash,
        signing_key,
        0,
    );
    (caller, resp)
}

/// #159 tranche 2: pin the REQUEST wire against the reference receiver.
///
/// A Python responder computes the request id as the TRUNCATED PACKET HASH of
/// the request packet (Link.py:1035 `packet.getTruncatedHash()`) and the
/// initiator later matches the response by that id (Link.py:910) — so the id
/// we hand our caller must equal the reference recomputation from the wire
/// bytes, or every response correlation fails. The payload is msgpack
/// `[timestamp, truncated_hash(path), data]` (Link.py:489-491); the peer
/// dispatches on element 1 against its registered path hashes (Link.py:861).
///
/// Element 0 is `time.time()` in the reference (Link.py:490) — epoch
/// seconds, handed to every registered request handler as `requested_at`
/// (Link.py:878-880). Pinned here through the #155 emission-timebase
/// machinery: with a known wall timebase the wire float must carry epoch
/// seconds, never process uptime (Codeberg #164) — an uptime value sits far
/// below `EMISSION_PLAUSIBLE_MIN_SECS` and fails the bound.
#[test]
fn request_wire_pins_reference_request_semantics() {
    let (mut responder, dest_hash, signing_key) = make_responder("req");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");
    let (caller_id, _resp_id) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    // Seed the #155 wall timebase (the MockClock itself is clockless).
    const WALL_SECS: u64 = 1_700_000_000;
    assert!(initiator
        .transport
        .set_wall_time_unix_secs(WALL_SECS, crate::transport::TimeSource::Host));

    // One msgpack bin value as request data.
    let mut req_data = Vec::new();
    crate::resource::msgpack::write_bin(&mut req_data, b"ping");

    let (request_id, out) = initiator
        .send_request(&caller_id, "/echo", Some(&req_data), None)
        .unwrap();
    let wire = action_data(&out)
        .into_iter()
        .find(|p| p.len() > 18 && p[18] == PacketContext::Request.to_byte())
        .expect("send_request must emit a REQUEST context packet");

    assert_eq!(
        request_id,
        reference_truncated_packet_hash(&wire),
        "our request id must equal the Python peer's packet.getTruncatedHash()"
    );

    // Reference payload composition (Link.py:490).
    let plaintext = decrypt_link_payload(&initiator, &caller_id, &wire);
    assert_eq!(plaintext[0], 0x93, "payload must be a msgpack fixarray(3)");
    assert_eq!(
        plaintext[1], 0xCB,
        "element 0 must be a msgpack float64 timestamp"
    );
    // Element 0 value: epoch seconds from the #155 timebase, exactly the
    // seeded floor here (the MockClock does not advance). Process uptime —
    // today's bug shape — sits below EMISSION_PLAUSIBLE_MIN_SECS and fails.
    let ts = f64::from_be_bytes(plaintext[2..10].try_into().unwrap());
    assert!(
        ts >= crate::constants::EMISSION_PLAUSIBLE_MIN_SECS as f64,
        "element 0 must carry epoch seconds like the reference's time.time() \
         (Link.py:490), not process uptime (Codeberg #164): got {ts}"
    );
    assert_eq!(
        ts, WALL_SECS as f64,
        "element 0 must come from the #155 emission timebase"
    );
    // Element 1: bin8(16) truncated path hash — the peer's dispatch key.
    assert_eq!(&plaintext[10..12], &[0xC4, 0x10]);
    assert_eq!(
        &plaintext[12..28],
        &crate::crypto::truncated_hash(b"/echo"),
        "element 1 must be truncated_hash(utf8 path): the peer dispatches on it"
    );
    // Element 2: the caller's msgpack value, verbatim.
    assert_eq!(&plaintext[28..], &req_data[..]);

    // Close the loop through the peer's actual decision.
    let (_out, events) = deliver_collect(&mut responder, r_iface, std::vec![wire]);
    let received = events
        .iter()
        .find_map(|ev| match ev {
            NodeEvent::RequestReceived {
                request_id: rid,
                path,
                data,
                requested_at,
                ..
            } => Some((*rid, path.clone(), data.clone(), *requested_at)),
            _ => None,
        })
        .expect("responder must dispatch the request");
    assert_eq!(received.0, request_id);
    assert_eq!(received.1, "/echo");
    assert_eq!(received.2, req_data);
    assert_eq!(
        received.3, ts,
        "the handler's requested_at must be the wire timestamp verbatim, \
         like the reference (Link.py:878-880)"
    );
}

/// #164, resource path: `send_request_resource` fills the same payload
/// element 0, so a request too large for a single packet must also carry
/// epoch seconds from the #155 timebase. Pinned through the peer's actual
/// dispatch: the full ADV/REQ/part/proof exchange runs sans-I/O and the
/// responder's `RequestReceived.requested_at` — documented "seconds since
/// epoch" — must sit at the seeded wall timebase, not at process uptime.
#[test]
fn request_resource_timestamp_carries_epoch_seconds() {
    let (mut responder, dest_hash, signing_key) = make_responder("req");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");
    let (caller_id, _resp_id) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    const WALL_SECS: u64 = 1_700_000_000;
    assert!(initiator
        .transport
        .set_wall_time_unix_secs(WALL_SECS, crate::transport::TimeSource::Host));

    // One msgpack bin value, larger than the link MDU.
    let mut req_data = Vec::new();
    crate::resource::msgpack::write_bin(&mut req_data, &std::vec![0x5a_u8; 1_400]);

    let (request_id, out) = {
        let (rid, _hash, out) = initiator
            .send_request_resource(&caller_id, "/echo", Some(&req_data), None)
            .unwrap();
        (rid, out)
    };

    // Ping-pong the resource transfer to completion.
    let mut for_responder = action_data(&out);
    let mut received = None;
    for _ in 0..16 {
        if for_responder.is_empty() {
            break;
        }
        let (back, events) = deliver_collect(&mut responder, r_iface, for_responder);
        received = received.or_else(|| {
            events.iter().find_map(|ev| match ev {
                NodeEvent::RequestReceived {
                    request_id: rid,
                    requested_at,
                    ..
                } => Some((*rid, *requested_at)),
                _ => None,
            })
        });
        let (fwd, _) = deliver_collect(&mut initiator, i_iface, back);
        for_responder = fwd;
    }

    let (rid, requested_at) = received.expect("responder must dispatch the resource-borne request");
    assert_eq!(rid, request_id);
    assert!(
        requested_at >= crate::constants::EMISSION_PLAUSIBLE_MIN_SECS as f64,
        "requested_at must carry epoch seconds like the reference's \
         time.time() (Link.py:490), not process uptime (Codeberg #164): \
         got {requested_at}"
    );
    assert_eq!(
        requested_at, WALL_SECS as f64,
        "requested_at must come from the #155 emission timebase"
    );
}

/// #159 tranche 2: pin the RESPONSE wire: msgpack `[request_id, response]`
/// (Link.py:897). The initiator unpacks element 0 and matches it against its
/// pending requests (Link.py:1050 + :910); a response whose embedded id does
/// not equal the id the requester derived from its own request packet is
/// silently dropped.
#[test]
fn response_wire_pins_reference_response_semantics() {
    let (mut responder, dest_hash, signing_key) = make_responder("resp");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");
    let (caller_id, resp_id) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    let (request_id, out) = initiator
        .send_request(&caller_id, "/echo", None, None)
        .unwrap();
    let req_wire = action_data(&out)
        .into_iter()
        .find(|p| p.len() > 18 && p[18] == PacketContext::Request.to_byte())
        .unwrap();
    let (_out, _ev) = deliver_collect(&mut responder, r_iface, std::vec![req_wire]);

    // Respond with msgpack true (0xC3).
    let out = responder
        .send_response(&resp_id, &request_id, &[0xC3])
        .unwrap();
    let resp_wire = action_data(&out)
        .into_iter()
        .find(|p| p.len() > 18 && p[18] == PacketContext::Response.to_byte())
        .expect("send_response must emit a RESPONSE context packet");

    let plaintext = decrypt_link_payload(&responder, &resp_id, &resp_wire);
    assert_eq!(plaintext[0], 0x92, "payload must be a msgpack fixarray(2)");
    assert_eq!(
        &plaintext[1..3],
        &[0xC4, 0x10],
        "element 0 must be a bin8(16) request id"
    );
    assert_eq!(
        &plaintext[3..19],
        &request_id,
        "embedded id must equal the id the requester derived from its own \
         request packet — the reference matching key (Link.py:910)"
    );
    assert_eq!(&plaintext[19..], &[0xC3]);

    // Close the loop: the initiator's pending request is consumed by it.
    let (_out, events) = deliver_collect(&mut initiator, i_iface, std::vec![resp_wire]);
    let matched = events.iter().any(|ev| {
        matches!(ev, NodeEvent::ResponseReceived { request_id: rid, response_data, .. }
            if *rid == request_id && response_data == &[0xC3])
    });
    assert!(
        matched,
        "initiator must correlate the response by embedded id"
    );
}

/// #159 tranche 2: pin the LINKIDENTIFY payload by reconstructing the signed
/// data in reference order (Link.py:469-470 `link_id + public_key`, verified
/// by the peer at :1010-1020 with the Ed25519 half of the WIRE public key).
/// The peer sets the link's remote identity from these bytes — this is
/// authentication, a forged or reordered composition must never verify.
#[test]
fn identify_wire_verifies_under_reference_composition() {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let (mut responder, dest_hash, signing_key) = make_responder("ident");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");
    let (caller_id, _resp_id) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    let app_identity = Identity::generate(&mut OsRng);
    let out = initiator.identify_link(&caller_id, &app_identity).unwrap();
    let wire = action_data(&out)
        .into_iter()
        .find(|p| p.len() > 18 && p[18] == PacketContext::LinkIdentify.to_byte())
        .expect("identify_link must emit a LINKIDENTIFY packet");

    let plaintext = decrypt_link_payload(&initiator, &caller_id, &wire);
    assert_eq!(
        plaintext.len(),
        128,
        "identify plaintext must be public_key(64) + signature(64) \
         (Link.py:1013 length gate)"
    );
    let public_key = &plaintext[..64];
    assert_eq!(public_key, &app_identity.public_key_bytes());

    // Reference verification: ed25519 half of the wire key over
    // link_id + public_key.
    let ed25519_half: [u8; 32] = public_key[32..64].try_into().unwrap();
    let vk = VerifyingKey::from_bytes(&ed25519_half).unwrap();
    let sig = Signature::from_bytes(plaintext[64..128].try_into().unwrap());
    let mut signed = Vec::new();
    signed.extend_from_slice(caller_id.as_bytes());
    signed.extend_from_slice(public_key);
    vk.verify(&signed, &sig)
        .expect("reference-composed identify signed data must verify");

    // The pin must bite: without the link id (an unbound identity proof that
    // could be replayed onto another link) verification must fail.
    assert!(
        vk.verify(&signed[TRUNCATED_HASHBYTES..], &sig).is_err(),
        "identify signature must actually bind the link id"
    );

    // Close the loop through the peer's decision.
    let (_out, events) = deliver_collect(&mut responder, r_iface, std::vec![wire]);
    let identified = events.iter().any(|ev| {
        matches!(ev, NodeEvent::LinkIdentified { identity_hash, .. }
            if identity_hash == app_identity.hash())
    });
    assert!(identified, "responder must adopt the identified identity");
}

/// #159 tranche 2: pin the LINKCLOSE payload: the encrypted plaintext must be
/// exactly the link id (Link.py:694-695); the peer tears the link down IFF
/// the decrypted bytes equal its own link id (Link.py:710-713). Any other
/// plaintext is ignored and the link stays up.
#[test]
fn link_close_wire_plaintext_is_the_link_id() {
    let (mut responder, dest_hash, signing_key) = make_responder("close");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");
    let (caller_id, resp_id) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    // Decrypt the close payload with the responder's link BEFORE delivering
    // (the responder link is gone afterwards).
    let out = initiator.close_link(&caller_id);
    let wire = action_data(&out)
        .into_iter()
        .find(|p| p.len() > 18 && p[18] == PacketContext::LinkClose.to_byte())
        .expect("close_link must emit a LINKCLOSE packet");

    let plaintext = decrypt_link_payload(&responder, &resp_id, &wire);
    assert_eq!(
        plaintext,
        caller_id.as_bytes(),
        "close plaintext must be the link id: the peer's teardown condition"
    );

    let (_out, events) = deliver_collect(&mut responder, r_iface, std::vec![wire]);
    let closed = events
        .iter()
        .any(|ev| matches!(ev, NodeEvent::LinkClosed { link_id, .. } if *link_id == resp_id));
    assert!(closed, "peer must tear down on a matching close plaintext");
}

/// #159 tranche 2: pin the Channel envelope against the reference receiver.
///
/// Wire form `>HHH` msgtype/sequence/length (Channel.py:196); a fresh link's
/// receiver starts at `_next_rx_sequence = 0` (Channel.py:293) and delivers
/// strictly in sequence (:450-452), so our FIRST envelope on a new link must
/// carry sequence 0 and count up by exactly one — an uptime- or rekey-seeded
/// counter would park every message in the peer's ring until timeout.
#[test]
fn channel_envelope_wire_pins_reference_sequence_semantics() {
    let (mut responder, dest_hash, signing_key) = make_responder("chan");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");
    let (caller_id, _resp_id) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    let mut envelopes = Vec::new();
    for payload in [&b"alpha"[..], &b"beta"[..]] {
        let out = initiator.send_on_link(&caller_id, payload).unwrap();
        let wire = action_data(&out)
            .into_iter()
            .find(|p| p.len() > 18 && p[18] == PacketContext::Channel.to_byte())
            .expect("send_on_link must emit a CHANNEL context packet");
        envelopes.push(decrypt_link_payload(&initiator, &caller_id, &wire));
    }

    for (i, (env, payload)) in envelopes.iter().zip([&b"alpha"[..], b"beta"]).enumerate() {
        let msgtype = u16::from_be_bytes([env[0], env[1]]);
        let sequence = u16::from_be_bytes([env[2], env[3]]);
        let length = u16::from_be_bytes([env[4], env[5]]);
        assert_eq!(msgtype, 0x0000, "raw byte messages ride as msgtype 0");
        assert_eq!(
            sequence, i as u16,
            "a fresh link's envelopes must count 0, 1, ... — the peer's \
             _next_rx_sequence starts at 0 and delivers strictly in order"
        );
        assert_eq!(length as usize, payload.len());
        assert_eq!(&env[6..], payload);
    }
}

/// #159 tranche 2: pin the packet-proof (delivery receipt) payload: the FULL
/// packet hash (32 bytes, over the reference hashable part) followed by an
/// Ed25519 signature of that hash (Link.py:383-394 prove_packet), sent
/// unencrypted (Packet.py:200-201). The requester matches its receipt by the
/// embedded hash and validates the signature with the responder's identity
/// key — a hash computed over different bytes orphans every receipt.
#[test]
fn data_proof_wire_carries_reference_packet_hash_and_signature() {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let (mut responder, dest_hash, signing_key) = make_responder("proof");
    let r_iface = add_iface(&mut responder, "r0");
    let mut initiator = make_initiator();
    let i_iface = add_iface(&mut initiator, "i0");
    let (caller_id, _resp_id) = establish(
        &mut initiator,
        i_iface,
        &mut responder,
        r_iface,
        dest_hash,
        signing_key,
    );

    let (_hash, out) = initiator
        .send_packet_on_link(&caller_id, b"prove me")
        .unwrap();
    let data_wire = action_data(&out)
        .into_iter()
        .find(|p| p.len() > 18 && p[18] == PacketContext::None.to_byte())
        .expect("send_packet_on_link must emit a plain DATA packet");

    // Responder proves (destination strategy PROVE_ALL).
    let (resp_out, _ev) = deliver_collect(&mut responder, r_iface, std::vec![data_wire.clone()]);
    let proof_wire = resp_out
        .into_iter()
        .find(|p| {
            Packet::unpack(p)
                .map(|pk| pk.flags.packet_type == PacketType::Proof)
                .unwrap_or(false)
        })
        .expect("responder must prove the delivered packet");

    let payload = &proof_wire[19..];
    assert_eq!(
        payload.len(),
        96,
        "explicit proof is hash(32) + signature(64)"
    );

    // Reference packet hash: sha256 over the hashable part of the DATA
    // packet's wire bytes (Packet.py:346-361).
    let mut hashable = std::vec![data_wire[0] & 0x0F];
    hashable.extend_from_slice(&data_wire[2..]);
    let ref_hash = crate::crypto::sha256(&hashable);
    assert_eq!(
        &payload[..32],
        &ref_hash,
        "proof must embed the reference full packet hash: the requester's \
         receipt lookup key"
    );

    // Signature by the responder's destination identity (Link.py:279: an
    // incoming link signs with the owner identity's key).
    let vk = VerifyingKey::from_bytes(&signing_key).unwrap();
    let sig = Signature::from_bytes(payload[32..96].try_into().unwrap());
    vk.verify(&ref_hash, &sig)
        .expect("proof signature must verify against the destination identity");

    // Close the loop: the initiator confirms delivery from this proof.
    let (_out, events) = deliver_collect(&mut initiator, i_iface, std::vec![proof_wire]);
    let confirmed = events
        .iter()
        .any(|ev| matches!(ev, NodeEvent::LinkDeliveryConfirmed { .. }));
    assert!(confirmed, "initiator must confirm delivery from the proof");
}

/// #159 tranche 3, settling the open question from the #158 discussion: what
/// does a node emit when a path request names one of its OWN destinations?
///
/// The reference REGENERATES the announce (`Transport.path_request`,
/// Transport.py:2939-2941 calls `destination.announce(path_response=True)`;
/// Destination.py:281-307 builds fresh `random_hash` — 5 random bytes + the
/// current unix-seconds emission — a fresh signature, applies the
/// `default_app_data` fallback at :289-293, and sets the PATH_RESPONSE
/// context at :307). Peers decide on each of those: the emission timestamp
/// orders the path against older entries (Transport.py:1765-1772, the #155
/// rule), the random half is the replay-dedup key (:1772), the app_data is
/// what applications read, and the PATH_RESPONSE context suppresses
/// rebroadcast (:1886).
///
/// We match: Transport emits `PathRequestReceived` and NodeCore regenerates
/// via `dest.announce(None, ..)` (node/mod.rs Block A), where `None` falls
/// back to the destination's default app data (destination.rs:923-932). Every
/// expectation below is read from the response wire bytes and compared
/// against values recomposed from the ORIGINAL announce's wire bytes.
#[test]
fn own_destination_path_response_is_a_fresh_regeneration() {
    use crate::destination::DestinationType as DT;
    use crate::memory_storage::MemoryStorage;
    use crate::packet::{HeaderType, PacketData, PacketFlags, TransportType};
    use crate::traits::Clock;

    // KAT from the vendored reference:
    // RNS.Destination.hash(None, "rnstransport", "path", "request").
    const PATH_REQUEST_DEST_KAT: [u8; TRUNCATED_HASHBYTES] = [
        0x6b, 0x9f, 0x66, 0x01, 0x4d, 0x98, 0x53, 0xfa, 0xab, 0x22, 0x0f, 0xba, 0x47, 0xd0, 0x27,
        0x61,
    ];

    let emission_of = |announce_payload: &[u8]| -> u64 {
        // Type1 announce payload: pk(64) + name(10) + random(10) + sig + app;
        // emission is random bytes 5..10, big-endian (Destination.py:282).
        let ts = &announce_payload[79..84];
        ((ts[0] as u64) << 32)
            | ((ts[1] as u64) << 24)
            | ((ts[2] as u64) << 16)
            | ((ts[3] as u64) << 8)
            | (ts[4] as u64)
    };

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
        "fieldpins",
        &["ownpr"],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    node.register_destination(dest);

    // t0: the application announces with explicit app data.
    let app_data = b"own-pr-appdata";
    let out = node
        .announce_destination(&dest_hash, Some(app_data))
        .unwrap();
    let announce0 = action_data(&out)
        .into_iter()
        .next()
        .expect("announce must be emitted");
    let payload0 = announce0[19..].to_vec();
    let random0 = payload0[74..84].to_vec();
    let sig0 = payload0[84..148].to_vec();

    // t0 + 5s: a peer's path request names our own destination.
    let now = node.transport().clock().now_ms();
    node.transport().clock().set(now + 5_000);
    let mut pr_data = Vec::new();
    pr_data.extend_from_slice(dest_hash.as_bytes());
    pr_data.extend_from_slice(&[0xB7u8; TRUNCATED_HASHBYTES]); // tag
    let request = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DT::Plain,
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
    // requesting interface.
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
        .expect("own-destination path response must target the requesting interface");

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

    let payload1 = &response[19..];
    let random1 = &payload1[74..84];
    let sig1 = &payload1[84..148];
    assert_ne!(
        random1,
        &random0[..],
        "the response must be a fresh emission, not a replay of the cached blob"
    );
    assert_eq!(
        emission_of(payload1),
        emission_of(&payload0) + 5,
        "the fresh emission must carry the CURRENT time, 5s after the original"
    );
    assert_ne!(
        sig1,
        &sig0[..],
        "a fresh random blob forces a fresh signature"
    );
    assert_eq!(
        &payload1[148..],
        app_data,
        "regeneration with `None` must fall back to the announced app data \
         (default_app_data, Destination.py:289-293)"
    );
}
