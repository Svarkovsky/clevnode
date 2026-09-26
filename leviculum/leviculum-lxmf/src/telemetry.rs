//! Sideband `Telemeter` codec: the values of `FIELD_TELEMETRY` and
//! `FIELD_TELEMETRY_STREAM`.
//!
//! The format is Sideband's, defined by `Telemeter.packed` /
//! `Telemeter.from_packed` in `sense.py` (Sideband `2000d81`), and the rules
//! for reading and writing it are fixed by `docs/src/concepts/telemetry.md`.
//! Out-of-tree references are cited by symbol plus upstream commit, per that
//! document's citation rule: Sideband `2000d81`, Columba `0930293`.
//!
//! The unit is a [`Telemetry`] with *n* sensors, never a position packet with
//! extras bolted on: every sensor is one optional field, one encode arm and
//! one decode arm, so adding one later is a codec change and not a design.
//!
//! Wire shape: a msgpack map from sensor ID to that sensor's packed value.
//! Absence has one encoding on write and two on read:
//!
//! - **We emit the omission.** A sensor without a reading contributes no key.
//! - **We accept both forms on read.** Sideband packs every *active* sensor
//!   and a sensor whose data is unavailable packs as its SID mapped to nil
//!   (`Sensor.pack` returns `None`); that is "sensor present, no reading",
//!   not a malformed message.
//!
//! Unknown sensor IDs are skipped on read, exactly as `Telemeter.from_packed`
//! skips a SID it has no class for — that tolerance is the format's own
//! extension mechanism. A malformed value under a *known* SID likewise yields
//! an absent reading rather than an error, because every reference sensor's
//! `unpack` catches its own exceptions and returns `None`; refusing a payload
//! the origin accepts would be our defect, not the sender's.
//!
//! This codec produces the value half of an LXMF [`Field`](crate::Field) and
//! nothing more: no clock, no cadence, no delivery policy. Those rules live
//! with the producer (`docs/src/concepts/telemetry.md`).

use crate::msgpack::{self, Kind, Number};
use alloc::string::String;
use alloc::vec::Vec;

pub const SID_TIME: i64 = 0x01;
pub const SID_LOCATION: i64 = 0x02;
pub const SID_BATTERY: i64 = 0x04;
pub const SID_PHYSICAL_LINK: i64 = 0x05;
pub const SID_TEMPERATURE: i64 = 0x07;
pub const SID_POWER_PRODUCTION: i64 = 0x12;

/// A position reading, in the wire's own scaled-integer units.
///
/// The reference packs six big-endian fixed-width integers as msgpack bins
/// plus a bare timestamp (`Location.pack`, Sideband `2000d81`):
/// `!i lat*1e6, !i lon*1e6, !i alt*1e2, !I speed*1e2, !i bearing*1e2,
/// !H accuracy*1e2, last_update`. The codec stays in that integer domain —
/// GNSS receivers deliver scaled integers natively, and float rounding
/// parity with two references that disagree on rounding is not a contract
/// we can keep. Converting a measurement into these units, and refusing to
/// pack a fix that is not worth reporting, is the producer's business.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// Degrees north, times 1e6. Negative is the southern hemisphere.
    pub latitude_e6: i32,
    /// Degrees east, times 1e6. Negative is the western hemisphere.
    pub longitude_e6: i32,
    /// Metres above sea level, times 1e2. Negative is below sea level.
    pub altitude_e2: i32,
    /// Metres per second, times 1e2. The wire field is unsigned.
    pub speed_e2: u32,
    /// Degrees, times 1e2.
    pub bearing_e2: i32,
    /// Metres, times 1e2. The wire field is 16 bits wide.
    pub accuracy_e2: u16,
    /// Unix seconds of the fix this reading reports.
    pub last_update: i64,
}

