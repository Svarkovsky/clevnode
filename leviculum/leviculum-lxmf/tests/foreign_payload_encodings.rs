//! What we accept from a writer whose MessagePack encoder is not Python's.
//!
//! LXMF itself always writes `time.time()`, so every payload a Python peer
//! produces carries a float64 timestamp and a `bin`-typed title and content.
//! The exposure is a third-party writer — reticulum-kt, microReticulum, a
//! hand-rolled encoder — that packs the same *values* with different
//! MessagePack *types*. The reference reads
//! `timestamp = unpacked_payload[0]` (LXMessage.py:766) with no type check at
//! all, and hashes the payload bytes it received verbatim when the payload has
//! no stamp (`packed_payload`, :753-762), so those messages deliver on Python.
//!
//! Codeberg #183. Every acceptance and every rejection here was measured
//! against the pinned reference first; the accepted cases are backed by
//! generated vectors in `docs/src/appendix/lxmf/vectors/vectors.json`
//! (`VEC-MSG-FOREIGN-*`), which record the reference's own verdict.

mod common;

use leviculum_core::{crypto::full_hash, Identity};
use leviculum_lxmf::{DeliveryMethod, Message, MessageError, Verification};

fn source_identity() -> Identity {
    let source_private = hex::decode(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
         202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
    )
    .unwrap();
    Identity::from_private_key_bytes(&source_private).unwrap()
}

const DESTINATION: [u8; 16] = [0xd0; 16];
const SOURCE: [u8; 16] = [0x50; 16];

/// Compose the bytes a foreign writer would put on the wire.
///
/// `timestamp_bytes` is one complete MessagePack value spliced in as
/// `payload[0]`; the remaining elements use the canonical `bin` forms. The
/// signature is taken over exactly the bytes produced here, which is what any
/// writer does — it signs what it packed, not what a re-encoder would produce.
fn foreign_message(timestamp_bytes: &[u8], stamp: Option<&[u8]>) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(if stamp.is_some() { 0x95 } else { 0x94 });
    payload.extend_from_slice(timestamp_bytes);
    payload.extend_from_slice(&[0xc4, 0x02, b'H', b'i']); // bin8 "Hi"
    payload.extend_from_slice(&[0xc4, 0x05, b'H', b'e', b'l', b'l', b'o']); // bin8 "Hello"
    payload.push(0x80); // fixmap(0)
    if let Some(stamp) = stamp {
        payload.push(0xc4);
        payload.push(stamp.len() as u8);
        payload.extend_from_slice(stamp);
    }

    // The reference hashes `packed_payload` — the received bytes — whenever the
    // payload has no stamp, and `msgpack.packb(unpacked_payload[:4])` when it
    // has one (LXMessage.py:753-762). A writer signs the former in both cases.
    let signed_payload = if stamp.is_some() {
        let mut without_stamp = payload.clone();
        without_stamp.truncate(payload.len() - (stamp.map_or(0, |s| s.len() + 2)));
        without_stamp[0] = 0x94;
        without_stamp
    } else {
        payload.clone()
    };

    let mut hashed = Vec::new();
    hashed.extend_from_slice(&DESTINATION);
    hashed.extend_from_slice(&SOURCE);
    hashed.extend_from_slice(&signed_payload);
    let message_id = full_hash(&hashed);
    let mut signed = hashed;
    signed.extend_from_slice(&message_id);
    let signature = source_identity().sign(&signed).unwrap();

    let mut out = Vec::new();
    out.extend_from_slice(&DESTINATION);
    out.extend_from_slice(&SOURCE);
    out.extend_from_slice(&signature);
    out.extend_from_slice(&payload);
    out
}

fn unpack(bytes: &[u8]) -> Result<Message, MessageError> {
    Message::unpack(
        bytes,
        None,
        Some(&source_identity()),
        DeliveryMethod::Direct,
    )
}

