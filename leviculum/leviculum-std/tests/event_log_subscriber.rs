//! Phase-A unit tests for the structured event-log subscriber.
//! See `leviculum-std/src/test_support/event_log.rs`.
//!
//! # Cross-test pollution
//!
//! The subscriber lives behind a process-global tracing layer.  Every
//! active `EventLogHandle` receives every event the layer sees,
//! regardless of which test emitted it.  Two things keep that
//! deterministic.
//!
//! Where a test can choose its own event name it uses a **disjoint**
//! one (`EV_BASIC`, `EV_VIOLATION` (= `PKT_RX` since the production
//! catalogue is needed for the schema-violation test), `EV_UNKNOWN`,
//! `EV_PANIC`, `EV_MACRO`, `EV_WS`) and filters the dump to lines that
//! reference its own event name before asserting.
//!
//! Disjoint names are not available to every test: the `test_catalogue_*`
//! family and `obs3_endpoint_events_clean_under_layer` assert on the
//! *production* catalogue entries, so they necessarily share names —
//! and they disagree about them, because the catalogue tests emit a
//! deliberately malformed event to prove the violation fires while the
//! OBS-3 test asserts those same names are violation-free.  Filtering
//! by name cannot separate the two.  Every test therefore takes
//! [`lock_event_log`] before creating its handle, so no two handles are
//! ever active at once.

use std::panic;
use std::sync::{Mutex, MutexGuard, PoisonError};

use leviculum_std::test_support::event_log::{
    init_event_log, init_event_log_to_file, init_event_log_with_extra_schemas, EventLogHandle,
    EventSchema,
};

/// Serialises the tests in this file.
///
/// A handle is registered when it is created and removed when it is
/// dropped (`leviculum-std/src/event_log.rs:382`, `:344`), so it
/// captures exactly the events emitted during its lifetime.  Holding
/// this lock across a test's handle therefore makes the buffer contain
/// that test's events and nothing else.
///
/// Take it *before* creating the handle and bind the guard first, so
/// the handle — declared later — drops first and deregisters while the
/// lock is still held.
///
/// Poison is recovered rather than propagated: several tests panic on
/// purpose, and a poisoned lock would turn one deliberate panic into 25
/// unrelated failures.
static EVENT_LOG_LOCK: Mutex<()> = Mutex::new(());

fn lock_event_log() -> MutexGuard<'static, ()> {
    EVENT_LOG_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Test-only catalogue extension for `test_assert_no_schema_violations_macro_red`.
/// Per-handle (lives in the handle's `extra_schemas`); other handles
/// don't see it.
const TEST_MACRO_SCHEMAS: &[EventSchema] = &[EventSchema {
    name: "EV_MACRO",
    required_keys: &["k1", "k2"],
}];

/// Filter `handle.dump()` for lines that concern the named event:
/// either the canonical event line (starts with `"EVENT_NAME "`) or
/// a `*_VIOLATION` line that names the event in its `event=...`
/// field.
fn lines_for(handle: &EventLogHandle, event_name: &str) -> Vec<String> {
    let event_prefix = format!("{event_name} ");
    let event_kv = format!("event={event_name} ");
    handle
        .dump()
        .into_iter()
        .filter(|l| l.starts_with(&event_prefix) || l.contains(&event_kv))
        .collect()
}