impl Location {
    /// Build a reading from unclamped scaled integers, saturating every
    /// field to its wire width.
    ///
    /// The two references resolve an out-of-range value differently:
    /// Sideband's `Location.pack` lets `struct.pack` raise and drops the
    /// whole reading to nil, Columba's `packLocationTelemetry` clamps
    /// (`minOf(..., 0xFFFF)` for accuracy, `maxOf(..., 0.0)` for speed).
    /// We saturate, on the Columba side: a fix survives its worst field.
    /// Note the accuracy ceiling this buys: a reading whose true accuracy
    /// is worse than 655.35 m goes on the wire *as* 655.35 m, so a producer
    /// that considers such a fix unusable must apply its accuracy threshold
    /// before packing — the codec will not invent a refusal for it.
    pub fn saturating(
        latitude_e6: i64,
        longitude_e6: i64,
        altitude_e2: i64,
        speed_e2: i64,
        bearing_e2: i64,
        accuracy_e2: i64,
        last_update: i64,
    ) -> Self {
        Self {
            latitude_e6: latitude_e6.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            longitude_e6: longitude_e6.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            altitude_e2: altitude_e2.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            speed_e2: speed_e2.clamp(0, u32::MAX as i64) as u32,
            bearing_e2: bearing_e2.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            accuracy_e2: accuracy_e2.clamp(0, u16::MAX as i64) as u16,
            last_update,
        }
    }
}

/// A battery reading: `[charge_percent, charging, temperature]`
/// (`Battery.pack`, Sideband `2000d81`).
///
/// Every element is optional on read because the origin emits it that way:
/// Sideband's own producers pack `temperature` as nil on Android and Linux,
/// its `unpack` accepts a two-element list, and `charging` passes through
/// whatever the platform reported. `charge_percent` is a [`Number`] rather
/// than a fixed float because the reference emits a float where a firmware
/// naturally has an integer percent, and both decode identically at the
/// receiver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Battery {
    pub charge_percent: Number,
    pub charging: Option<bool>,
    /// Degrees Celsius of the battery itself, when the platform knows it.
    pub temperature: Option<Number>,
}

/// A physical-link quality reading: `[rssi, snr, q]`
/// (`PhysicalLink.pack`, Sideband `2000d81`). Each element is nil-able on
/// the wire — the reference initialises all three to `None` and packs them
/// raw.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PhysicalLink {
    pub rssi: Option<Number>,
    pub snr: Option<Number>,
    /// Link quality, 0–100.
    pub q: Option<Number>,
}

/// One power producer: an entry of `PowerProduction.pack` (Sideband
/// `2000d81`), on the wire as `[type_label, [power_watts, custom_icon]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct PowerProducer {
    /// `None` is the reference's default slot, packed as the integer `0x00`;
    /// every named producer is a string label.
    pub type_label: Option<String>,
    /// Watts.
    pub power: Number,
    /// A Material Design icon name, when the producer set one.
    pub custom_icon: Option<String>,
}

/// Degrees Celsius as `SID_TEMPERATURE` wants them, from a reading in
/// quarter-degree steps.
///
/// The nRF52's `sd_temp_get` is one such producer (0.25 °C per step, the
/// raw TEMP register unit) and it is why this lives here rather than in the
/// firmware: the packing convention belongs to the codec, and a second copy
/// of it beside a sensor would drift from this one.
///
/// Sideband rounds the sensor to two decimals before packing
/// (`round(self.android_sensor.temperature, 2)`, `sense.py`
/// `Temperature.update_data`, Sideband `2000d81`), and `pack` then emits
/// that number bare. A quarter-degree step is exact at two decimals and
/// exactly representable as a double, so `× 0.25` **is** that rounding
/// rather than an approximation of it — which is also why the caller is
/// asked for quarter-degrees instead of a float it would have to round
/// without a `libm` to round with.
pub fn celsius_from_quarter_degrees(quarters: i32) -> Number {
    Number::Float(quarters as f64 * 0.25)
}

