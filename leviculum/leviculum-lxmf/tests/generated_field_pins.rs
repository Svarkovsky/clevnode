//! Codeberg #159 tranche 4: pins for the fields LXMF *generates*.
//!
//! Every test here answers the three audit questions of
//! `docs/src/concepts/wire-field-semantics.md` for one generated field: what
//! the reference writes (citation into `reference/LXMF`), what a peer DECIDES
//! from it, and whether our value satisfies that rule. Expected values are
//! recomposed independently of the writer — either from a known-answer vector
//! produced by importing the vendored Python reference
//! (`docs/src/appendix/lxmf/vectors/gen_vectors.py`), or by rebuilding the
//! reference's own formula in the test body. Nothing here may call the helper
//! whose output it checks.

mod common;

use leviculum_core::crypto::full_hash;
use leviculum_core::Identity;
use leviculum_lxmf::constants::{
    COST_TICKET, TICKET_EXPIRY, TICKET_GRACE, TICKET_INTERVAL, TICKET_LENGTH, TICKET_RENEW,
};
use leviculum_lxmf::propagation::PropagationUpload;
use leviculum_lxmf::stamp::ticket_stamp;
use leviculum_lxmf::ticket::{Ticket, TicketStore};
use leviculum_lxmf::{DeliveryMethod, Message, MessageError};
use rand_core::OsRng;

fn source_identity() -> Identity {
    Identity::from_private_key_bytes(
        &hex::decode(common::fixture("VEC-MSG-1", "src_identity_prv_hex")).unwrap(),
    )
    .unwrap()
}

fn hex_fixture(id: &str, field: &str) -> Vec<u8> {
    hex::decode(common::fixture(id, field)).unwrap()
}

/// Rebuild the ticket-stamped reference message with our own writer.
///
/// The inputs are read from the vector; the bytes are never copied from it.
fn ticketed_message() -> Message {
    let ticket_field_value = hex_fixture("VEC-MSG-TICKET", "ticket_field_msgpack_hex");
    let mut message = Message::create(
        hex_fixture("VEC-MSG-TICKET", "destination_hash")
            .try_into()
            .unwrap(),
        hex_fixture("VEC-MSG-TICKET", "source_hash")
            .try_into()
            .unwrap(),
        &source_identity(),
        1_700_000_000.0,
        b"T".to_vec(),
        b"ticketed".to_vec(),
        vec![(0x0C, ticket_field_value)],
        DeliveryMethod::Direct,
    )
    .expect("build the ticket-stamped reference message");
    let stamp = ticket_stamp(
        &hex_fixture("VEC-MSG-TICKET", "outbound_ticket_hex")
            .try_into()
            .unwrap(),
        &message.message_id,
    );
    message.set_stamp(stamp.to_vec()).expect("16-byte stamp");
    message
}