/// Every MessagePack numeric type the reference accepts as `payload[0]`
/// decodes here, and the signature over the writer's own bytes still verifies.
///
/// Reference verdicts, measured on the pinned snapshot with
/// `LXMessage.unpack_from_bytes`: all of these return a message with
/// `signature_validated == True` and a `message_id` equal to the writer's.
/// Before Codeberg #183 we returned `Err(InvalidFormat)` for every case except
/// float64, because `decode_payload` called `msgpack::read_f64`, which demands
/// the `0xcb` marker.
#[test]
fn foreign_numeric_timestamp_encodings_are_accepted() {
    let cases: &[(&str, &[u8], f64)] = &[
        (
            "float64",
            &[0xcb, 0x41, 0xd9, 0x54, 0xfc, 0x40, 0x00, 0x00, 0x00],
            1_700_000_000.0,
        ),
        ("float32", &[0xca, 0x3f, 0xc0, 0x00, 0x00], 1.5),
        ("positive fixint", &[0x2a], 42.0),
        ("negative fixint", &[0xff], -1.0),
        ("uint8", &[0xcc, 0x80], 128.0),
        ("uint16", &[0xcd, 0x12, 0x34], 4660.0),
        ("uint32", &[0xce, 0x65, 0x53, 0xf1, 0x00], 1_700_000_000.0),
        (
            "uint64",
            &[0xcf, 0, 0, 0, 0, 0x65, 0x53, 0xf1, 0x00],
            1_700_000_000.0,
        ),
        ("int8", &[0xd0, 0x80], -128.0),
        ("int16", &[0xd1, 0xff, 0x00], -256.0),
        ("int32", &[0xd2, 0xff, 0xff, 0xff, 0x00], -256.0),
        (
            "int64",
            &[0xd3, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00],
            -256.0,
        ),
    ];

    for (name, timestamp_bytes, expected) in cases {
        let message = unpack(&foreign_message(timestamp_bytes, None))
            .unwrap_or_else(|error| panic!("{name} timestamp rejected: {error:?}"));
        assert_eq!(
            message.timestamp, *expected,
            "{name} decoded to a wrong value"
        );
        assert_eq!(
            message.verification,
            Verification::Valid,
            "{name}: the signature covers the writer's own payload bytes"
        );
    }
}

/// A payload with no stamp is hashed exactly as it arrived.
///
/// Reference: `unpack_from_bytes` keeps `packed_payload` — the slice taken
/// from the wire — and hashes it (`hashed_part = destination_hash +
/// source_hash + packed_payload`, LXMessage.py:753,762). Only the stamped
/// branch re-packs (`packed_payload = msgpack.packb(unpacked_payload[:4])`,
/// :759). Re-encoding canonically on the unstamped branch, as we did before
/// #183, changes the hashed bytes for any writer whose encoder differs from
/// ours, which fails the signature and changes the message ID — the message is
/// dropped, and the ID we would report to an application is not the ID the
/// sender used.
#[test]
fn unstamped_payload_is_hashed_as_received() {
    // uint32 is a form Python's own packer would produce for this value, so the
    // divergence is not exotic: it is what any encoder that keeps integers
    // integral emits.
    let wire = foreign_message(&[0xce, 0x65, 0x53, 0xf1, 0x00], None);
    let message = unpack(&wire).expect("an integer timestamp is accepted");

    let mut expected = Vec::new();
    expected.extend_from_slice(&DESTINATION);
    expected.extend_from_slice(&SOURCE);
    expected.extend_from_slice(&wire[96..]);
    assert_eq!(
        message.message_id,
        full_hash(&expected),
        "the message ID must hash the payload bytes as received"
    );
    assert_eq!(message.verification, Verification::Valid);
    assert_eq!(
        message.pack(),
        wire,
        "packing a decoded message must reproduce the bytes its signature covers"
    );
}

/// A stamped payload is re-packed, and the re-pack keeps the numeric family.
///
/// Reference: the stamped branch discards the received bytes and hashes
/// `msgpack.packb(unpacked_payload[:4])` (LXMessage.py:757-759). Python's
/// packer writes an int back as a minimal-width int and a float back as
/// float64, so an integer timestamp survives the round trip and validates —
/// measured `signature_validated == True` for a uint32 timestamp with a
/// 16-byte stamp. Encoding the decoded value as float64 regardless, as we did,
/// makes the same message fail verification on our side only.
#[test]
fn stamped_payload_rehash_preserves_the_numeric_family() {
    let stamp = [0u8; 16];
    let message = unpack(&foreign_message(
        &[0xce, 0x65, 0x53, 0xf1, 0x00],
        Some(&stamp),
    ))
    .expect("an integer timestamp is accepted with a stamp");
    assert_eq!(message.timestamp, 1_700_000_000.0);
    assert_eq!(message.stamp.as_deref(), Some(&stamp[..]));
    assert_eq!(
        message.verification,
        Verification::Valid,
        "the re-packed payload must reproduce the writer's minimal-width integer"
    );
}

/// The reference's re-pack is itself lossy, and we reproduce its verdict.
///
/// A writer that spells 1 700 000 000 as a uint64 signs those nine bytes;
/// Python re-packs the decoded int minimally as uint32 and the signature fails
/// (measured: `signature_validated == False`). Mirroring the reference here
/// rather than reusing the received bytes keeps our accept/reject set equal to
/// Python's, which is what a sender debugging against either stack needs.
#[test]
fn stamped_payload_rejects_what_the_reference_rejects() {
    let stamp = [0u8; 16];
    let message = unpack(&foreign_message(
        &[0xcf, 0, 0, 0, 0, 0x65, 0x53, 0xf1, 0x00],
        Some(&stamp),
    ))
    .expect("the payload still decodes");
    assert_eq!(
        message.verification,
        Verification::Invalid,
        "a non-minimal integer does not survive the reference's re-pack either"
    );
}