/// Test 1 — basic format.  Three `EV_BASIC` events with the PKT_RX-
/// shape field set; assert format on each.  EV_BASIC is not in the
/// production catalogue, so no schema check fires.
#[test]
fn test_basic_event_format() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    for _ in 0..3 {
        tracing::debug!(
            event = "EV_BASIC",
            iface = "tcp0",
            r#type = "Data",
            dst = "ab",
            hops = 1u8,
            len = 64usize,
        );
    }
    let lines = lines_for(&handle, "EV_BASIC");
    assert_eq!(
        lines.len(),
        3,
        "expected 3 EV_BASIC lines, got {}: {lines:?}",
        lines.len()
    );
    for line in &lines {
        assert!(
            line.starts_with("EV_BASIC node=local "),
            "wrong prefix: {line}"
        );
        assert!(
            line.contains(" dst=ab ")
                && line.contains(" hops=1 ")
                && line.contains(" iface=tcp0 ")
                && line.contains(" len=64 ")
                && line.contains(" type=Data "),
            "missing canonical key=value: {line}",
        );
        // node first; then alphabetical: dst < hops < iface < len < type; t last.
        let node_pos = line.find("node=").unwrap();
        let dst_pos = line.find("dst=").unwrap();
        let hops_pos = line.find("hops=").unwrap();
        let iface_pos = line.find("iface=").unwrap();
        let len_pos = line.find("len=").unwrap();
        let type_pos = line.find("type=").unwrap();
        let t_pos = line.rfind(" t=").unwrap();
        assert!(
            node_pos < dst_pos
                && dst_pos < hops_pos
                && hops_pos < iface_pos
                && iface_pos < len_pos
                && len_pos < type_pos
                && type_pos < t_pos,
            "non-canonical key order in: {line}",
        );
    }
}

/// Test 2 — schema violation.  PKT_RX is the production-catalogue
/// entry; emit a PKT_RX missing `hops` and `len` and assert the
/// synthetic `EVENT_SCHEMA_VIOLATION` line is appended.
#[test]
fn test_schema_violation_emitted() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "PKT_RX",
        iface = "tcp0",
        r#type = "Data",
        dst = "ab",
        ph = "00112233aabbccdd",
        // hops + len intentionally missing
    );
    let lines = lines_for(&handle, "PKT_RX");
    // 1 event line + 1 violation line referencing PKT_RX.
    assert_eq!(
        lines.len(),
        2,
        "expected 2 PKT_RX-related lines, got {}: {lines:?}",
        lines.len(),
    );
    assert!(
        lines[0].starts_with("PKT_RX node=local "),
        "first line should be the event line: {}",
        lines[0]
    );
    let violation = &lines[1];
    assert!(
        violation.starts_with("EVENT_SCHEMA_VIOLATION event=PKT_RX "),
        "violation prefix wrong: {violation}",
    );
    assert!(
        violation.contains("missing=[hops,len]"),
        "missing list wrong: {violation}",
    );
    assert!(
        violation.contains("caller=event_log_subscriber.rs:"),
        "caller field wrong: {violation}",
    );
}

/// Test 3 — unknown events pass through.  An event whose name is
/// not in the catalogue gets emitted as-is, no schema check fires,
/// no field-value violation expected.
#[test]
fn test_unknown_event_passes_through() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(event = "EV_UNKNOWN", x = 1u32);
    let lines = lines_for(&handle, "EV_UNKNOWN");
    assert_eq!(
        lines.len(),
        1,
        "expected exactly 1 EV_UNKNOWN line, got: {lines:?}"
    );
    assert!(
        lines[0].starts_with("EV_UNKNOWN node=local "),
        "wrong prefix: {}",
        lines[0]
    );
    assert!(lines[0].contains("x=1"), "missing field: {}", lines[0]);
}

/// Test 4 — drop on panic.  A handle that goes out of scope while
/// the thread is panicking dumps the buffer to the configured target.
/// Use the file-target variant so we can read the dump back
/// deterministically.
#[test]
fn test_dump_on_panic() {
    // Outside the catch_unwind: the guard must not live in the frame
    // that unwinds, or the deliberate panic would drop it before the
    // handle it protects.
    let _lock = lock_event_log();
    let dump_path = std::env::temp_dir().join(format!("event-log-dump-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&dump_path);

    let dump_path_for_panic = dump_path.clone();
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let _handle = init_event_log_to_file(dump_path_for_panic);
        tracing::debug!(event = "EV_PANIC", note = "before_panic");
        panic!("intentional panic for test_dump_on_panic");
    }));
    assert!(result.is_err(), "panic was expected to propagate");

    let dumped = std::fs::read_to_string(&dump_path)
        .expect("dump file should have been written by the Drop");
    assert!(
        dumped.contains("=== EVENT LOG DUMP"),
        "missing banner: {dumped}",
    );
    assert!(
        dumped.contains("EV_PANIC node=local note=before_panic"),
        "missing event line: {dumped}",
    );

    let _ = std::fs::remove_file(&dump_path);
}