/// The message ID and the signature cover the FOUR-element payload; the stamp
/// is appended afterwards and is covered by neither.
///
/// Reference (`LXMessage.pack`, LXMessage.py:362-380): `self.payload` is built
/// with four elements, `hashed_part = destination.hash + source.hash +
/// msgpack.packb(payload)`, `self.hash = full_hash(hashed_part)`, and only
/// *then* is the stamp appended to `self.payload` (:371-373). The signature is
/// taken over `hashed_part + self.hash` (:375-378), and the wire payload is
/// re-packed with five elements at :380.
///
/// What a peer DECIDES: `unpack_from_bytes` (LXMessage.py:754-762) strips
/// `unpacked_payload[4]`, re-packs the remaining four elements, and validates
/// the signature over that recomposition (:766, :814-819). A writer that
/// folded the stamp into the hash would produce a message every Python peer
/// rejects as `SIGNATURE_INVALID` — and that our own reader would accept,
/// because it strips the stamp symmetrically. That is the #155 class exactly.
///
/// This test recomposes the signed data from the vector's own byte fields and
/// verifies with the source identity, never through `Message::unpack`, whose
/// composition is shared with the writer.
#[test]
fn message_id_and_signature_exclude_the_stamp() {
    let message = ticketed_message();
    let expected_packed = hex_fixture("VEC-MSG-TICKET", "packed_hex");

    // Ed25519 is deterministic (RFC 8032), so byte equality against the
    // reference's own output proves the signed-data composition, not just the
    // framing.
    assert_eq!(
        hex::encode(message.pack()),
        hex::encode(&expected_packed),
        "packed bytes must equal the reference's ticket-stamped message"
    );
    assert_eq!(
        hex::encode(message.message_id),
        common::fixture("VEC-MSG-TICKET", "message_id_hex")
    );

    // Independent recomposition: destination || source || four-element payload.
    let hashed_part = hex_fixture("VEC-MSG-TICKET", "hashed_part_hex");
    assert_eq!(
        hex::encode(full_hash(&hashed_part)),
        common::fixture("VEC-MSG-TICKET", "message_id_hex"),
        "the message ID is full_hash over the unstamped payload"
    );
    let mut signed_part = hashed_part.clone();
    signed_part.extend_from_slice(&message.message_id);
    assert!(source_identity()
        .verify(&signed_part, &message.signature)
        .unwrap());

    // The pin bites: folding the stamp into the signed data breaks it. This is
    // the composition a naive writer would produce.
    let mut stamped_signed_part = hex_fixture("VEC-MSG-TICKET", "stamped_payload_msgpack_hex");
    let mut wrong = expected_packed[..32].to_vec();
    wrong.append(&mut stamped_signed_part);
    let wrong_id = full_hash(&wrong);
    wrong.extend_from_slice(&wrong_id);
    assert!(
        !source_identity()
            .verify(&wrong, &message.signature)
            .unwrap(),
        "a signature over the stamped payload must not verify"
    );
    assert_ne!(
        wrong_id, message.message_id,
        "hashing the stamped payload must not reproduce the message ID"
    );
}

