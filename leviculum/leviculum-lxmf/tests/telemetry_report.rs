//! Codeberg #236: the shape of a reporting message.
//!
//! The codec itself is proven against the origin implementation in
//! `telemetry_vectors.rs`; this file proves the *message* a reporting node
//! puts on the wire around it. The oracle is the same #237 fixture set, so
//! a payload that survives this test is a payload Sideband's
//! `Telemeter.from_packed` reads to the values the fixture records — the
//! message wrapper cannot quietly re-encode it.

mod common;

use leviculum_core::Identity;
use leviculum_lxmf::constants::FIELD_TELEMETRY;
use leviculum_lxmf::msgpack::Number;
use leviculum_lxmf::telemetry::{
    build_report, celsius_from_quarter_degrees, Battery, Location, ReportError, Telemetry,
};
use leviculum_lxmf::{DeliveryMethod, Message, Verification};

const FIXTURES: &str = include_str!("../../docs/src/appendix/lxmf/vectors/telemetry_vectors.json");

fn field(id: &str, name: &str) -> &'static str {
    common::fixture_from(FIXTURES, id, name)
}

fn int(id: &str, name: &str) -> i64 {
    field(id, name).parse().unwrap()
}

fn source_identity() -> Identity {
    let bytes = hex::decode(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
         202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
    )
    .unwrap();
    Identity::from_private_key_bytes(&bytes).unwrap()
}

/// The two-sensor reading Columba's own `packLocationTelemetry` emits, so
/// the payload under test is byte-identical to what its
/// `unpackLocationTelemetry` reads from its own producers.
fn columba_reading() -> Telemetry {
    let id = "VEC-TELE-COLUMBA";
    Telemetry {
        time: Some(int(id, "time_utc")),
        location: Some(Location {
            latitude_e6: int(id, "latitude_e6") as i32,
            longitude_e6: int(id, "longitude_e6") as i32,
            altitude_e2: int(id, "altitude_e2") as i32,
            speed_e2: int(id, "speed_e2") as u32,
            bearing_e2: int(id, "bearing_e2") as i32,
            accuracy_e2: int(id, "accuracy_e2") as u16,
            last_update: int(id, "last_update"),
        }),
        ..Telemetry::default()
    }
}

#[test]
fn a_report_carries_the_reference_payload_under_field_telemetry() {
    let source = source_identity();
    let reading = columba_reading();
    let message = build_report([0x11; 16], [0x22; 16], &source, 1_790_000_000.0, &reading).unwrap();

    assert_eq!(message.fields.len(), 1);
    let (id, value) = &message.fields[0];
    assert_eq!(*id, FIELD_TELEMETRY);

    // The field value is the fixture's packed blob, wrapped as one
    // msgpack bin exactly as Sideband stores it.
    let expected = hex::decode(field("VEC-TELE-COLUMBA", "packed_hex")).unwrap();
    assert_eq!(*value, reading.encode_field_value());
    assert_eq!(Telemetry::decode(&expected).unwrap(), reading);
    assert_eq!(Telemetry::decode_field_value(value).unwrap(), reading);
}

#[test]
fn a_report_survives_the_wire_and_still_decodes_to_the_fixture_values() {
    let source = source_identity();
    let reading = columba_reading();
    let destination = [0x11u8; 16];
    let message =
        build_report(destination, [0x22; 16], &source, 1_790_000_000.0, &reading).unwrap();

    // Opportunistic: the destination hash is stripped on air and put back
    // by the receiver, which is the only way a receiver ever sees this.
    let on_air = message.on_air().unwrap();
    let received = Message::unpack(
        &on_air,
        Some(destination),
        Some(&source),
        DeliveryMethod::Opportunistic,
    )
    .unwrap();

    assert_eq!(received.verification, Verification::Valid);
    assert_eq!(received.fields.len(), 1);
    assert_eq!(received.fields[0].0, FIELD_TELEMETRY);
    let decoded = Telemetry::decode_field_value(&received.fields[0].1).unwrap();
    assert_eq!(decoded, reading);
    assert_eq!(
        decoded.location.unwrap().latitude_e6,
        int("VEC-TELE-COLUMBA", "latitude_e6") as i32
    );
}

#[test]
fn content_and_title_are_empty_on_every_report() {
    // The hard wire rule: Sideband suppresses the notification for a
    // telemetry-bearing message only when both are empty. There is no
    // parameter to fill, and the packed bytes must show it.
    let source = source_identity();
    let message = build_report(
        [0x11; 16],
        [0x22; 16],
        &source,
        1_790_000_000.0,
        &columba_reading(),
    )
    .unwrap();
    assert!(message.title.is_empty());
    assert!(message.content.is_empty());

    let received = Message::unpack(
        &message.on_air().unwrap(),
        Some([0x11; 16]),
        Some(&source),
        DeliveryMethod::Opportunistic,
    )
    .unwrap();
    assert!(received.title.is_empty(), "title survived the round trip");
    assert!(
        received.content.is_empty(),
        "content survived the round trip"
    );
}

#[test]
fn a_report_is_opportunistic() {
    let source = source_identity();
    let message = build_report(
        [0x11; 16],
        [0x22; 16],
        &source,
        1_790_000_000.0,
        &columba_reading(),
    )
    .unwrap();
    assert_eq!(message.method, DeliveryMethod::Opportunistic);
}