/// One decoded (or to-be-encoded) Telemeter: the value of `FIELD_TELEMETRY`.
///
/// `None` means "no reading" and encodes as an omitted key. The sensor set
/// here is what an LNode can measure today; adding a sensor is one field,
/// one encode arm, one decode arm.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Telemetry {
    /// `SID_TIME`: unix seconds. The concept's wall-clock rule
    /// (`docs/src/concepts/telemetry.md`) makes this load-bearing at the
    /// receiver; the codec carries it and the producer vouches for it.
    pub time: Option<i64>,
    pub location: Option<Location>,
    pub battery: Option<Battery>,
    pub physical_link: Option<PhysicalLink>,
    /// `SID_TEMPERATURE`: a bare number, degrees Celsius
    /// (`Temperature.pack`, Sideband `2000d81`).
    pub temperature: Option<Number>,
    pub power_production: Option<Vec<PowerProducer>>,
}

impl Telemetry {
    /// True when no sensor has a reading, i.e. [`encode`](Self::encode)
    /// would produce an empty map. The producer rule is to send no message
    /// at all in that case, not an empty one.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Pack into the Telemeter blob — the bytes `Telemeter.from_packed`
    /// consumes. Sensors are emitted in ascending SID order; a sensor
    /// without a reading contributes no key.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        let count = self.time.is_some() as usize
            + self.location.is_some() as usize
            + self.battery.is_some() as usize
            + self.physical_link.is_some() as usize
            + self.temperature.is_some() as usize
            + self.power_production.is_some() as usize;
        msgpack::map(&mut o, count);
        if let Some(time) = self.time {
            msgpack::int(&mut o, SID_TIME);
            msgpack::int(&mut o, time);
        }
        if let Some(location) = &self.location {
            msgpack::int(&mut o, SID_LOCATION);
            msgpack::array(&mut o, 7);
            msgpack::bin(&mut o, &location.latitude_e6.to_be_bytes());
            msgpack::bin(&mut o, &location.longitude_e6.to_be_bytes());
            msgpack::bin(&mut o, &location.altitude_e2.to_be_bytes());
            msgpack::bin(&mut o, &location.speed_e2.to_be_bytes());
            msgpack::bin(&mut o, &location.bearing_e2.to_be_bytes());
            msgpack::bin(&mut o, &location.accuracy_e2.to_be_bytes());
            msgpack::int(&mut o, location.last_update);
        }
        if let Some(battery) = &self.battery {
            msgpack::int(&mut o, SID_BATTERY);
            msgpack::array(&mut o, 3);
            number(&mut o, battery.charge_percent);
            match battery.charging {
                Some(charging) => msgpack::bool(&mut o, charging),
                None => msgpack::nil(&mut o),
            }
            opt_number(&mut o, battery.temperature);
        }
        if let Some(link) = &self.physical_link {
            msgpack::int(&mut o, SID_PHYSICAL_LINK);
            msgpack::array(&mut o, 3);
            opt_number(&mut o, link.rssi);
            opt_number(&mut o, link.snr);
            opt_number(&mut o, link.q);
        }
        if let Some(temperature) = self.temperature {
            msgpack::int(&mut o, SID_TEMPERATURE);
            number(&mut o, temperature);
        }
        if let Some(producers) = &self.power_production {
            msgpack::int(&mut o, SID_POWER_PRODUCTION);
            msgpack::array(&mut o, producers.len());
            for producer in producers {
                msgpack::array(&mut o, 2);
                match &producer.type_label {
                    Some(label) => msgpack::string(&mut o, label),
                    None => msgpack::int(&mut o, 0x00),
                }
                msgpack::array(&mut o, 2);
                number(&mut o, producer.power);
                match &producer.custom_icon {
                    Some(icon) => msgpack::string(&mut o, icon),
                    None => msgpack::nil(&mut o),
                }
            }
        }
        o
    }

    /// The complete `FIELD_TELEMETRY` value: the blob of
    /// [`encode`](Self::encode) wrapped as one msgpack bin, ready to be the
    /// value half of a [`Field`](crate::Field). Sideband stores the packed
    /// bytes in the field (`lxm.fields[FIELD_TELEMETRY] = telemeter.packed()`),
    /// so inside the fields map the value is a bin, not a nested map.
    pub fn encode_field_value(&self) -> Vec<u8> {
        let blob = self.encode();
        let mut o = Vec::new();
        msgpack::bin(&mut o, &blob);
        o
    }

    /// Decode a Telemeter blob (the *unwrapped* packed bytes).
    ///
    /// Errors only where `Telemeter.from_packed` would return `None` for the
    /// whole payload: bytes that are not a msgpack map. Everything below the
    /// map is tolerated the way the reference tolerates it — unknown SIDs
    /// and non-integer keys are skipped, a SID mapped to nil is a sensor
    /// without a reading, and a malformed value under a known SID yields an
    /// absent reading, because every reference sensor's `unpack` catches its
    /// own exceptions. A duplicate SID resolves to the last occurrence,
    /// as Python dict insertion does. Trailing bytes after the map are
    /// ignored, as `umsgpack.unpackb` ignores them.
    pub fn decode(d: &[u8]) -> Result<Self, msgpack::Error> {
        let mut p = 0;
        let entries = msgpack::map_len(d, &mut p)?;
        let mut telemetry = Self::default();
        for _ in 0..entries {
            let key = msgpack::raw(d, &mut p)?;
            let value = msgpack::raw(d, &mut p)?;
            let sid = {
                let mut kp = 0;
                match msgpack::read_int(key, &mut kp) {
                    Ok(sid) => sid,
                    // A non-integer key matches no SID; from_packed skips it.
                    Err(_) => continue,
                }
            };
            // SID -> nil: sensor present, no reading. The second encoding
            // of absence, accepted per docs/src/concepts/telemetry.md.
            if matches!(msgpack::peek_kind(value, 0), Ok(Kind::Nil)) {
                continue;
            }
            match sid {
                SID_TIME => telemetry.time = parse_time(value),
                SID_LOCATION => telemetry.location = parse_location(value),
                SID_BATTERY => telemetry.battery = parse_battery(value),
                SID_PHYSICAL_LINK => telemetry.physical_link = parse_physical_link(value),
                SID_TEMPERATURE => telemetry.temperature = parse_number(value),
                SID_POWER_PRODUCTION => telemetry.power_production = parse_power_production(value),
                _ => {}
            }
        }
        Ok(telemetry)
    }

    /// Decode a complete `FIELD_TELEMETRY` value (a msgpack bin wrapping the
    /// Telemeter blob), the inverse of
    /// [`encode_field_value`](Self::encode_field_value).
    pub fn decode_field_value(d: &[u8]) -> Result<Self, msgpack::Error> {
        let mut p = 0;
        Self::decode(msgpack::read_bin(d, &mut p)?)
    }
}