/// A `payload[0]` that is not a number is refused by name.
///
/// The reference performs no check, so `nil`, `True` and `"1700000000"` all
/// reach a Python application as the message timestamp — measured, all three
/// unpack with a valid signature. They cannot reach ours: `Message.timestamp`
/// is an `f64`. The rejection is named rather than folded into
/// `InvalidFormat` so the drop is attributable in a log.
///
/// This cannot cost a legitimate peer a message. Nothing downstream of the
/// reference can use a non-numeric timestamp: every consumer either formats it
/// through `time` or compares it against `time.time()`, and both raise
/// `TypeError` on `nil`, a bool or a string. A writer emitting one has already
/// produced a message no Python client can display or order.
#[test]
fn non_numeric_timestamps_are_refused_by_name() {
    for (name, bytes) in [
        ("nil", &[0xc0][..]),
        ("false", &[0xc2][..]),
        ("true", &[0xc3][..]),
        ("fixstr", &[0xa2, b'4', b'2'][..]),
        ("bin", &[0xc4, 0x01, 0x2a][..]),
        ("fixarray", &[0x91, 0x2a][..]),
        ("fixmap", &[0x81, 0x2a, 0x2a][..]),
    ] {
        assert_eq!(
            unpack(&foreign_message(bytes, None)).unwrap_err(),
            MessageError::InvalidTimestamp,
            "{name} must be refused as a timestamp, not as a generic format error"
        );
    }
}

/// A uint64 above `i64::MAX` is refused by name.
///
/// Python carries it as an arbitrary-precision int (measured:
/// `18446744073709551615`). We refuse rather than widen: the smallest value in
/// that range is 9.2e18 unix seconds, about 292 billion years, so no writer
/// that means a time can land there. Refusing keeps the decoded value exact
/// for every value we do accept, instead of silently substituting the nearest
/// double.
#[test]
fn timestamps_above_the_signed_range_are_refused_by_name() {
    assert_eq!(
        unpack(&foreign_message(
            &[0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            None
        ))
        .unwrap_err(),
        MessageError::InvalidTimestamp
    );
}

/// Values that are numeric but implausible are accepted, deliberately.
///
/// Negative timestamps (a pre-1970 clock, or a peer whose clock has not been
/// set), integers beyond 2^53 where the conversion to `f64` rounds, and the
/// non-finite float64s all pass. The reference applies no plausibility window
/// anywhere on the read path, and a receiver that invents one drops messages
/// the sender and every Python peer consider fine. Compare the write side,
/// where Codeberg #184 decided the opposite for non-finite values: refusing to
/// *emit* a value costs no peer anything, refusing to *accept* one costs the
/// sender its message.
#[test]
fn implausible_but_numeric_timestamps_are_accepted() {
    let cases: &[(&str, &[u8])] = &[
        ("negative fixint", &[0xff]),
        (
            "int64 far before the epoch",
            &[0xd3, 0x80, 0, 0, 0, 0, 0, 0, 0],
        ),
        ("uint64 past 2^53", &[0xcf, 0x00, 0x20, 0, 0, 0, 0, 0, 0x01]),
        ("float64 NaN", &[0xcb, 0x7f, 0xf8, 0, 0, 0, 0, 0, 0]),
        ("float64 +Inf", &[0xcb, 0x7f, 0xf0, 0, 0, 0, 0, 0, 0]),
        ("float64 -Inf", &[0xcb, 0xff, 0xf0, 0, 0, 0, 0, 0, 0]),
    ];
    for (name, bytes) in cases {
        let message = unpack(&foreign_message(bytes, None))
            .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(
            message.verification,
            Verification::Valid,
            "{name} must still verify against the writer's own bytes"
        );
    }
}

/// The generated vectors carry the reference's own verdict on these bytes.
#[test]
fn reference_vectors_agree_with_our_reader() {
    for id in [
        "VEC-MSG-FOREIGN-UINT32",
        "VEC-MSG-FOREIGN-FLOAT32",
        "VEC-MSG-FOREIGN-NEGATIVE-FIXINT",
    ] {
        let packed = hex::decode(common::fixture(id, "packed_hex")).unwrap();
        assert_eq!(
            common::fixture(id, "reference_signature_validated"),
            "true",
            "{id}: the reference must accept the vector it generated"
        );
        let message = Message::unpack(
            &packed,
            None,
            Some(&source_identity()),
            DeliveryMethod::Direct,
        )
        .unwrap_or_else(|error| panic!("{id} rejected: {error:?}"));
        assert_eq!(message.verification, Verification::Valid, "{id}");
        assert_eq!(
            hex::encode(message.message_id),
            common::fixture(id, "message_id_hex"),
            "{id}: our message ID must equal the reference's"
        );
    }
}