/// The message timestamp is msgpack float64 unix seconds, at a fixed width.
///
/// Reference: `self.timestamp = time.time()` (LXMessage.py:357) — a Python
/// float, which umsgpack always encodes as the 9-byte float64 form `0xcb ||
/// be64`. Two things decide on the width rather than the value:
/// `content_size = len(packed_payload) - TIMESTAMP_SIZE - STRUCT_OVERHEAD`
/// (:386) with `TIMESTAMP_SIZE = 8` (:59-60), which selects packet versus
/// resource representation (:396-424). An integer-encoded timestamp would
/// shift that arithmetic and let a message the sender believes fits exceed the
/// peer's single-packet limit.
///
/// Our encoder writes `msgpack::f64` unconditionally, so the marker is `0xcb`
/// for every value. Checked here at the epoch, at a fractional value, and at
/// the smallest and largest finite doubles — the field's representable limit —
/// none of which change the 9-byte width. Unlike the announce emission
/// timestamp there is no truncating wire width to saturate against
/// (`docs/src/concepts/time-and-clocks.md`, "The wire field is 40 bits"):
/// float64 holds every unix second exactly.
#[test]
fn message_timestamp_is_always_msgpack_float64() {
    for timestamp in [
        0.0,
        1_700_000_000.0,
        1_700_000_000.25,
        -1.0,
        f64::MIN,
        f64::MAX,
    ] {
        let message = Message::create(
            [1; 16],
            [2; 16],
            &source_identity(),
            timestamp,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .expect("any finite timestamp is accepted");
        let payload = &message.pack()[96..];
        assert_eq!(payload[0], 0x94, "four-element payload array");
        assert_eq!(payload[1], 0xcb, "timestamp must use the float64 marker");
        assert_eq!(
            f64::from_be_bytes(payload[2..10].try_into().unwrap()).to_bits(),
            timestamp.to_bits(),
            "the timestamp is emitted verbatim, not rounded or clamped"
        );
    }

    // The reference emits `time.time()` unvalidated (LXMessage.py:357), so we
    // do too. Pinned as a deliberate non-behaviour: this crate has no clock of
    // its own and applies no plausibility window to the caller's value, mirroring
    // `docs/src/concepts/time-and-clocks.md`, "We do not validate our own clock".
    // The one exception is the finiteness refusal below, which is not a
    // plausibility window: it excludes values that are not times at all.
    let uptime_stamped = Message::create(
        [1; 16],
        [2; 16],
        &source_identity(),
        42.0,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        DeliveryMethod::Direct,
    )
    .expect("an implausible timestamp is not refused");
    assert_eq!(uptime_stamped.timestamp, 42.0);
}

/// We refuse to sign a timestamp that is `NaN` or infinite. Codeberg #184.
///
/// This pin replaces the tranche-4 pin that recorded the opposite as
/// deliberate. What changed is the argument, not the reference: the reference
/// still writes `time.time()` with no validation (LXMessage.py:357), and
/// `time.time()` cannot be non-finite, so the reference's silence on this is
/// not a decision we are deviating from — it is a case its writer cannot
/// produce.
///
/// Question 1 of the audit method, answered against the pinned snapshot: LXMF
/// core decides *nothing* from an inbound message's timestamp. It stores it
/// (`message.timestamp = timestamp`, LXMessage.py:797) and hands it to the
/// application; the propagation store weights and expires entries by the
/// *receive* time instead (`get_weight`, LXMRouter.py:1056-1067, and the store
/// filename, :1150-1158). The decision rule therefore lives in a client we
/// cannot cite, and the only thing every client does with the field is order
/// and display by it. A `NaN` makes both meaningless: every comparison against
/// it is False in both directions, so it sorts arbitrarily and non-transitively
/// and no "newer than" test can ever select it. We cannot bound the damage,
/// which is the argument for not emitting it.
///
/// The refusal is wire-invisible — there is no value a conforming peer expects
/// from us that we now withhold — and it costs no caller anything: since
/// Codeberg #182 the router resolves the field from `NodeCore::emission_secs`,
/// a `u64`, so our own path cannot produce a non-finite value at all. It also
/// settles a split the crate had been carrying: `Ticket::from_field_value` and
/// the `TicketStore` lookups already refused non-finite values, while
/// `Message::create` did not.
///
/// The read side stays permissive on purpose — see
/// `foreign_payload_encodings.rs`, `implausible_but_numeric_timestamps_are_accepted`.
/// Refusing to emit a value costs no peer anything; refusing to accept one
/// costs the sender its message, which is the Codeberg #183 defect class.
#[test]
fn a_non_finite_message_timestamp_is_never_signed() {
    for timestamp in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            Message::create(
                [1; 16],
                [2; 16],
                &source_identity(),
                timestamp,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                DeliveryMethod::Direct,
            )
            .unwrap_err(),
            MessageError::NonFiniteTimestamp,
            "a non-finite timestamp must be refused by name, not silently packed"
        );
    }

    // The refusal is exactly the non-finite set: the extremes of the finite
    // range still pass, so this is not a plausibility window in disguise.
    for timestamp in [f64::MIN, f64::MAX, -0.0, f64::MIN_POSITIVE] {
        assert!(Message::create(
            [1; 16],
            [2; 16],
            &source_identity(),
            timestamp,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .is_ok());
    }
}