/// Test 5 — `assert_no_schema_violations!` macro.  Emits an EV_MACRO
/// event missing required keys (the test handle adds an EV_MACRO
/// schema via extra_schemas; other handles don't see it).  Macro
/// catches the violation and panics with a count message.
#[test]
fn test_assert_no_schema_violations_macro_red() {
    // See test_dump_on_panic: the guard stays outside the unwinding frame.
    let _lock = lock_event_log();
    let result = panic::catch_unwind(|| {
        let handle = init_event_log_with_extra_schemas(TEST_MACRO_SCHEMAS, None);
        // EV_MACRO requires k1 and k2; we send only k1 → schema violation
        // appears in this handle's buffer (because EV_MACRO is in this
        // handle's extra_schemas).
        tracing::debug!(event = "EV_MACRO", k1 = "v1");
        leviculum_std::assert_no_schema_violations!(handle);
    });
    let err = result.expect_err("the macro should have panicked on the violation");
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&'static str>().copied())
        .unwrap_or("<non-string panic payload>");
    // Cross-test pollution makes the exact violation count and the
    // identity of the "first" violation non-deterministic — every
    // active handle sees every event the global layer captures (see
    // module-level docs in `test_support::event_log`). The contract
    // we can assert reliably is that the macro panicked AND that
    // EV_MACRO appears as a violation somewhere in the panic
    // surface — but `panic!` only renders the first violation, so
    // we relax to: panic occurred + message starts with the
    // expected prefix.
    assert!(
        msg.starts_with("schema violations:"),
        "panic message should start with violations prefix, got: {msg}",
    );
}

/// Catalogue extension with one event name declared under TWO shapes
/// (the `RNODE_TX_QUEUE_DROP` situation, Codeberg #320: `len` bytes on
/// `queue_full`, `frames` on the abandon reasons).  Per-handle via
/// extra_schemas so the production catalogue stays untouched.
const TWO_SHAPE_SCHEMAS: &[EventSchema] = &[
    EventSchema {
        name: "EV_TWO_SHAPE",
        required_keys: &["iface", "len", "reason"],
    },
    EventSchema {
        name: "EV_TWO_SHAPE",
        required_keys: &["iface", "frames", "reason"],
    },
];

/// Test 5b — an event name catalogued under several shapes passes when
/// ANY declared shape is fully present, and still violates when none
/// is.  The violation reports the nearest shape (fewest missing keys),
/// so the message stays actionable.
#[test]
fn test_multi_shape_catalogue_accepts_either_shape() {
    let _lock = lock_event_log();
    let handle = init_event_log_with_extra_schemas(TWO_SHAPE_SCHEMAS, None);

    // Shape 1 (len) — no violation may fire.
    tracing::debug!(
        event = "EV_TWO_SHAPE",
        iface = "if0",
        len = 42u64,
        reason = "queue_full"
    );
    // Shape 2 (frames) — no violation may fire either; under a
    // first-entry-only lookup this one would be flagged for a
    // missing `len`.
    tracing::debug!(
        event = "EV_TWO_SHAPE",
        iface = "if0",
        frames = 3u64,
        reason = "serial_eof"
    );

    let lines = lines_for(&handle, "EV_TWO_SHAPE");
    let violations: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("EVENT_SCHEMA_VIOLATION"))
        .collect();
    assert!(
        violations.is_empty(),
        "either declared shape must satisfy the check, got: {violations:?}"
    );

    // Neither shape — exactly one violation, naming the nearest miss
    // (both shapes lack one key here, so the first declared wins).
    tracing::debug!(event = "EV_TWO_SHAPE", iface = "if0", reason = "queue_full");
    let lines = lines_for(&handle, "EV_TWO_SHAPE");
    let violations: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("EVENT_SCHEMA_VIOLATION"))
        .collect();
    assert_eq!(
        violations.len(),
        1,
        "a record matching no declared shape must still violate: {violations:?}"
    );
    assert!(
        violations[0].contains("missing=[len]"),
        "the violation should name the nearest shape's missing keys: {}",
        violations[0]
    );
}