/// One row of a `FIELD_TELEMETRY_STREAM` value:
/// `[source_hash, timestamp, packed_telemetry, appearance]`.
///
/// `telemetry` is the packed Telemeter blob, carried opaquely — a collector
/// relays rows it could not itself decode. `appearance` is likewise one
/// complete raw msgpack value ([`msgpack::append_raw`] compatible), because
/// its shape belongs to the viewers, not to this codec. `timestamp` keeps
/// its wire family ([`Number`]): Sideband stamps rows from stored floats.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamEntry {
    pub source: Vec<u8>,
    pub timestamp: Number,
    pub telemetry: Vec<u8>,
    pub appearance: Option<Vec<u8>>,
}

/// Encode the complete `FIELD_TELEMETRY_STREAM` value: a msgpack array of
/// rows, **always four elements per row**, nil in the fourth when there is
/// no appearance.
///
/// The arity is a hard requirement, not cosmetics: Sideband's stream ingest
/// indexes `telemetry_entry[3]` unconditionally and the resulting
/// `IndexError` aborts the whole message, so one short row loses every
/// other row in the same response (`docs/src/concepts/telemetry.md`, wire
/// table).
pub fn encode_stream_field_value(entries: &[StreamEntry]) -> Vec<u8> {
    let mut o = Vec::new();
    msgpack::array(&mut o, entries.len());
    for entry in entries {
        msgpack::array(&mut o, 4);
        msgpack::bin(&mut o, &entry.source);
        number(&mut o, entry.timestamp);
        msgpack::bin(&mut o, &entry.telemetry);
        match &entry.appearance {
            Some(appearance) => msgpack::append_raw(&mut o, appearance),
            None => msgpack::nil(&mut o),
        }
    }
    o
}