/// A ticket travels as `[expires, ticket]` and a peer keeps it only while
/// `time.time() < expires` — an absolute wall-clock comparison across nodes.
///
/// Reference: `generate_ticket` returns `[now+TICKET_EXPIRY, os.urandom(16)]`
/// (LXMRouter.py:1073-1100) and the router stores it under `FIELD_TICKET`
/// (:1770-1772). The receiving side decides at LXMRouter.py:1848-1856: the
/// value must be a list of at least two elements, `time.time() < expires` must
/// hold *on the receiver's clock*, and the secret must be exactly
/// `TICKET_LENGTH` bytes — otherwise the ticket is silently dropped and every
/// reply from that peer keeps paying full proof-of-work.
///
/// Because the comparison is against the receiver's wall clock, this field has
/// the #155 property: a node stamping uptime seconds issues tickets that no
/// Python peer will ever accept, while two such nodes accept each other's
/// happily. The expiry is checked here against an independently computed
/// reference value, and the ticket-lifecycle constants are pinned against the
/// reference class attributes (LXMessage.py:42, :49-53).
#[test]
fn issued_ticket_carries_reference_expiry_and_encoding() {
    assert_eq!(TICKET_LENGTH, 16, "RNS.Identity.TRUNCATED_HASHLENGTH//8");
    assert_eq!(TICKET_EXPIRY, 21 * 24 * 60 * 60);
    assert_eq!(TICKET_GRACE, 5 * 24 * 60 * 60);
    assert_eq!(TICKET_RENEW, 14 * 24 * 60 * 60);
    assert_eq!(TICKET_INTERVAL, 24 * 60 * 60);
    assert_eq!(COST_TICKET, 0x100);

    let now_unix = 1_700_000_000.0;
    let mut store = TicketStore::default();
    let issued = store
        .issue([9; 16], now_unix, &mut OsRng)
        .expect("first issue is never throttled");

    // Recomposed from the reference expression, not read back from the store.
    assert_eq!(
        issued.expires_unix,
        now_unix + 21.0 * 24.0 * 60.0 * 60.0,
        "expires = now + TICKET_EXPIRY (LXMRouter.py:1095)"
    );
    assert_eq!(issued.secret.len(), TICKET_LENGTH);

    // The peer's acceptance rule, applied to our own issued value.
    assert!(
        now_unix < issued.expires_unix,
        "a peer keeps the ticket only while now < expires"
    );

    // The wire encoding is the reference's `[expires, ticket]` list. Compared
    // against bytes msgpack-packed by the vendored Python reference.
    let vector_ticket = Ticket {
        expires_unix: 1_700_000_000.0 + TICKET_EXPIRY as f64,
        secret: hex_fixture("VEC-MSG-TICKET", "field_ticket_secret_hex")
            .try_into()
            .unwrap(),
    };
    assert_eq!(
        hex::encode(vector_ticket.field_value()),
        common::fixture("VEC-MSG-TICKET", "ticket_field_msgpack_hex")
    );
}

/// A ticket stamp is `truncated_hash(ticket || message_id)`, 16 bytes.
///
/// Reference: the sender writes it at LXMessage.py:299-302 and the receiver
/// accepts on exactly that recomputation at :272-276, scoring the message at
/// `COST_TICKET` and bypassing proof-of-work entirely. The byte order is the
/// whole contract: `ticket + message_id`, not the reverse, and the truncated
/// (16-byte) hash, not the full one. A transposed or full-length stamp is
/// rejected by every peer as an ordinary stamp that fails its cost.
///
/// Pinned against the stamp the vendored reference produced for the vector
/// message, and against an independent recomputation of the concatenation.
#[test]
fn ticket_stamp_is_the_reference_truncated_hash() {
    let ticket: [u8; 16] = hex_fixture("VEC-MSG-TICKET", "outbound_ticket_hex")
        .try_into()
        .unwrap();
    let message_id: [u8; 32] = hex_fixture("VEC-MSG-TICKET", "message_id_hex")
        .try_into()
        .unwrap();

    let stamp = ticket_stamp(&ticket, &message_id);
    assert_eq!(
        hex::encode(stamp),
        common::fixture("VEC-MSG-TICKET", "stamp_hex")
    );
    assert_eq!(stamp.len(), 16, "TICKET stamps are the truncated hash");

    // Independent recomposition of RNS.Identity.truncated_hash(ticket+id).
    let mut material = ticket.to_vec();
    material.extend_from_slice(&message_id);
    assert_eq!(stamp, full_hash(&material)[..16]);

    // The pin bites on a transposed concatenation.
    let mut transposed = message_id.to_vec();
    transposed.extend_from_slice(&ticket);
    assert_ne!(stamp, full_hash(&transposed)[..16]);
}