/// Test 6 — field-value violations.  Field values containing
/// whitespace, `=`, or non-printable characters trigger an
/// `EVENT_FIELD_VIOLATION` line in addition to the original event.
#[test]
fn test_field_value_whitespace_violation() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(event = "EV_WS", note = "has space");
    tracing::debug!(event = "EV_WS", extra = "key=val");
    tracing::debug!(event = "EV_WS", clean = "ok");

    let lines = lines_for(&handle, "EV_WS");
    // 3 event lines + 2 violation lines (one per offending field).
    assert_eq!(
        lines.len(),
        5,
        "expected 5 EV_WS lines (3 events + 2 violations), got: {lines:?}",
    );

    let violations: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("EVENT_FIELD_VIOLATION"))
        .collect();
    assert_eq!(
        violations.len(),
        2,
        "expected exactly 2 EVENT_FIELD_VIOLATION lines, got: {violations:?}",
    );

    let v_ws = violations
        .iter()
        .find(|l| l.contains("field=note") && l.contains("value_problem=whitespace"))
        .unwrap_or_else(|| panic!("missing whitespace violation: {violations:?}"));
    assert!(
        v_ws.contains("event=EV_WS"),
        "wrong event in violation: {v_ws}",
    );
    assert!(
        v_ws.contains("caller=event_log_subscriber.rs:"),
        "missing caller: {v_ws}",
    );

    let v_eq = violations
        .iter()
        .find(|l| l.contains("field=extra") && l.contains("value_problem=equals"))
        .unwrap_or_else(|| panic!("missing equals violation: {violations:?}"));
    assert!(
        v_eq.contains("event=EV_WS"),
        "wrong event in violation: {v_eq}",
    );
}

/// BUG-3 — well-formed by construction. A field value containing BOTH
/// whitespace AND an embedded `=` must be sanitized to a single scalar
/// so the canonical line tokenizes cleanly: every key=val token has
/// exactly one `=`, the field appears exactly once, and the embedded `=`
/// does not leak a second (colliding) key. The advisory
/// EVENT_FIELD_VIOLATION must still fire (it surfaces the source bug).
#[test]
fn test_field_value_sanitized_to_scalar() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "EV_SANI",
        msg = "froze remaining_hops=path_hops for forwarded link request"
    );

    let lines = lines_for(&handle, "EV_SANI");
    let canonical = lines
        .iter()
        .find(|l| l.starts_with("EV_SANI "))
        .unwrap_or_else(|| panic!("missing EV_SANI canonical line: {lines:?}"));

    // Every token after the event name is a well-formed key=val.
    for tok in canonical.split_whitespace().skip(1) {
        assert_eq!(
            tok.matches('=').count(),
            1,
            "non key=val token {tok:?} in line: {canonical}"
        );
    }
    // The msg field is a single scalar (no stray bare tokens).
    let msg_tokens: Vec<&str> = canonical
        .split_whitespace()
        .filter(|t| t.starts_with("msg="))
        .collect();
    assert_eq!(
        msg_tokens.len(),
        1,
        "msg must be a single scalar token: {canonical}"
    );
    // The embedded `=` must NOT have leaked a colliding remaining_hops key.
    assert!(
        !canonical.contains("remaining_hops="),
        "embedded `=` leaked a colliding key: {canonical}"
    );

    // The violation still fires so the source bug is surfaced.
    assert!(
        lines.iter().any(|l| l.starts_with("EVENT_FIELD_VIOLATION")
            && l.contains("event=EV_SANI")
            && l.contains("field=msg")),
        "EVENT_FIELD_VIOLATION must still fire for the offending field: {lines:?}"
    );
}