/// Decode a `FIELD_TELEMETRY_STREAM` value, tolerantly.
///
/// We emit four-element rows; on read we accept what the ecosystem emits:
/// Columba's native collector sends three elements when appearance is
/// absent (`0930293`), so a three-element row decodes with `appearance:
/// None` instead of failing. A row that lacks even the three semantic
/// elements, or is not an array at all, is dropped and the remaining rows
/// survive — the opposite of the reference's whole-message abort, which is
/// the defect the four-element emit rule exists to avoid triggering.
pub fn decode_stream_field_value(d: &[u8]) -> Result<Vec<StreamEntry>, msgpack::Error> {
    let mut p = 0;
    let rows = msgpack::array_len(d, &mut p)?;
    let mut entries = Vec::new();
    for _ in 0..rows {
        let row = msgpack::raw(d, &mut p)?;
        if let Some(entry) = parse_stream_row(row) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn parse_stream_row(row: &[u8]) -> Option<StreamEntry> {
    let mut p = 0;
    let elements = msgpack::array_len(row, &mut p).ok()?;
    if elements < 3 {
        return None;
    }
    let source = msgpack::read_bin(row, &mut p).ok()?.to_vec();
    let timestamp = msgpack::read_number(row, &mut p).ok()?;
    let telemetry = msgpack::read_bin(row, &mut p).ok()?.to_vec();
    let appearance = if elements > 3 {
        let raw = msgpack::raw(row, &mut p).ok()?;
        match msgpack::peek_kind(raw, 0) {
            Ok(Kind::Nil) => None,
            _ => Some(raw.to_vec()),
        }
    } else {
        None
    };
    Some(StreamEntry {
        source,
        timestamp,
        telemetry,
        appearance,
    })
}

fn number(out: &mut Vec<u8>, value: Number) {
    match value {
        Number::Int(v) => msgpack::int(out, v),
        Number::Float(v) => msgpack::f64(out, v),
    }
}

fn opt_number(out: &mut Vec<u8>, value: Option<Number>) {
    match value {
        Some(v) => number(out, v),
        None => msgpack::nil(out),
    }
}

fn parse_time(v: &[u8]) -> Option<i64> {
    let mut p = 0;
    // The origin emits an int (`int(time.time())`); a float is taken the
    // way Python arithmetic would use it.
    match msgpack::read_number(v, &mut p).ok()? {
        Number::Int(seconds) => Some(seconds),
        Number::Float(seconds) => Some(seconds as i64),
    }
}

fn parse_number(v: &[u8]) -> Option<Number> {
    let mut p = 0;
    msgpack::read_number(v, &mut p).ok()
}

/// Nil or a number; anything else is the caller's parse failure.
fn parse_nilable_number(v: &[u8], p: &mut usize) -> Result<Option<Number>, msgpack::Error> {
    if matches!(msgpack::peek_kind(v, *p), Ok(Kind::Nil)) {
        msgpack::read_nil(v, p)?;
        return Ok(None);
    }
    msgpack::read_number(v, p).map(Some)
}

fn parse_location(v: &[u8]) -> Option<Location> {
    let mut p = 0;
    let elements = msgpack::array_len(v, &mut p).ok()?;
    // The reference indexes elements 0..=6 and ignores anything beyond;
    // `struct.unpack` makes the blob widths exact, not minimums.
    if elements < 7 {
        return None;
    }
    let mut fixed = |width: usize| -> Option<&[u8]> {
        let blob = msgpack::read_bin(v, &mut p).ok()?;
        (blob.len() == width).then_some(blob)
    };
    let latitude_e6 = i32::from_be_bytes(fixed(4)?.try_into().ok()?);
    let longitude_e6 = i32::from_be_bytes(fixed(4)?.try_into().ok()?);
    let altitude_e2 = i32::from_be_bytes(fixed(4)?.try_into().ok()?);
    let speed_e2 = u32::from_be_bytes(fixed(4)?.try_into().ok()?);
    let bearing_e2 = i32::from_be_bytes(fixed(4)?.try_into().ok()?);
    let accuracy_e2 = u16::from_be_bytes(fixed(2)?.try_into().ok()?);
    let last_update = match msgpack::read_number(v, &mut p).ok()? {
        Number::Int(seconds) => seconds,
        Number::Float(seconds) => seconds as i64,
    };
    Some(Location {
        latitude_e6,
        longitude_e6,
        altitude_e2,
        speed_e2,
        bearing_e2,
        accuracy_e2,
        last_update,
    })
}

fn parse_battery(v: &[u8]) -> Option<Battery> {
    let mut p = 0;
    let elements = msgpack::array_len(v, &mut p).ok()?;
    // `Battery.unpack` requires the first two elements and treats the
    // temperature as optional (`if len(packed) > 2`). A nil charge percent
    // fails its `round(packed[0], 1)` and drops the sensor; matched here.
    if elements < 2 {
        return None;
    }
    let charge_percent = msgpack::read_number(v, &mut p).ok()?;
    let charging = match msgpack::peek_kind(v, p).ok()? {
        Kind::True => {
            msgpack::read_bool(v, &mut p).ok()?;
            Some(true)
        }
        Kind::False => {
            msgpack::read_bool(v, &mut p).ok()?;
            Some(false)
        }
        Kind::Nil => {
            msgpack::read_nil(v, &mut p).ok()?;
            None
        }
        // The reference stores the element raw and only ever tests its
        // truthiness, so a numeric flag stays meaningful.
        _ => Some(msgpack::read_number(v, &mut p).ok()? != Number::Int(0)),
    };
    let temperature = if elements > 2 {
        parse_nilable_number(v, &mut p).ok()?
    } else {
        None
    };
    Some(Battery {
        charge_percent,
        charging,
        temperature,
    })
}

fn parse_physical_link(v: &[u8]) -> Option<PhysicalLink> {
    let mut p = 0;
    if msgpack::array_len(v, &mut p).ok()? < 3 {
        return None;
    }
    Some(PhysicalLink {
        rssi: parse_nilable_number(v, &mut p).ok()?,
        snr: parse_nilable_number(v, &mut p).ok()?,
        q: parse_nilable_number(v, &mut p).ok()?,
    })
}

fn parse_power_production(v: &[u8]) -> Option<Vec<PowerProducer>> {
    let mut p = 0;
    let entries = msgpack::array_len(v, &mut p).ok()?;
    let mut producers = Vec::new();
    for _ in 0..entries {
        let entry = msgpack::raw(v, &mut p).ok()?;
        // `PowerProduction.unpack` performs no per-entry validation at all;
        // an entry this codec cannot type is dropped, not fatal.
        if let Some(producer) = parse_power_producer(entry) {
            producers.push(producer);
        }
    }
    Some(producers)
}

fn parse_power_producer(entry: &[u8]) -> Option<PowerProducer> {
    let mut p = 0;
    if msgpack::array_len(entry, &mut p).ok()? < 2 {
        return None;
    }
    // `update_producer` writes the integer 0x00 for the unnamed default
    // slot and a string for every named producer.
    let type_label = {
        let mut sp = p;
        if let Ok(label) = msgpack::read_str(entry, &mut sp) {
            p = sp;
            Some(String::from(label))
        } else {
            msgpack::read_int(entry, &mut p).ok()?;
            None
        }
    };
    let mut vp = p;
    if msgpack::array_len(entry, &mut vp).ok()? < 1 {
        return None;
    }
    let power = msgpack::read_number(entry, &mut vp).ok()?;
    let custom_icon = match msgpack::peek_kind(entry, vp) {
        Ok(Kind::Other) => msgpack::read_str(entry, &mut vp).ok().map(String::from),
        _ => None,
    };
    Some(PowerProducer {
        type_label,
        power,
        custom_icon,
    })
}

// ---------------------------------------------------------------------------
// The reporting message (Codeberg #236)
// ---------------------------------------------------------------------------

/// Why a telemetry reading did not become a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    /// No sensor had a reading. The producer rule is to send nothing at
    /// all rather than an empty Telemeter: an empty map is a message a
    /// receiver stores, dated, under a source that told it nothing.
    NoReadings,
    /// The message could not be built or signed.
    Message(crate::message::MessageError),
}

impl core::fmt::Display for ReportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoReadings => write!(f, "no sensor had a reading"),
            Self::Message(e) => write!(f, "telemetry message: {e}"),
        }
    }
}