/// Only an unexpired ticket we issued validates an inbound ticket stamp.
///
/// Reference: `validate_stamp` iterates `get_inbound_tickets(source)`
/// (LXMessage.py:272-280; LXMRouter.py:1123-1134), which returns only tickets
/// with `now < expiry`. Expired ticket material stays on disk for
/// `TICKET_GRACE` (LXMRouter.py:1313-1323) but is not offered for validation —
/// the grace period exists so a ticket in flight is not lost, not so an expired
/// one keeps working.
#[test]
fn inbound_ticket_stamp_validation_follows_the_reference_expiry_boundary() {
    let source = [3; 16];
    let message_id = [7; 32];
    let mut store = TicketStore::default();
    let issued = store
        .issue(source, 1_000.0, &mut OsRng)
        .expect("issue a ticket");
    let stamp = ticket_stamp(&issued.secret, &message_id);

    assert!(store.validates_inbound_stamp(&source, &message_id, &stamp, 1_000.0));
    assert!(!store.validates_inbound_stamp(&source, &message_id, &stamp, issued.expires_unix,));

    // Still owned during the grace window, still not validating.
    store.clean(issued.expires_unix + 1.0);
    assert!(store.contains_inbound(&source, &issued));
    assert!(!store.validates_inbound_stamp(
        &source,
        &message_id,
        &stamp,
        issued.expires_unix + 1.0,
    ));
}

/// Big-endian 256-bit comparison against `2^(256 - cost)`, built from u128
/// halves so it shares no construction with the production byte-array target.
#[cfg(feature = "pow")]
fn reference_stamp_valid(digest: &[u8; 32], cost: u8) -> bool {
    // LXStamper.py:73-77: `target = 0b1 << 256-target_cost`, valid iff
    // `int.from_bytes(result, "big") <= target`. At cost 0 the target is
    // 2^256, which exceeds every 256-bit digest.
    if cost == 0 {
        return true;
    }
    let high = u128::from_be_bytes(digest[..16].try_into().unwrap());
    let low = u128::from_be_bytes(digest[16..].try_into().unwrap());
    let shift = 256 - u32::from(cost);
    let (target_high, target_low) = if shift >= 128 {
        (1u128 << (shift - 128), 0u128)
    } else {
        (0u128, 1u128 << shift)
    };
    (high, low) <= (target_high, target_low)
}