/// #113 positive — a name-type field (`iface`) whose value carries
/// legitimate whitespace (auto-connect names an interface after a
/// discovered node, e.g. `autoconnect/Dark Doodad 23`) is coerced into a
/// tokenizable scalar for the line but emits NO `EVENT_FIELD_VIOLATION`.
/// This is the false-positive flood that poisoned the miauhaus self-alarm.
#[test]
fn test_name_field_whitespace_no_violation() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(event = "EV_IFACE", iface = "autoconnect/Dark Doodad 23");

    let lines = lines_for(&handle, "EV_IFACE");
    let canonical = lines
        .iter()
        .find(|l| l.starts_with("EV_IFACE "))
        .unwrap_or_else(|| panic!("missing EV_IFACE canonical line: {lines:?}"));

    // The line is still well-formed: iface is a single scalar token with
    // the whitespace coerced away.
    let iface_tokens: Vec<&str> = canonical
        .split_whitespace()
        .filter(|t| t.starts_with("iface="))
        .collect();
    assert_eq!(
        iface_tokens.len(),
        1,
        "iface must be a single scalar token: {canonical}"
    );
    assert!(
        canonical.contains("iface=autoconnect/Dark_Doodad_23"),
        "iface whitespace should be coerced to `_`: {canonical}"
    );

    // The crux of #113: NO violation for the legitimate spaced name.
    assert!(
        !lines.iter().any(|l| l.starts_with("EVENT_FIELD_VIOLATION")),
        "name-type field must not emit EVENT_FIELD_VIOLATION: {lines:?}"
    );
}

/// #113 negative — the detector keeps its BUG-1 value. A genuinely
/// structured field (not a name field) with unexpected whitespace STILL
/// emits `EVENT_FIELD_VIOLATION`, so real freetext-leak bugs are still
/// surfaced.
#[test]
fn test_structured_field_whitespace_still_violates() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(event = "EV_STRUCT", reason = "no route found");

    let lines = lines_for(&handle, "EV_STRUCT");
    assert!(
        lines.iter().any(|l| l.starts_with("EVENT_FIELD_VIOLATION")
            && l.contains("event=EV_STRUCT")
            && l.contains("field=reason")
            && l.contains("value_problem=whitespace")),
        "structured field with whitespace must still emit a violation: {lines:?}"
    );
}

/// BUG-2 — `Debug`-recorded values render as bare scalars. An
/// `Option`-wrapped value must not leak Rust Debug wrapper syntax
/// (`Some("…")`) or quotes into the line. `None` stays the bare scalar
/// `None` (legitimate enum variants are also named `None`).
#[test]
fn test_record_debug_renders_bare_scalar() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "EV_OPT",
        next_hop = ?Some("373efabc"),
        gone = ?Option::<&str>::None,
    );

    let lines = lines_for(&handle, "EV_OPT");
    let canonical = lines
        .iter()
        .find(|l| l.starts_with("EV_OPT "))
        .unwrap_or_else(|| panic!("missing EV_OPT canonical line: {lines:?}"));

    assert!(
        canonical.contains(" next_hop=373efabc "),
        "expected bare scalar next_hop=373efabc: {canonical}"
    );
    assert!(
        !canonical.contains("Some("),
        "Debug Some(...) wrapper leaked: {canonical}"
    );
    assert!(!canonical.contains('"'), "Debug quote leaked: {canonical}");
    assert!(
        canonical.contains(" gone=None "),
        "None must stay the bare scalar None: {canonical}"
    );
    // No field violation: both values are already clean scalars.
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("EVENT_FIELD_VIOLATION") && l.contains("event=EV_OPT")),
        "no EVENT_FIELD_VIOLATION expected for clean scalars: {lines:?}"
    );
}