impl core::error::Error for ReportError {}

/// Build the LXMF message that carries one telemetry reading.
///
/// Every rule `docs/src/concepts/telemetry.md` puts on a *reporting*
/// message is applied here rather than left to each caller, because each
/// of them is a rule a caller can only get wrong:
///
/// * **`content` and `title` are empty.** Not tidiness — Sideband
///   suppresses the notification for a telemetry-bearing message only when
///   both are empty, so a single non-empty byte turns the feature into a
///   notification once per reporting interval, forever. The parameters do
///   not exist, so they cannot be filled.
/// * **Delivery is opportunistic.** A direct delivery pays three round
///   trips of link setup for a payload of roughly fifty bytes; a
///   propagated one adds a delay that makes a position stale. A port that
///   needs something else builds its own message and states why.
/// * **An empty Telemeter is refused.** [`ReportError::NoReadings`].
///
/// `timestamp` is the LXMF message timestamp, in unix seconds; the
/// reading's own `SID_TIME` is inside `telemetry` and is what the receiver
/// keys on. Both come from the producer's calendar, and the calendar rule
/// — best honest estimate, never ahead of it — is the producer's to keep.
pub fn build_report(
    destination_hash: [u8; 16],
    source_hash: [u8; 16],
    source: &leviculum_core::identity::Identity,
    timestamp: f64,
    telemetry: &Telemetry,
) -> Result<crate::message::Message, ReportError> {
    if telemetry.is_empty() {
        return Err(ReportError::NoReadings);
    }
    crate::message::Message::create(
        destination_hash,
        source_hash,
        source,
        timestamp,
        Vec::new(),
        Vec::new(),
        alloc::vec![(
            crate::constants::FIELD_TELEMETRY,
            telemetry.encode_field_value()
        )],
        crate::message::DeliveryMethod::Opportunistic,
    )
    .map_err(ReportError::Message)
}