/// A stamp is accepted on `full_hash(workblock || stamp) <= 2^(256 - cost)`,
/// and the declared cost is what a peer mines against.
///
/// Reference: `stamp_valid` (LXStamper.py:73-77) and `stamp_value`
/// (:62-70). The receiver decides acceptance on the first and scores the
/// message with the second (LXMessage.py:270-291); the propagation node
/// applies the same rule to the outer stamp at a floor of
/// `max(0, propagation_stamp_cost - flexibility)` (LXMRouter.py:2242-2243) and
/// tears the link down plus throttles the sender when any entry fails
/// (:2447-2454). Getting the boundary wrong by one bit therefore does not
/// degrade gracefully — it costs the peering.
///
/// Checked here at both representable ends: cost 0, where the reference target
/// is 2^256 and every stamp is valid, and cost 255, the largest a `u8` can
/// declare. At cost 0 our generator deliberately skips the 768 KB workblock
/// and returns 32 random bytes; the reference reaches the same outcome by
/// accepting its first random draw (LXStamper.py:192-199), so the emitted
/// value is identically distributed and identically accepted.
#[cfg(feature = "pow")]
#[test]
fn stamp_validity_boundary_matches_the_reference_target() {
    use leviculum_lxmf::stamp::{valid, value};

    let vector_digest: [u8; 32] = hex_fixture("VEC-STAMP-1", "digest_hex").try_into().unwrap();
    let vector_cost: u8 = common::fixture("VEC-STAMP-1", "target_cost")
        .parse()
        .unwrap();

    // The reference's own digest satisfies the independently rebuilt rule at
    // the cost it was mined for, and fails one bit above it.
    assert!(reference_stamp_valid(&vector_digest, vector_cost));
    assert!(!reference_stamp_valid(&vector_digest, vector_cost + 1));

    // Drive `valid()` through the public path — workblock plus stamp — and
    // compare against the rebuilt target on the digest we compute ourselves.
    // The search walks stamps until it has crossed the boundary in both
    // directions for every small cost, so no cost is only ever seen accepting
    // or only ever rejecting.
    let workblock = b"generated-field-pin workblock";
    for cost in [0u8, 1, 2, 3, 4, 8, 9, 64, 128, 200, 255] {
        let (mut seen_valid, mut seen_invalid) = (false, false);
        for counter in 0u32..4_096 {
            let mut stamp = [0u8; 32];
            stamp[..4].copy_from_slice(&counter.to_be_bytes());
            let mut material = workblock.to_vec();
            material.extend_from_slice(&stamp);
            let digest = full_hash(&material);

            let expected = reference_stamp_valid(&digest, cost);
            assert_eq!(
                valid(workblock, &stamp, cost),
                expected,
                "cost {cost}, counter {counter}"
            );
            if expected {
                seen_valid = true;
            } else {
                seen_invalid = true;
            }
            if seen_valid && seen_invalid {
                break;
            }
        }
        // Costs above a handful of bits are unreachable by a 4096-stamp walk;
        // for those the accepting state is proven by VEC-STAMP-1 above rather
        // than by this loop, and only the rejecting state is exercised here.
        assert!(seen_invalid || cost == 0, "cost {cost} never rejected");
        assert!(seen_valid || cost > 4, "cost {cost} never accepted");
    }

    // Cost 0 accepts anything, which is why our generator may skip the
    // 768 KB workblock and return 32 random bytes.
    assert!(valid(b"any workblock", &[0xff; 32], 0));
    assert!(reference_stamp_valid(&full_hash(b"anything"), 0));

    // `value` counts leading zero bits of the same digest (LXStamper.py:62-70),
    // recomposed here without the production fold.
    let stamp = [0x5au8; 32];
    let mut material = workblock.to_vec();
    material.extend_from_slice(&stamp);
    let digest = full_hash(&material);
    let mut expected = 0u16;
    for byte in digest {
        expected += byte.leading_zeros() as u16;
        if byte != 0 {
            break;
        }
    }
    assert_eq!(value(workblock, &stamp), expected);
}

/// The propagation transient ID hashes the UNSTAMPED bytes.
///
/// Reference (LXMessage.py:430-435): `lxmf_data = packed[:16] +
/// destination.encrypt(packed[16:])`, `self.transient_id =
/// full_hash(lxmf_data)`, and only afterwards is the propagation stamp
/// appended. The propagation node recomputes the same way when it validates
/// (`validate_pn_stamp`, LXStamper.py:83-93: it strips the trailing
/// `STAMP_SIZE` bytes and hashes the remainder), and that hash is the key of
/// its message store, the ID it offers to peers, and the ID a client sends
/// back to purge the message (LXMRouter.py:1508-1519).
///
/// Hashing the stamped bytes instead would produce an ID that matches nothing
/// on the node: the upload would be stored under the node's own recomputation,
/// and our later purge request would name an ID the node never had. The stamp
/// itself is also the proof-of-work material, so the two must agree.
#[test]
fn propagation_transient_id_covers_the_unstamped_message_only() {
    let unstamped: Vec<u8> = (0u8..=200).collect();
    let stamp = [0xa5u8; 32];
    let upload = PropagationUpload::single(1_700_000_000.0, unstamped.clone(), stamp);

    assert_eq!(
        *upload.transient_id(),
        full_hash(&unstamped),
        "transient_id = full_hash(lxmf_data) before the stamp is appended"
    );

    // Recompose the node's own view: strip STAMP_SIZE from the encoded entry.
    let encoded = upload.encode();
    let entry_start = encoded.len() - (unstamped.len() + 32);
    let entry = &encoded[entry_start..];
    assert_eq!(&entry[entry.len() - 32..], &stamp);
    assert_eq!(
        full_hash(&entry[..entry.len() - 32]),
        *upload.transient_id()
    );

    // The pin bites: the stamped bytes hash to something else entirely.
    assert_ne!(full_hash(entry), *upload.transient_id());
}

