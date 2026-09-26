#!/usr/bin/env python3
"""Generate golden vectors for the Telemeter codec (Codeberg #237).

The Telemeter format is Sideband's (`sbapp/sideband/sense.py`), not LXMF's,
and Sideband is not vendored in this tree. This harness therefore imports the
REAL `sense.py` from a Sideband checkout named by the ``SIDEBAND_PATH``
environment variable (the path of the checkout's ``sbapp`` directory), pinned
at the commit the concept document cites:

    Sideband commit 2000d81a44bff57e3b3cb7d45915ba29bbde4e18

Run from the repository root:

    PYTHONPATH=reference/Reticulum SIDEBAND_PATH=/path/to/Sideband/sbapp \\
        python3 docs/src/appendix/lxmf/vectors/gen_telemetry_vectors.py

Output: ``telemetry_vectors.json`` next to this script. The file is committed;
re-running MUST reproduce it byte for byte.

What each vector proves
-----------------------
Capture vectors (``packed_hex``) are the genuine output of
``Telemeter.packed()`` at the pinned commit, and every capture is fed back
through ``Telemeter.from_packed`` here, asserting the decoded values against
the inputs — so a Rust test that (a) decodes ``packed_hex`` to the recorded
facts and (b) encodes the recorded facts back to ``packed_hex`` has proven
both directions against the reference. The recorded location integers are
read back OUT of the capture (``struct.unpack``), never recomputed from the
float inputs, so Python's rounding wobble cannot skew the expectation.

Derived vectors are hand-built maps for cases the origin cannot itself emit
(saturated wire bounds, Columba's two-entry map, tolerance rows); each states
its derivation in ``_derivation`` and is still round-tripped through
``from_packed`` where the reference accepts the shape.

Columba (`TelemeterCodec.kt`, commit 0930293) encodes with msgpack-java's
minimal-width encodings, which for the value domain used here (fixint SIDs,
uint32 timestamps, bin8 blobs, fixarray/fixmap) are byte-identical to
umsgpack's choices, so the Columba capture is derived with umsgpack following
`packLocationTelemetry` statement by statement.
"""

import json
import os
import struct
import sys
import time

SIDEBAND_COMMIT = "2000d81a44bff57e3b3cb7d45915ba29bbde4e18"
COLUMBA_COMMIT = "0930293"

sideband_path = os.environ.get("SIDEBAND_PATH")
if not sideband_path:
    print("SIDEBAND_PATH must name a Sideband checkout's sbapp directory", file=sys.stderr)
    sys.exit(1)
sys.path.insert(0, sideband_path)

from RNS.vendor import umsgpack  # noqa: E402

# Freeze the clock before sense.py can call it: Telemeter.packed() stamps
# SID_TIME from time.time() unconditionally.
FIXED_TIME = 1723722000
time.time = lambda: FIXED_TIME

from sideband.sense import Sensor, Telemeter  # noqa: E402

VECTORS = []


def sideband_capture(readings):
    """Pack the given sensor data dicts through the real Telemeter."""
    t = Telemeter()
    for name, data in readings.items():
        if name == "time":
            continue
        t.synthesize(name)
        t.sensors[name].data = data
    return t.packed()


def location_ints_from_capture(packed):
    """Read the wire's scaled integers back out of a capture."""
    loc = umsgpack.unpackb(packed)[Sensor.SID_LOCATION]
    return {
        "latitude_e6": struct.unpack("!i", loc[0])[0],
        "longitude_e6": struct.unpack("!i", loc[1])[0],
        "altitude_e2": struct.unpack("!i", loc[2])[0],
        "speed_e2": struct.unpack("!I", loc[3])[0],
        "bearing_e2": struct.unpack("!i", loc[4])[0],
        "accuracy_e2": struct.unpack("!H", loc[5])[0],
        "last_update": loc[6],
    }


