mod common;

use leviculum_core::Identity;
use leviculum_lxmf::{DeliveryMethod, Message, Verification};

fn source_identity() -> Identity {
    let source_private = hex::decode(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
         202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
    )
    .unwrap();
    Identity::from_private_key_bytes(&source_private).unwrap()
}

#[test]
fn python_minimal_message_vector() {
    let source = source_identity();
    let packed = hex::decode(common::fixture("VEC-MSG-1", "packed_hex")).unwrap();
    let destination_hash = packed[..16].try_into().unwrap();
    let source_hash = packed[16..32].try_into().unwrap();
    let message = Message::create(
        destination_hash,
        source_hash,
        &source,
        1_700_000_000.0,
        b"Hi".to_vec(),
        b"Hello".to_vec(),
        Vec::new(),
        DeliveryMethod::Opportunistic,
    )
    .unwrap();

    assert_eq!(message.pack(), packed);
    assert_eq!(
        Message::unpack(
            &message.on_air().unwrap(),
            Some(destination_hash),
            Some(&source),
            DeliveryMethod::Opportunistic,
        )
        .unwrap()
        .verification,
        Verification::Valid
    );
}

#[test]
fn python_negative_fixint_field_key_vector() {
    let source = source_identity();
    let packed = hex::decode(common::fixture("VEC-MSG-NEGATIVE-FIELD", "packed_hex")).unwrap();

    let message = Message::unpack(&packed, None, Some(&source), DeliveryMethod::Direct).unwrap();

    assert_eq!(message.verification, Verification::Valid);
    assert_eq!(message.fields.len(), 1);
    assert_eq!(message.fields[0].0, -1);
    assert_eq!(message.pack(), packed);
    assert_eq!(
        hex::encode(&packed[96..]),
        common::fixture("VEC-MSG-NEGATIVE-FIELD", "payload_msgpack_hex")
    );
}