// ---------------------------------------------------------------------------
// Telemetry requests (Codeberg #371)
// ---------------------------------------------------------------------------

/// Sideband's command id for "send me your telemetry since this timebase",
/// carried inside `FIELD_COMMANDS` (`reference/LXMF/LXMF/LXMF.py:16`).
///
/// ASSUMPTION to verify against a captured request: Sideband's source is
/// not in `reference/`, so the id (`Commands.TELEMETRY_REQUEST` in
/// Sideband core) and the value shape below are taken from Sideband's
/// documented behaviour, not from a citable line. The shape implemented:
/// the field value is a list of command maps, a request is
/// `[{0x01: <timebase>}]` with the timebase in unix seconds. Tolerated on
/// read: the timebase as any msgpack number, as the first element of a
/// list, or nil (read as 0, "everything you have").
pub const COMMAND_TELEMETRY_REQUEST: i64 = 0x01;

/// Find a telemetry request among a message's fields.
///
/// Returns the request's timebase, or `None` when no well-formed
/// `FIELD_COMMANDS` entry carries [`COMMAND_TELEMETRY_REQUEST`]. Unknown
/// command ids and non-map list entries are skipped, the same tolerance
/// the Telemeter codec applies to unknown sensors; a malformed field
/// yields `None` rather than an error, because "not a request" is all a
/// consumer can do about it either way.
pub fn telemetry_request_timebase(fields: &[crate::Field]) -> Option<i64> {
    let (_, raw) = fields
        .iter()
        .find(|(key, _)| *key == crate::constants::FIELD_COMMANDS)?;
    let d = raw.as_slice();
    let mut p = 0;
    let commands = msgpack::array_len(d, &mut p).ok()?;
    for _ in 0..commands {
        let entries = match msgpack::map_len(d, &mut p) {
            Ok(entries) => entries,
            // Not a command map; skip the value and keep scanning.
            Err(_) => {
                msgpack::skip(d, &mut p).ok()?;
                continue;
            }
        };
        for _ in 0..entries {
            let id = msgpack::read_int(d, &mut p).ok()?;
            if id != COMMAND_TELEMETRY_REQUEST {
                msgpack::skip(d, &mut p).ok()?;
                continue;
            }
            return match msgpack::peek_kind(d, p).ok()? {
                // `[timebase, ...]`: trailing elements (Sideband adds a
                // collector flag) do not change what is asked of a node
                // that only ever answers with its own current reading.
                Kind::Array => {
                    let n = msgpack::array_len(d, &mut p).ok()?;
                    if n == 0 {
                        return None;
                    }
                    Some(timebase_secs(msgpack::read_number(d, &mut p).ok()?))
                }
                Kind::Nil => Some(0),
                _ => Some(timebase_secs(msgpack::read_number(d, &mut p).ok()?)),
            };
        }
    }
    None
}