/// The propagation upload envelope is `[timestamp, [entry, ...]]`, and the
/// timestamp is a field the reference reads and then discards.
///
/// Reference: the client packs `msgpack.packb([time.time(), [lxmf_data]])`
/// (LXMessage.py:436). Both node-side ingest paths bind `remote_timebase =
/// data[0]` and never act on it: `propagation_packet` (LXMRouter.py:2238-2240)
/// drops it, and `propagation_resource_concluded` (:2344) overwrites it with
/// the announce's `pn_config[1]` before the only use at :2375. The structural
/// guard at :2341 is `type(data[0] == float)`, which evaluates a comparison
/// and takes `type()` of the resulting bool — always truthy, so no type check
/// reaches the value either.
///
/// Verdict: no peer decision depends on this value, so our sending the
/// preparation time rather than the transmission time is a deviation with no
/// observable effect. Pinned so a later change that starts deriving meaning
/// from it has to confront the reference behaviour first.
#[test]
fn propagation_upload_envelope_matches_the_reference_shape() {
    let unstamped = vec![0x11u8; 120];
    let upload = PropagationUpload::single(1_700_000_000.0, unstamped.clone(), [0x22; 32]);
    let encoded = upload.encode();

    assert_eq!(encoded[0], 0x92, "two-element outer array");
    assert_eq!(encoded[1], 0xcb, "timestamp is msgpack float64");
    assert_eq!(
        f64::from_be_bytes(encoded[2..10].try_into().unwrap()),
        1_700_000_000.0
    );
    assert_eq!(encoded[10], 0x91, "one-element message list");
    assert_eq!(encoded[11], 0xc4, "each entry is a msgpack bin");
    assert_eq!(
        encoded[12] as usize,
        unstamped.len() + 32,
        "the entry length covers message and stamp"
    );
}