#[test]
fn a_heartbeat_without_a_fix_carries_no_location_key_at_all() {
    // The absence encoding: a sensor without a reading contributes no
    // key, never a placeholder coordinate.
    let source = source_identity();
    let reading = Telemetry {
        time: Some(1_790_000_000),
        battery: Some(Battery {
            charge_percent: Number::Int(63),
            charging: Some(false),
            temperature: None,
        }),
        ..Telemetry::default()
    };
    let message = build_report([0x11; 16], [0x22; 16], &source, 1_790_000_000.0, &reading).unwrap();
    let decoded = Telemetry::decode_field_value(&message.fields[0].1).unwrap();
    assert_eq!(decoded, reading);
    assert!(decoded.location.is_none());
    assert!(decoded.battery.is_some());
}

#[test]
fn a_reading_with_no_sensors_is_not_a_message() {
    let source = source_identity();
    assert_eq!(
        build_report(
            [0x11; 16],
            [0x22; 16],
            &source,
            1_790_000_000.0,
            &Telemetry::default()
        )
        .unwrap_err(),
        ReportError::NoReadings
    );
}

#[test]
fn a_position_report_fits_one_opportunistic_packet() {
    // The concept prices opportunistic delivery against "roughly fifty
    // bytes". A report that outgrew a single packet would silently become
    // a link plus a resource transfer, which is the shape the concept
    // says telemetry is not.
    let source = source_identity();
    let reading = Telemetry {
        battery: Some(Battery {
            charge_percent: Number::Int(63),
            charging: Some(false),
            temperature: None,
        }),
        ..columba_reading()
    };
    let message = build_report([0x11; 16], [0x22; 16], &source, 1_790_000_000.0, &reading).unwrap();
    let on_air = message.on_air().unwrap();
    // Reticulum's single-packet payload budget after the 16-byte
    // destination hash and the encryption overhead the stack adds.
    assert!(
        on_air.len() <= 383,
        "a position + battery report is {} B on air",
        on_air.len()
    );
}

// ---------------------------------------------------------------------------
// The die-temperature sensor (SID 0x07)
// ---------------------------------------------------------------------------

/// A reading as it reaches a receiver: through `build_report`, onto the
/// air, and back out of `Message::unpack`. Nothing here trusts the bytes
/// the encoder happened to produce — the assertion is on what a peer reads.
fn round_trip(source: &Identity, reading: &Telemetry) -> Telemetry {
    let destination = [0xD1u8; 16];
    let message = build_report(
        destination,
        *source.hash(),
        source,
        1_790_000_000.0,
        reading,
    )
    .unwrap();
    let received = Message::unpack(
        &message.on_air().unwrap(),
        Some(destination),
        Some(source),
        DeliveryMethod::Opportunistic,
    )
    .unwrap();
    assert_eq!(received.verification, Verification::Valid);
    assert_eq!(received.fields[0].0, FIELD_TELEMETRY);
    Telemetry::decode_field_value(&received.fields[0].1).unwrap()
}

/// An LNode reports every sensor it has, and every nRF52840 has a die
/// thermometer. The reading survives the report as SID 0x07, in degrees
/// Celsius and not in the raw quarter-degree steps `sd_temp_get` speaks.
#[test]
fn a_temperature_reading_reaches_the_report_as_degrees_celsius() {
    let source = source_identity();
    // 93 quarter-degree steps = 23.25 °C.
    let reading = Telemetry {
        temperature: Some(celsius_from_quarter_degrees(93)),
        ..columba_reading()
    };
    let decoded = round_trip(&source, &reading);

    assert_eq!(decoded.temperature.map(Number::as_f64), Some(23.25));
    // And it displaced nothing that was already there.
    assert_eq!(decoded.location, reading.location);
    assert_eq!(decoded.time, reading.time);
}

/// **The positive control.** The same message shape from a platform that
/// reports no temperature contributes no key at all — the concept's
/// absence encoding — so "did not measure" stays distinguishable from
/// "measured zero", and the test above cannot be passing on a decoder that
/// invents a reading.
#[test]
fn a_platform_without_a_thermometer_contributes_no_temperature_key() {
    let source = source_identity();
    let reading = columba_reading();
    assert_eq!(reading.temperature, None, "the fixture has no thermometer");
    assert_eq!(round_trip(&source, &reading).temperature, None);
}

/// Zero degrees is a reading, not an absence.
#[test]
fn zero_degrees_is_a_reading_and_not_a_missing_sensor() {
    let source = source_identity();
    let reading = Telemetry {
        temperature: Some(celsius_from_quarter_degrees(0)),
        ..columba_reading()
    };
    assert_eq!(
        round_trip(&source, &reading)
            .temperature
            .map(Number::as_f64),
        Some(0.0)
    );
}

/// A below-freezing die is a negative number on the wire, which is the one
/// case a sign-blind conversion would get wrong, and the quarter-degree
/// step is carried exactly rather than rounded to a whole degree.
#[test]
fn a_negative_temperature_keeps_its_sign_and_its_quarter_degree() {
    let source = source_identity();
    let reading = Telemetry {
        temperature: Some(celsius_from_quarter_degrees(-51)),
        ..columba_reading()
    };
    assert_eq!(
        round_trip(&source, &reading)
            .temperature
            .map(Number::as_f64),
        Some(-12.75)
    );
}

/// The conversion mirrors Sideband's `round(temperature, 2)` exactly
/// rather than approximately: every quarter-degree step lands on a value
/// that is already two-decimal exact, so rounding it again changes nothing.
#[test]
fn every_quarter_degree_step_is_already_two_decimal_exact() {
    for quarters in -400..=400 {
        let celsius = celsius_from_quarter_degrees(quarters).as_f64();
        assert_eq!(
            (celsius * 100.0).round() / 100.0,
            celsius,
            "step {quarters} is not exact at two decimals"
        );
    }
}