/// Test — `SILENCE_LNODE_ENTER` and `SILENCE_LNODE_EXIT` (Codeberg #50
/// Bug-A forensic events) emit through the catalogue with their
/// required keys and trigger an `EVENT_SCHEMA_VIOLATION` when a
/// required key is missing.
#[test]
fn test_silence_lnode_events_in_catalogue() {
    let _lock = lock_event_log();
    let handle = init_event_log();

    // Happy path — both events with all required keys.
    tracing::debug!(
        event = "SILENCE_LNODE_ENTER",
        usb_serial = "DEC9947DAD9D2869",
        port_path = "/dev/ttyACM6",
    );
    tracing::debug!(
        event = "SILENCE_LNODE_EXIT",
        usb_serial = "DEC9947DAD9D2869",
        port_path = "/dev/ttyACM6",
        result = "acked",
    );
    // Schema-violation path — EXIT missing `result` key.
    tracing::debug!(
        event = "SILENCE_LNODE_EXIT",
        usb_serial = "ABFAB3F1807E459B",
        port_path = "/dev/ttyACM4",
    );

    let enter = lines_for(&handle, "SILENCE_LNODE_ENTER");
    assert_eq!(enter.len(), 1, "ENTER lines: {enter:?}");
    assert!(enter[0].starts_with("SILENCE_LNODE_ENTER node=local "));
    assert!(enter[0].contains(" port_path=/dev/ttyACM6 "));
    assert!(
        enter[0].contains(" usb_serial=DEC9947DAD9D2869 "),
        "missing usb_serial: {}",
        enter[0]
    );

    let exit = lines_for(&handle, "SILENCE_LNODE_EXIT");
    // Three lines: 1 valid SILENCE_LNODE_EXIT + 1 invalid + 1
    // EVENT_SCHEMA_VIOLATION for the invalid one.
    assert_eq!(exit.len(), 3, "EXIT lines: {exit:?}");
    let valid = exit
        .iter()
        .find(|l| l.starts_with("SILENCE_LNODE_EXIT") && l.contains("result=acked"))
        .unwrap_or_else(|| panic!("missing valid EXIT: {exit:?}"));
    assert!(valid.contains(" port_path=/dev/ttyACM6 "));

    let violation = exit
        .iter()
        .find(|l| l.starts_with("EVENT_SCHEMA_VIOLATION"))
        .unwrap_or_else(|| panic!("missing schema violation: {exit:?}"));
    assert!(
        violation.contains("event=SILENCE_LNODE_EXIT"),
        "wrong event: {violation}"
    );
    assert!(
        violation.contains("missing=[result]"),
        "wrong missing keys: {violation}"
    );
}

// Stage-6 catalogue expansion (Codeberg #50 P2 follow-up).  One test
// per newly-added catalogue entry: emit with all required keys, then
// emit missing one, assert the canonical line + the schema-violation
// line for the missing-key case.

fn assert_catalogue_round_trip(
    handle: &EventLogHandle,
    event_name: &str,
    expected_required_keys: &[&str],
) {
    let lines = lines_for(handle, event_name);
    // Must contain at least the canonical line emitted with all
    // required keys plus a schema violation for the line we emitted
    // with one missing key.
    let canonical = lines
        .iter()
        .find(|l| l.starts_with(&format!("{event_name} ")))
        .unwrap_or_else(|| panic!("missing canonical line for {event_name}: {lines:?}"));
    for k in expected_required_keys {
        assert!(
            canonical.contains(&format!(" {k}=")),
            "canonical line for {event_name} missing key '{k}': {canonical}"
        );
    }
    let violation = lines
        .iter()
        .find(|l| {
            l.starts_with("EVENT_SCHEMA_VIOLATION") && l.contains(&format!("event={event_name} "))
        })
        .unwrap_or_else(|| panic!("missing schema violation for {event_name}: {lines:?}"));
    assert!(
        violation.contains("missing="),
        "violation has no missing= field: {violation}"
    );
}

#[test]
fn test_catalogue_emb_evict() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "EMB_EVICT",
        map = "destinations",
        len_before = 32_usize,
        cap = 32_usize
    );
    tracing::debug!(event = "EMB_EVICT", map = "destinations"); // missing len_before, cap
    assert_catalogue_round_trip(&handle, "EMB_EVICT", &["cap", "len_before", "map"]);
}