/// The advertised stamp cost is emitted only inside the reference's
/// `0 < cost < 255` window; 0 and 255 are sent as "no cost" (Codeberg #181).
///
/// Reference: `get_announce_app_data` (LXMRouter.py:1042-1045) builds
/// `stamp_cost = None` and overwrites it only `if delivery_destination
/// .stamp_cost > 0 and delivery_destination.stamp_cost < 255`. The same window
/// sits one layer earlier in `set_inbound_stamp_cost` (LXMRouter.py:378-393),
/// which stores `None` for `< 1` and returns `True`, but *refuses* `>= 255` and
/// returns `False` without touching the stored value.
///
/// What a peer DECIDES: `LXMFDeliveryAnnounceHandler.received_announce`
/// (Handlers.py:17-18) reads the field with `stamp_cost_from_app_data` and
/// hands it to `update_stamp_cost` (LXMRouter.py:1027-1029), which stores it
/// with no bound of its own. It is later passed straight into
/// `LXStamper.generate_stamp` (LXMessage.py:320), whose search loop
/// (LXStamper.py:199) runs until `stamp_valid` holds. At cost 255 the target is
/// `1 << 1`, so the loop needs a 256-bit digest of 0, 1 or 2 and never
/// terminates. One announce from us therefore wedges the outbound queue of
/// every Python peer holding a message for our destination, with nothing in
/// their logs naming us. That is why the reference declines to send the value
/// rather than trusting the reader to bound it.
///
/// Expected bytes come from the reference's own emitter and its own decoder,
/// recorded in `VEC-ANN-STAMP-COST-WINDOW` by `gen_vectors.py` — the vector
/// calls `LXMRouter.get_announce_app_data` and `stamp_cost_from_app_data`
/// directly, so nothing about the window is rebuilt in Rust.
///
/// What this test cannot catch:
/// - Byte equality at six points does not prove the guard is a range test.
///   A lookup table over exactly `{None, 0, 1, 8, 254, 255}` would pass, and
///   so would `<= 254` written as `< 255` — the two are the same predicate on
///   `u8`, which is precisely why the boundary is pinned at 254 and 255 rather
///   than described.
/// - It says nothing about what we do with an inbound 255; that is the read
///   side, pinned separately in `router.rs`
///   (`hostile_announced_stamp_cost_is_not_mined`).
/// - As in the stamp-target pin above, a `<=`/`<` flip inside the digest
///   comparison is invisible here: this field never reaches that comparison.
#[test]
fn announced_stamp_cost_stays_inside_the_reference_window() {
    use leviculum_lxmf::announce::{DeliveryAnnounce, StampCostRefused};

    let display_name = hex_fixture("VEC-ANN-STAMP-COST-WINDOW", "display_name_hex");

    for (cost, key) in [
        (None, "none"),
        (Some(0), "0"),
        (Some(1), "1"),
        (Some(8), "8"),
        (Some(254), "254"),
        (Some(255), "255"),
    ] {
        let announce = DeliveryAnnounce {
            display_name: Some(display_name.clone()),
            stamp_cost: cost,
            compression_supported: true,
        };
        assert_eq!(
            hex::encode(announce.encode()),
            common::fixture("VEC-ANN-STAMP-COST-WINDOW", &format!("emit_{key}_hex")),
            "cost {cost:?} must produce the reference's own announce bytes"
        );

        // What the reference's decoder reads back from exactly those bytes.
        let peer_reads = common::fixture("VEC-ANN-STAMP-COST-WINDOW", &format!("peer_reads_{key}"));
        assert_eq!(
            peer_reads,
            match cost {
                Some(cost) if cost > 0 && cost < 255 => cost.to_string(),
                _ => "null".to_string(),
            },
            "cost {cost:?}: the reference decoder's view of our bytes"
        );

        // The checked constructor mirrors `set_inbound_stamp_cost`: it maps
        // 0 to "no cost" and accepts, and refuses 255 outright.
        let accepted = common::fixture(
            "VEC-ANN-STAMP-COST-WINDOW",
            &format!("setter_accepts_{key}"),
        ) == "true";
        let constructed = DeliveryAnnounce::new(Some(display_name.clone()), cost);
        assert_eq!(
            constructed.is_ok(),
            accepted,
            "cost {cost:?}: constructor acceptance must match set_inbound_stamp_cost"
        );
        if let Ok(constructed) = constructed {
            let stored =
                common::fixture("VEC-ANN-STAMP-COST-WINDOW", &format!("setter_stores_{key}"));
            assert_eq!(
                constructed
                    .stamp_cost
                    .map_or_else(|| "null".to_string(), |cost| cost.to_string()),
                stored,
                "cost {cost:?}: constructor must store what the setter stores"
            );
        } else {
            assert_eq!(constructed.unwrap_err(), StampCostRefused(255));
        }
    }

    // The pin bites: emitting the caller's byte verbatim is what we did before
    // #181, and it is not what the reference emits. Recomposed here as
    // fixarray(3) || bin8 "Alice" || uint8 255 || fixarray(1) || 0.
    let mut unclamped = vec![0x93, 0xc4, display_name.len() as u8];
    unclamped.extend_from_slice(&display_name);
    unclamped.extend_from_slice(&[0xcc, 0xff, 0x91, 0x00]);
    assert_ne!(
        hex::encode(&unclamped),
        common::fixture("VEC-ANN-STAMP-COST-WINDOW", "emit_255_hex"),
        "the naive encoding must differ from the reference's, or the pin is vacuous"
    );
    assert_eq!(
        hex::encode(&unclamped),
        // Same recomposition against cost 254, which the reference *does* send,
        // proves the byte layout above is right and only the value differs.
        common::fixture("VEC-ANN-STAMP-COST-WINDOW", "emit_254_hex").replace("ccfe", "ccff"),
    );
}