def assert_from_packed(packed, expect):
    """Feed a capture to the reference decoder and check what it read."""
    t = Telemeter.from_packed(packed)
    assert t is not None, "reference rejected the capture outright"
    readings = t.read_all()
    for name, want in expect.items():
        got = readings.get(name)
        assert got == want, f"reference read {name}={got!r}, expected {want!r}"
    return readings


def vector(vid, **fields):
    VECTORS.append({"id": vid, **fields})


# ---------------------------------------------------------------------------
# VEC-TELE-TRACKER: the full multi-sensor tracker case. Time, location,
# battery, physical link, temperature and power production in one map, in
# ascending SID order (the codec's emit order).
# ---------------------------------------------------------------------------
tracker_location = {
    "latitude": 53.551086, "longitude": 9.993682, "altitude": 12.34,
    "speed": 1.25, "bearing": 271.5, "accuracy": 4.7,
    "last_update": FIXED_TIME,
}
tracker = sideband_capture({
    "location": tracker_location,
    "battery": {"charge_percent": 84.5, "charging": True, "temperature": None},
    "physical_link": {"rssi": -87, "snr": 5.5, "q": 92},
    "temperature": {"c": 21.5},
    "power_production": {"solar": [4.2, "solar-power"], 0x00: [0.5, None]},
})
tracker_ints = location_ints_from_capture(tracker)
assert_from_packed(tracker, {
    "time": {"utc": FIXED_TIME},
    "battery": {"charge_percent": 84.5, "charging": True, "temperature": None},
    "physical_link": {"rssi": -87, "snr": 5.5, "q": 92},
    "temperature": {"c": 21.5},
    "power_production": {"solar": [4.2, "solar-power"], 0x00: [0.5, None]},
})
vector(
    "VEC-TELE-TRACKER",
    packed_hex=tracker.hex(),
    time_utc=FIXED_TIME,
    **tracker_ints,
    battery_charge_percent=84.5,
    battery_charging=True,
    link_rssi=-87,
    link_snr=5.5,
    link_q=92,
    temperature_c=21.5,
    power_producers=2,
    power_0_label="solar",
    power_0_watts=4.2,
    power_0_icon="solar-power",
    power_1_watts=0.5,
)

# ---------------------------------------------------------------------------
# VEC-TELE-SOUTHWEST: sign edges — southern and western hemispheres,
# negative altitude, zero speed and bearing.
# ---------------------------------------------------------------------------
southwest = sideband_capture({
    "location": {
        "latitude": -33.045, "longitude": -71.6197, "altitude": -2.5,
        "speed": 0.0, "bearing": 0.0, "accuracy": 12.0,
        "last_update": FIXED_TIME,
    },
})
southwest_ints = location_ints_from_capture(southwest)
assert southwest_ints["latitude_e6"] < 0 and southwest_ints["longitude_e6"] < 0
assert southwest_ints["altitude_e2"] == -250
assert_from_packed(southwest, {"time": {"utc": FIXED_TIME}})
vector("VEC-TELE-SOUTHWEST", packed_hex=southwest.hex(), time_utc=FIXED_TIME, **southwest_ints)

# ---------------------------------------------------------------------------
# VEC-TELE-SATURATED: range edges the origin cannot emit — its own
# struct.pack raises above the field widths and Location.pack drops the
# reading to nil, while Columba clamps. Our encoder saturates (the Columba
# side), so the wire carries the exact field bounds; this derived map proves
# the reference DECODER accepts those bounds.
# ---------------------------------------------------------------------------
saturated_map = {
    Sensor.SID_TIME: FIXED_TIME,
    Sensor.SID_LOCATION: [
        struct.pack("!i", 53551086),
        struct.pack("!i", 9993682),
        struct.pack("!i", 1234),
        struct.pack("!I", 0xFFFFFFFF),  # speed saturated at the unsigned bound
        struct.pack("!i", 27150),
        struct.pack("!H", 0xFFFF),      # accuracy saturated above the 16-bit ceiling
        FIXED_TIME,
    ],
}
saturated = umsgpack.packb(saturated_map)
readings = assert_from_packed(saturated, {"time": {"utc": FIXED_TIME}})
assert readings["location"]["speed"] == 0xFFFFFFFF / 100
assert readings["location"]["accuracy"] == 0xFFFF / 100
vector(
    "VEC-TELE-SATURATED",
    _derivation="hand-built map at the exact wire bounds; accepted by Telemeter.from_packed",
    packed_hex=saturated.hex(),
    time_utc=FIXED_TIME,
    latitude_e6=53551086, longitude_e6=9993682, altitude_e2=1234,
    speed_e2=0xFFFFFFFF, bearing_e2=27150, accuracy_e2=0xFFFF,
    last_update=FIXED_TIME,
)