#[test]
fn test_catalogue_emb_insert_fail() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "EMB_INSERT_FAIL",
        map = "paths",
        len_after_evict = 10_usize,
        cap = 16_usize
    );
    tracing::debug!(event = "EMB_INSERT_FAIL", map = "paths"); // missing len_after_evict, cap
    assert_catalogue_round_trip(
        &handle,
        "EMB_INSERT_FAIL",
        &["cap", "len_after_evict", "map"],
    );
}

#[test]
fn test_catalogue_identity() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::info!(
        event = "IDENTITY",
        node = "deadbeef00112233445566778899aabb"
    );
    tracing::info!(event = "IDENTITY"); // missing node
    assert_catalogue_round_trip(&handle, "IDENTITY", &["node"]);
}

#[test]
fn test_catalogue_path_lookup() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    // found=true site emits dst, found, hops, iface
    tracing::debug!(
        event = "PATH_LOOKUP",
        dst = "abc1",
        found = true,
        hops = 0_u8,
        iface = "lora0"
    );
    // found=false site emits dst, found only — still satisfies required keys
    tracing::debug!(event = "PATH_LOOKUP", dst = "abc2", found = false);
    // Missing-required test: drop both dst and found
    tracing::debug!(event = "PATH_LOOKUP", note = "incomplete");
    assert_catalogue_round_trip(&handle, "PATH_LOOKUP", &["dst", "found"]);
}

#[test]
fn test_catalogue_path_table() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(event = "PATH_TABLE", size = 5_usize);
    tracing::debug!(event = "PATH_TABLE"); // missing size
    assert_catalogue_round_trip(&handle, "PATH_TABLE", &["size"]);
}

#[test]
fn test_catalogue_path_table_entry() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "PATH_TABLE_ENTRY",
        dst = "abc1",
        hops = 1_u8,
        iface = "lora0",
        next_hop = "beta",
        expires_in_ms = 9000_u64,
    );
    tracing::debug!(event = "PATH_TABLE_ENTRY", dst = "abc1"); // missing hops, iface, next_hop, expires_in_ms
    assert_catalogue_round_trip(
        &handle,
        "PATH_TABLE_ENTRY",
        &["dst", "expires_in_ms", "hops", "iface", "next_hop"],
    );
}

#[test]
fn test_catalogue_proof_gen() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(event = "PROOF_GEN", for_pkt = "abcd", to_dst = "ef01");
    tracing::debug!(event = "PROOF_GEN", for_pkt = "abcd"); // missing to_dst
    assert_catalogue_round_trip(&handle, "PROOF_GEN", &["for_pkt", "to_dst"]);
}

#[test]
fn test_catalogue_proof_send() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(event = "PROOF_SEND", pkt = "abcd", iface = "lora0");
    tracing::debug!(event = "PROOF_SEND", pkt = "abcd"); // missing iface
    assert_catalogue_round_trip(&handle, "PROOF_SEND", &["iface", "pkt"]);
}

// OBS-3 (Codeberg #114): endpoint observability catalogue entries. One
// round-trip test each — emit with all required keys (no violation) then with
// one missing (schema violation) — proving the catalogue entry is wired.

#[test]
fn test_catalogue_link_local() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "LINK_LOCAL",
        link = "aabb",
        dst = "ccdd",
        iface = "lora0",
    );
    tracing::debug!(event = "LINK_LOCAL", link = "aabb"); // missing dst, iface
    assert_catalogue_round_trip(&handle, "LINK_LOCAL", &["dst", "iface", "link"]);
}

#[test]
fn test_catalogue_request_rx() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "REQUEST_RX",
        link = "aabb",
        path_hash = "1122",
        request_id = "3344",
    );
    tracing::debug!(event = "REQUEST_RX", link = "aabb"); // missing path_hash, request_id
    assert_catalogue_round_trip(&handle, "REQUEST_RX", &["link", "path_hash", "request_id"]);
}