/// A timebase as unix seconds, whatever numeric family the wire used —
/// Python's `time.time()` is a float.
fn timebase_secs(number: Number) -> i64 {
    match number {
        Number::Int(v) => v,
        Number::Float(v) => v as i64,
    }
}

/// What one inbound delivery-destination packet is, as far as the
/// telemetry-request feature is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryRequestVerdict {
    /// Not an LXMF message, or one without a telemetry request: none of
    /// this feature's business, whoever sent it.
    NotARequest,
    /// A telemetry request from a sender that is not the allowed one.
    NotAllowed { source: [u8; 16] },
    /// A request naming the allowed sender, screened without that
    /// sender's identity — the caller holds no key to verify against.
    Unverifiable { source: [u8; 16] },
    /// A request naming the allowed sender whose signature does not
    /// verify against that sender's identity.
    BadSignature { source: [u8; 16] },
    /// A verified telemetry request from the allowed sender.
    Request { source: [u8; 16], timebase: i64 },
}

/// Screen one opportunistic packet delivered to `delivery_hash` for a
/// telemetry request from the one allowed sender.
///
/// `allowed_source` is the sender reports are configured to go to —
/// `None` when no target is set, in which case every request is
/// [`TelemetryRequestVerdict::NotAllowed`]. `allowed_identity` is that
/// sender's identity when the caller holds it; without it a request can
/// be attributed but not verified. The order is deliberate: what the
/// packet *is* first, who sent it second, whether the signature holds
/// last — so a caller's rejection line can name the most specific reason.
pub fn screen_telemetry_request(
    on_air: &[u8],
    delivery_hash: [u8; 16],
    allowed_source: Option<[u8; 16]>,
    allowed_identity: Option<&leviculum_core::identity::Identity>,
) -> TelemetryRequestVerdict {
    use crate::message::{DeliveryMethod, Message, Verification};
    let Ok(message) = Message::unpack(
        on_air,
        Some(delivery_hash),
        allowed_identity,
        DeliveryMethod::Opportunistic,
    ) else {
        return TelemetryRequestVerdict::NotARequest;
    };
    let Some(timebase) = telemetry_request_timebase(&message.fields) else {
        return TelemetryRequestVerdict::NotARequest;
    };
    let source = message.source_hash;
    if allowed_source != Some(source) {
        return TelemetryRequestVerdict::NotAllowed { source };
    }
    match message.verification {
        Verification::Valid => TelemetryRequestVerdict::Request { source, timebase },
        Verification::Invalid => TelemetryRequestVerdict::BadSignature { source },
        Verification::Unverified => TelemetryRequestVerdict::Unverifiable { source },
    }
}