# ---------------------------------------------------------------------------
# VEC-TELE-NIL-READING: the second encoding of absence. An active sensor
# without data packs as SID -> nil ("sensor present, no reading"); a decoder
# that refuses it is defective.
# ---------------------------------------------------------------------------
nil_reading = sideband_capture({"location": None})
assert umsgpack.unpackb(nil_reading)[Sensor.SID_LOCATION] is None
t = Telemeter.from_packed(nil_reading)
assert t is not None and t.read("location") is None
vector("VEC-TELE-NIL-READING", packed_hex=nil_reading.hex(), time_utc=FIXED_TIME)

# ---------------------------------------------------------------------------
# VEC-TELE-UNKNOWN-SENSOR: a capture carrying a sensor this codec does not
# implement (pressure, SID 0x03). Skipped on read, never an error — the
# format's own extension mechanism.
# ---------------------------------------------------------------------------
unknown_sensor = sideband_capture({
    "pressure": {"mbar": 1013.25},
    "location": tracker_location,
})
unknown_ints = location_ints_from_capture(unknown_sensor)
assert unknown_ints == tracker_ints
assert_from_packed(unknown_sensor, {"pressure": {"mbar": 1013.25}})
vector("VEC-TELE-UNKNOWN-SENSOR", packed_hex=unknown_sensor.hex(), time_utc=FIXED_TIME, **unknown_ints)

# ---------------------------------------------------------------------------
# VEC-TELE-SHORT-BATTERY: the origin's Battery.unpack takes a two-element
# list (temperature optional) and passes `charging` through untyped; accept
# the same set.
# ---------------------------------------------------------------------------
short_battery = umsgpack.packb({Sensor.SID_TIME: FIXED_TIME, Sensor.SID_BATTERY: [96, True]})
assert_from_packed(short_battery, {
    "battery": {"charge_percent": 96, "charging": True, "temperature": None},
})
vector(
    "VEC-TELE-SHORT-BATTERY",
    _derivation="hand-built two-element battery; accepted by Battery.unpack's len guard",
    packed_hex=short_battery.hex(),
    battery_charge_percent=96,
    battery_charging=True,
)

# ---------------------------------------------------------------------------
# VEC-TELE-COLUMBA: what Columba's packLocationTelemetry (0930293) puts on
# the wire — a two-entry map, speed written through a SIGNED 4-byte put
# (clamped non-negative), accuracy clamped to 0xFFFF. Value domain chosen so
# msgpack-java's and umsgpack's encodings coincide (see module docstring).
# ---------------------------------------------------------------------------
columba_ts = FIXED_TIME
columba = umsgpack.packb({
    Sensor.SID_TIME: columba_ts,
    Sensor.SID_LOCATION: [
        struct.pack("!i", round(52.520008 * 1e6)),
        struct.pack("!i", round(13.404954 * 1e6)),
        struct.pack("!i", round(34.0 * 1e2)),
        struct.pack("!i", round(0.0 * 1e2)),   # signed put, non-negative by clamp
        struct.pack("!i", round(180.0 * 1e2)),
        struct.pack("!H", min(round(8.0 * 1e2), 0xFFFF)),
        columba_ts,
    ],
})
columba_ints = location_ints_from_capture(columba)
assert_from_packed(columba, {"time": {"utc": columba_ts}})
vector(
    "VEC-TELE-COLUMBA",
    _derivation="packLocationTelemetry statement by statement via umsgpack",
    packed_hex=columba.hex(),
    time_utc=columba_ts,
    **columba_ints,
)