#[test]
fn test_catalogue_response_tx() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "RESPONSE_TX",
        link = "aabb",
        request_id = "3344",
        len = 12_usize,
    );
    tracing::debug!(event = "RESPONSE_TX", link = "aabb"); // missing request_id, len
    assert_catalogue_round_trip(&handle, "RESPONSE_TX", &["len", "link", "request_id"]);
}

/// Schema-health: emit every OBS-3 endpoint event with the EXACT keys the
/// production sites (transport.rs / node/link_management.rs / node/mod.rs) pass,
/// and assert none of them produces an EVENT_SCHEMA_VIOLATION (a required key is
/// missing -> undeclared/mis-declared) or EVENT_FIELD_VIOLATION (a value is not
/// a scalar token). Reused names (PROOF_GEN/PROOF_SEND/PKT_LOCAL) are included
/// because #114 wires them from new call sites.
#[test]
fn obs3_endpoint_events_clean_under_layer() {
    let _lock = lock_event_log();
    let handle = init_event_log();

    tracing::debug!(
        event = "LINK_LOCAL",
        link = "aabb",
        dst = "ccdd",
        iface = "lora0"
    );
    tracing::debug!(event = "PROOF_GEN", for_pkt = "aabb", to_dst = "ccdd");
    tracing::debug!(event = "PROOF_SEND", pkt = "aabb", iface = "lora0");
    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "ccdd",
        iface = "lora0",
        matched = true
    );
    tracing::debug!(
        event = "REQUEST_RX",
        link = "aabb",
        path_hash = "1122",
        request_id = "3344"
    );
    tracing::debug!(
        event = "RESPONSE_TX",
        link = "aabb",
        request_id = "3344",
        len = 12_usize
    );

    let dump = handle.dump();
    for name in [
        "LINK_LOCAL",
        "PROOF_GEN",
        "PROOF_SEND",
        "PKT_LOCAL",
        "REQUEST_RX",
        "RESPONSE_TX",
    ] {
        // The canonical line must be present...
        assert!(
            dump.iter().any(|l| l.starts_with(&format!("{name} "))),
            "missing canonical line for {name}"
        );
        // ...and no violation line may reference it.
        for l in &dump {
            if l.starts_with("EVENT_SCHEMA_VIOLATION") || l.starts_with("EVENT_FIELD_VIOLATION") {
                assert!(
                    !l.contains(&format!("event={name} ")),
                    "OBS-3 event {name} produced a violation: {l}"
                );
            }
        }
    }
}

#[test]
fn test_catalogue_reverse_add() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::debug!(
        event = "REVERSE_ADD",
        pkt_hash = "deadbeef",
        in_iface = "lora0",
        out_iface = "tcp1",
    );
    tracing::debug!(event = "REVERSE_ADD", pkt_hash = "deadbeef"); // missing in_iface, out_iface
    assert_catalogue_round_trip(
        &handle,
        "REVERSE_ADD",
        &["in_iface", "out_iface", "pkt_hash"],
    );
}

#[test]
fn test_catalogue_event_channel_full() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::warn!(
        event = "EVENT_CHANNEL_FULL",
        queue_capacity = 256_usize,
        dropped_event_type = "AnnounceReceived",
    );
    tracing::warn!(event = "EVENT_CHANNEL_FULL", queue_capacity = 256_usize); // missing dropped_event_type
    assert_catalogue_round_trip(
        &handle,
        "EVENT_CHANNEL_FULL",
        &["dropped_event_type", "queue_capacity"],
    );
}

#[test]
fn test_catalogue_event_channel_closed() {
    let _lock = lock_event_log();
    let handle = init_event_log();
    tracing::warn!(
        event = "EVENT_CHANNEL_CLOSED",
        dropped_event_type = "LinkClosed",
    );
    tracing::warn!(event = "EVENT_CHANNEL_CLOSED"); // missing dropped_event_type
    assert_catalogue_round_trip(&handle, "EVENT_CHANNEL_CLOSED", &["dropped_event_type"]);
}
