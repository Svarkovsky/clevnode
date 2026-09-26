//! Codeberg #371: answering a Sideband telemetry request starts with
//! reading one, and reading one starts with refusing everyone who is not
//! the configured target.
//!
//! The wire shape under test is the documented ASSUMPTION at
//! `COMMAND_TELEMETRY_REQUEST` (Sideband's source is not in `reference/`):
//! `FIELD_COMMANDS` (`reference/LXMF/LXMF/LXMF.py:16`) carries a list of
//! command maps and a request is `[{0x01: <timebase>}]`. Once a real
//! Columba/Sideband request is captured on the bench, its bytes belong in
//! a fixture here.

use leviculum_core::Identity;
use leviculum_lxmf::constants::FIELD_COMMANDS;
use leviculum_lxmf::telemetry::{
    screen_telemetry_request, telemetry_request_timebase, TelemetryRequestVerdict,
    COMMAND_TELEMETRY_REQUEST,
};
use leviculum_lxmf::{msgpack, DeliveryMethod, Message};

const DELIVERY_HASH: [u8; 16] = [0x11; 16];
const TARGET_HASH: [u8; 16] = [0x22; 16];
const FOREIGN_HASH: [u8; 16] = [0x33; 16];

fn identity(seed: u8) -> Identity {
    let bytes: Vec<u8> = (0..64).map(|i| i ^ seed).collect();
    Identity::from_private_key_bytes(&bytes).unwrap()
}

/// `[{0x01: <timebase>}]` — the assumed Sideband request value.
fn request_value(timebase: i64) -> Vec<u8> {
    let mut v = Vec::new();
    msgpack::array(&mut v, 1);
    msgpack::map(&mut v, 1);
    msgpack::int(&mut v, COMMAND_TELEMETRY_REQUEST);
    msgpack::int(&mut v, timebase);
    v
}

fn request_on_air(source_hash: [u8; 16], signer: &Identity, value: Vec<u8>) -> Vec<u8> {
    Message::create(
        DELIVERY_HASH,
        source_hash,
        signer,
        1_700_000_000.0,
        Vec::new(),
        Vec::new(),
        vec![(FIELD_COMMANDS, value)],
        DeliveryMethod::Opportunistic,
    )
    .unwrap()
    .on_air()
    .unwrap()
}

#[test]
fn a_request_from_the_target_is_verified_and_carries_its_timebase() {
    let target = identity(0);
    let on_air = request_on_air(TARGET_HASH, &target, request_value(1_699_999_000));
    assert_eq!(
        screen_telemetry_request(&on_air, DELIVERY_HASH, Some(TARGET_HASH), Some(&target)),
        TelemetryRequestVerdict::Request {
            source: TARGET_HASH,
            timebase: 1_699_999_000
        }
    );
}

#[test]
fn a_request_from_a_foreign_sender_is_not_allowed() {
    let foreign = identity(1);
    let target = identity(0);
    let on_air = request_on_air(FOREIGN_HASH, &foreign, request_value(0));
    assert_eq!(
        screen_telemetry_request(&on_air, DELIVERY_HASH, Some(TARGET_HASH), Some(&target)),
        TelemetryRequestVerdict::NotAllowed {
            source: FOREIGN_HASH
        }
    );
}

#[test]
fn with_no_target_configured_nobody_is_allowed() {
    let target = identity(0);
    let on_air = request_on_air(TARGET_HASH, &target, request_value(0));
    assert_eq!(
        screen_telemetry_request(&on_air, DELIVERY_HASH, None, None),
        TelemetryRequestVerdict::NotAllowed {
            source: TARGET_HASH
        }
    );
}

/// A forged claim: the message names the target as its source but is
/// signed by somebody else's key.
#[test]
fn a_request_with_a_forged_signature_is_rejected() {
    let target = identity(0);
    let attacker = identity(1);
    let on_air = request_on_air(TARGET_HASH, &attacker, request_value(0));
    assert_eq!(
        screen_telemetry_request(&on_air, DELIVERY_HASH, Some(TARGET_HASH), Some(&target)),
        TelemetryRequestVerdict::BadSignature {
            source: TARGET_HASH
        }
    );
}