# ---------------------------------------------------------------------------
# VEC-TELE-STREAM: a FIELD_TELEMETRY_STREAM value. The field value is a
# NATIVE msgpack list of rows (Sideband iterates lxm.fields[...] directly),
# each row [source_hash, timestamp, packed_telemetry, appearance] — always
# four elements, nil in the fourth, because Sideband's ingest indexes
# telemetry_entry[3] unconditionally. Row timestamps keep Sideband's stored
# families: one int, one float.
# ---------------------------------------------------------------------------
appearance = ["account", [0, 0, 0, 1], [1, 1, 1, 1]]  # SidebandCore.DEFAULT_APPEARANCE
stream_rows = [
    [bytes(range(0x10, 0x20)), FIXED_TIME, tracker, appearance],
    [bytes(range(0xA0, 0xB0)), FIXED_TIME + 61.25, southwest, None],
]
stream = umsgpack.packb(stream_rows)
assert umsgpack.unpackb(stream)[0][3] == appearance
vector(
    "VEC-TELE-STREAM",
    packed_hex=stream.hex(),
    row_0_source_hex=stream_rows[0][0].hex(),
    row_0_timestamp=FIXED_TIME,
    row_0_telemetry_hex=tracker.hex(),
    row_0_appearance_hex=umsgpack.packb(appearance).hex(),
    row_1_source_hex=stream_rows[1][0].hex(),
    row_1_timestamp=FIXED_TIME + 61.25,
    row_1_telemetry_hex=southwest.hex(),
)

# ---------------------------------------------------------------------------
# VEC-TELE-STREAM-TOLERANT: rows our decoder must survive that Sideband's
# would abort the whole message on. A three-element row is what Columba's
# NATIVE collector emits when appearance is absent (0930293); the
# two-element row and the non-array row are garbage that must cost one row,
# not the message.
# ---------------------------------------------------------------------------
tolerant = umsgpack.packb([
    [bytes(range(0x10, 0x20)), FIXED_TIME, southwest],  # Columba-native three-element row
    [bytes(range(0x20, 0x30)), FIXED_TIME],             # short garbage: dropped
    "not-a-row",                                        # non-array garbage: dropped
    [bytes(range(0x30, 0x40)), FIXED_TIME + 1, southwest, None],
])
vector(
    "VEC-TELE-STREAM-TOLERANT",
    _derivation="hand-built; three-element row is Columba's native emit shape",
    packed_hex=tolerant.hex(),
    row_0_source_hex=bytes(range(0x10, 0x20)).hex(),
    row_1_source_hex=bytes(range(0x30, 0x40)).hex(),
    surviving_rows=2,
)

# ---------------------------------------------------------------------------
# Determinism check, then write. Mirrors gen_vectors.py: every stored field
# must reproduce byte for byte on a re-run.
# ---------------------------------------------------------------------------
snapshot = {v["id"]: json.dumps(v, sort_keys=True) for v in VECTORS}
for v in VECTORS:
    assert json.dumps(v, sort_keys=True) == snapshot[v["id"]], f"non-deterministic vector {v['id']}"

doc = {
    "_comment": (
        "Golden vectors for the Telemeter codec (leviculum-lxmf/src/telemetry.rs). "
        "Generated by gen_telemetry_vectors.py against the REAL Sideband sense.py; "
        "do not edit by hand; re-run the harness."
    ),
    "meta": {
        "sideband_commit": SIDEBAND_COMMIT,
        "columba_commit": COLUMBA_COMMIT,
        "fixed_timestamp": FIXED_TIME,
    },
    "vectors": VECTORS,
}
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "telemetry_vectors.json")
with open(out, "w") as f:
    json.dump(doc, f, indent=2, sort_keys=False)
    f.write("\n")
print(f"wrote {out} with {len(VECTORS)} vectors, all verified against Telemeter.from_packed")
