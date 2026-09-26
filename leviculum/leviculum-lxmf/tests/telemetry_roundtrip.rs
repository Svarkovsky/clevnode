//! Codeberg #237: bounded round-trip property tests for the Telemeter codec.
//!
//! The golden vectors (`telemetry_vectors.rs`) prove byte-identity against
//! the two reference implementations at fixed points; these tests sweep the
//! codec's whole value domain with a seeded deterministic generator and
//! assert that every encodable [`Telemetry`] and every encodable stream
//! survive `decode(encode(x)) == x`. Deterministic seeds, not `#[ignore]`d
//! fuzzing: a failure here reproduces on every run and names its seed.
//!
//! The generator is bounded to what the encoder can emit — `Number::Float`
//! excludes NaN because NaN breaks the equality the property is stated in
//! (and no reference producer emits it), and `charging` is a bool or nil
//! because the numeric-truthiness form is a read-side tolerance, not an
//! encoding this codec produces.

use leviculum_lxmf::msgpack::{self, Number};
use leviculum_lxmf::telemetry::{
    decode_stream_field_value, encode_stream_field_value, Battery, Location, PhysicalLink,
    PowerProducer, StreamEntry, Telemetry,
};

/// SplitMix64: a tiny deterministic PRNG so the sweep needs no new
/// dev-dependency and every failure is reproducible from the seed constant.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.next().is_multiple_of(one_in)
    }

    /// Any finite-or-infinite f64, never NaN. Raw bit patterns cover the
    /// subnormals and signed zeros a value-domain sweep would miss.
    fn float(&mut self) -> f64 {
        loop {
            let v = f64::from_bits(self.next());
            if !v.is_nan() {
                return v;
            }
        }
    }

    fn number(&mut self) -> Number {
        if self.chance(2) {
            Number::Int(self.next() as i64)
        } else {
            Number::Float(self.float())
        }
    }

    fn opt_number(&mut self) -> Option<Number> {
        self.chance(3).then(|| self.number())
    }

    fn ascii(&mut self, max_len: u64) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz_0123456789";
        let len = self.next() % (max_len + 1);
        (0..len)
            .map(|_| ALPHABET[(self.next() % ALPHABET.len() as u64) as usize] as char)
            .collect()
    }

    fn bytes(&mut self, max_len: u64) -> Vec<u8> {
        let len = self.next() % (max_len + 1);
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn arbitrary_telemetry(rng: &mut SplitMix64) -> Telemetry {
    Telemetry {
        time: rng.chance(2).then(|| rng.next() as i64),
        location: rng.chance(2).then(|| Location {
            latitude_e6: rng.next() as i32,
            longitude_e6: rng.next() as i32,
            altitude_e2: rng.next() as i32,
            speed_e2: rng.next() as u32,
            bearing_e2: rng.next() as i32,
            accuracy_e2: rng.next() as u16,
            last_update: rng.next() as i64,
        }),
        battery: rng.chance(2).then(|| Battery {
            charge_percent: rng.number(),
            charging: rng.chance(3).then(|| rng.chance(2)),
            temperature: rng.opt_number(),
        }),
        physical_link: rng.chance(2).then(|| PhysicalLink {
            rssi: rng.opt_number(),
            snr: rng.opt_number(),
            q: rng.opt_number(),
        }),
        temperature: rng.opt_number(),
        power_production: rng.chance(2).then(|| {
            (0..rng.next() % 4)
                .map(|_| PowerProducer {
                    type_label: rng.chance(2).then(|| rng.ascii(8)),
                    power: rng.number(),
                    custom_icon: rng.chance(3).then(|| rng.ascii(12)),
                })
                .collect()
        }),
    }
}

/// One complete non-nil msgpack value for the appearance slot. Nil is
/// excluded because the codec defines nil-in-slot-four as "no appearance",
/// i.e. it decodes to `None`, not to a raw value.
fn arbitrary_appearance(rng: &mut SplitMix64) -> Vec<u8> {
    let mut o = Vec::new();
    match rng.next() % 4 {
        0 => msgpack::int(&mut o, rng.next() as i64),
        1 => msgpack::string(&mut o, &rng.ascii(10)),
        2 => msgpack::bin(&mut o, &rng.bytes(16)),
        _ => {
            // The shape Sideband actually ships: a small array of scalars.
            msgpack::array(&mut o, 2);
            msgpack::string(&mut o, &rng.ascii(6));
            msgpack::f64(&mut o, rng.float());
        }
    }
    o
}

#[test]
fn every_encodable_telemetry_round_trips() {
    let mut rng = SplitMix64(0x2337_0001);
    for case in 0..512 {
        let telemetry = arbitrary_telemetry(&mut rng);
        let packed = telemetry.encode();
        let decoded = Telemetry::decode(&packed)
            .unwrap_or_else(|e| panic!("case {case}: decode failed: {e:?} for {telemetry:?}"));
        assert_eq!(decoded, telemetry, "case {case}: value round trip");
        assert_eq!(
            decoded.encode(),
            packed,
            "case {case}: re-encode byte identity"
        );
    }
}

#[test]
fn every_encodable_telemetry_round_trips_through_the_field_wrapper() {
    let mut rng = SplitMix64(0x2337_0002);
    for case in 0..256 {
        let telemetry = arbitrary_telemetry(&mut rng);
        let field_value = telemetry.encode_field_value();
        let decoded = Telemetry::decode_field_value(&field_value)
            .unwrap_or_else(|e| panic!("case {case}: decode failed: {e:?} for {telemetry:?}"));
        assert_eq!(decoded, telemetry, "case {case}");
    }
}

#[test]
fn every_encodable_stream_round_trips() {
    let mut rng = SplitMix64(0x2337_0003);
    for case in 0..256 {
        let entries: Vec<StreamEntry> = (0..rng.next() % 5)
            .map(|_| StreamEntry {
                source: rng.bytes(32),
                timestamp: rng.number(),
                telemetry: arbitrary_telemetry(&mut rng).encode(),
                appearance: rng.chance(2).then(|| arbitrary_appearance(&mut rng)),
            })
            .collect();
        let packed = encode_stream_field_value(&entries);
        let decoded = decode_stream_field_value(&packed)
            .unwrap_or_else(|e| panic!("case {case}: decode failed: {e:?}"));
        assert_eq!(decoded, entries, "case {case}: value round trip");
        assert_eq!(
            encode_stream_field_value(&decoded),
            packed,
            "case {case}: re-encode byte identity"
        );
    }
}