/// The caller holds no key for the target yet (awaiting-key): the request
/// is attributable but not verifiable, and the verdict says exactly that.
#[test]
fn a_request_without_the_targets_key_is_unverifiable() {
    let target = identity(0);
    let on_air = request_on_air(TARGET_HASH, &target, request_value(0));
    assert_eq!(
        screen_telemetry_request(&on_air, DELIVERY_HASH, Some(TARGET_HASH), None),
        TelemetryRequestVerdict::Unverifiable {
            source: TARGET_HASH
        }
    );
}

#[test]
fn a_message_without_commands_is_not_a_request() {
    let target = identity(0);
    let message = Message::create(
        DELIVERY_HASH,
        TARGET_HASH,
        &target,
        1_700_000_000.0,
        Vec::new(),
        b"hello".to_vec(),
        Vec::new(),
        DeliveryMethod::Opportunistic,
    )
    .unwrap();
    assert_eq!(
        screen_telemetry_request(
            &message.on_air().unwrap(),
            DELIVERY_HASH,
            Some(TARGET_HASH),
            Some(&target)
        ),
        TelemetryRequestVerdict::NotARequest
    );
}

#[test]
fn bytes_that_are_no_lxmf_message_are_not_a_request() {
    assert_eq!(
        screen_telemetry_request(b"random noise", DELIVERY_HASH, Some(TARGET_HASH), None),
        TelemetryRequestVerdict::NotARequest
    );
}

#[test]
fn an_unknown_command_id_is_not_a_request() {
    let mut v = Vec::new();
    msgpack::array(&mut v, 1);
    msgpack::map(&mut v, 1);
    msgpack::int(&mut v, 0x7F);
    msgpack::int(&mut v, 42);
    assert_eq!(telemetry_request_timebase(&[(FIELD_COMMANDS, v)]), None);
}

/// Sideband variants pack the timebase as the first element of a list
/// (with a collector flag behind it); the trailing elements are ignored.
#[test]
fn a_list_valued_request_reads_the_first_element_as_timebase() {
    let mut v = Vec::new();
    msgpack::array(&mut v, 1);
    msgpack::map(&mut v, 1);
    msgpack::int(&mut v, COMMAND_TELEMETRY_REQUEST);
    msgpack::array(&mut v, 2);
    msgpack::int(&mut v, 12_345);
    msgpack::bool(&mut v, true);
    assert_eq!(
        telemetry_request_timebase(&[(FIELD_COMMANDS, v)]),
        Some(12_345)
    );
}

/// Python's `time.time()` is a float; the timebase survives as whole
/// seconds.
#[test]
fn a_float_timebase_is_read_as_whole_seconds() {
    let mut v = Vec::new();
    msgpack::array(&mut v, 1);
    msgpack::map(&mut v, 1);
    msgpack::int(&mut v, COMMAND_TELEMETRY_REQUEST);
    msgpack::f64(&mut v, 1_699_999_000.75);
    assert_eq!(
        telemetry_request_timebase(&[(FIELD_COMMANDS, v)]),
        Some(1_699_999_000)
    );
}

/// A nil timebase reads as 0 — "everything you have" — rather than
/// refusing the request.
#[test]
fn a_nil_timebase_reads_as_zero() {
    let mut v = Vec::new();
    msgpack::array(&mut v, 1);
    msgpack::map(&mut v, 1);
    msgpack::int(&mut v, COMMAND_TELEMETRY_REQUEST);
    msgpack::nil(&mut v);
    assert_eq!(telemetry_request_timebase(&[(FIELD_COMMANDS, v)]), Some(0));
}

/// The request may share the list with other commands; the scan skips
/// what it does not know, like the Telemeter codec skips unknown sensors.
#[test]
fn a_request_behind_an_unknown_command_is_still_found() {
    let mut v = Vec::new();
    msgpack::array(&mut v, 2);
    msgpack::map(&mut v, 1);
    msgpack::int(&mut v, 0x7F);
    msgpack::string(&mut v, "ignored");
    msgpack::map(&mut v, 1);
    msgpack::int(&mut v, COMMAND_TELEMETRY_REQUEST);
    msgpack::int(&mut v, 7);
    assert_eq!(telemetry_request_timebase(&[(FIELD_COMMANDS, v)]), Some(7));
}

/// A truncated commands value yields no request, not a panic and not a
/// misread.
#[test]
fn a_malformed_commands_value_is_not_a_request() {
    let mut v = Vec::new();
    msgpack::array(&mut v, 3);
    msgpack::map(&mut v, 1);
    assert_eq!(telemetry_request_timebase(&[(FIELD_COMMANDS, v)]), None);
}
