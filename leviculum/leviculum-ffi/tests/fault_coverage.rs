//! Guards that the fault surface stays fully exercised.
//!
//! A failure path that no test triggers is a path nobody knows works. This
//! source-level check enforces that every `LEV_ERR_*` code and every
//! failure-class `LEV_EVENT_*` is named by at least one test under `tests/`,
//! or is explicitly allowlisted with a reason. Adding a new error code or
//! failure event without a triggering test fails here, the same way
//! `guard_coverage.rs` and `event_projection_coverage.rs` enforce their
//! invariants.
//!
//! "Named by a test" is an approximation (a grep, like the sibling guards):
//! it catches the forgotten-stub case (a code/event no test mentions at all),
//! not the subtler case of a mention without a real assertion.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Error codes not triggerable by an in-process test, each with its reason.
const UNTRIGGERABLE_CODES: &[(&str, &str)] = &[(
    "LEV_ERR_PANIC",
    "caught FFI-boundary panic; structurally guaranteed by guard_coverage.rs, \
     not reachable without a library bug",
)];

/// The failure / adverse-condition events. Each must be triggered by a test
/// or listed in `UNTRIGGERABLE_EVENTS`. This list is curated (the guard cannot
/// infer which events are failures), so a new failure event must be added here
/// AND given a triggering test.
const FAILURE_EVENTS: &[&str] = &[
    "LEV_EVENT_LINK_CLOSED",
    "LEV_EVENT_CONTROL_OVERFLOW",
    "LEV_EVENT_REQUEST_TIMEOUT",
    "LEV_EVENT_RESOURCE_FAILED",
    "LEV_EVENT_LINK_STALE",
    "LEV_EVENT_LINK_RECOVERED",
    "LEV_EVENT_PATH_LOST",
    "LEV_EVENT_DELIVERY_FAILED",
    "LEV_EVENT_LINK_DELIVERY_FAILED",
];

/// Failure events not triggerable cleanly in-process, each with its reason.
const UNTRIGGERABLE_EVENTS: &[(&str, &str)] = &[
    (
        "LEV_EVENT_DELIVERY_FAILED",
        "needs a peer that answers with an unverifiable proof, or a receipt left \
         to expire; the C API exposes no way to make either happen on demand. \
         The engine side is covered instead, by leviculum-core's \
         an_unverifiable_proof_is_invalid_proof_not_link_failed, and the \
         projection of its discriminant by \
         every_delivery_error_reaches_c_as_its_own_constant. (The earlier reason \
         here said engine #76 mis-fires this on VALID proofs — that was fixed by \
         the recalled-identity fallback in node/mod.rs and had gone stale.)",
    ),
    (
        "LEV_EVENT_LINK_DELIVERY_FAILED",
        "needs a link packet's receipt left to expire past its RTT-derived \
         deadline (max(rtt * 6, TRAFFIC_TIMEOUT_MIN_MS)); the C API exposes no \
         clock control to make that happen on demand, the same limitation as \
         LEV_EVENT_DELIVERY_FAILED above. The engine side is covered by \
         leviculum-core's raw_link_receipt_deadline_is_derived_from_rtt, \
         raw_link_receipt_can_outlive_the_old_fixed_timeout, and \
         late_raw_link_proof_is_ignored_after_rtt_deadline.",
    ),
    (
        "LEV_EVENT_LINK_RECOVERED",
        "a stale link recovers only within the engine's fixed few-second \
         stale-before-close grace window; hitting it reliably races timer \
         scheduling and cannot be made deterministic in-process. LINK_STALE, \
         the failure half of the pair, is triggered by silence_makes_the_link_\
         go_stale",
    ),
];

fn read(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Every `pub const LEV_ERR_*` name declared in `error.rs`.
fn error_codes() -> BTreeSet<String> {
    let src = read("src/error.rs");
    src.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("pub const ")?;
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            name.starts_with("LEV_ERR_").then_some(name)
        })
        .collect()
}

/// Names referenced anywhere under `tests/`, excluding this guard file (so the
/// guard's own allowlists do not count as coverage).
fn names_used_in_tests() -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut combined = String::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let entries =
            std::fs::read_dir(&d).unwrap_or_else(|e| panic!("read_dir {}: {e}", d.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            if path.file_name().is_some_and(|n| n == "fault_coverage.rs") {
                continue;
            }
            combined.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            combined.push('\n');
        }
    }
    combined
}

#[test]
fn every_error_code_is_exercised_or_allowlisted() {
    let codes = error_codes();
    assert!(
        codes.len() >= 15,
        "parsed only {} LEV_ERR_* codes — the parser likely drifted",
        codes.len()
    );
    let tests = names_used_in_tests();
    let allow: BTreeSet<&str> = UNTRIGGERABLE_CODES.iter().map(|(n, _)| *n).collect();

    let mut missing: Vec<String> = codes
        .iter()
        .filter(|c| !tests.contains(c.as_str()) && !allow.contains(c.as_str()))
        .cloned()
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "error codes named by no test (add a triggering test, or an \
         UNTRIGGERABLE_CODES entry with a reason): {missing:?}"
    );

    // Honest allowlist: every entry must name a real code.
    let stale: Vec<&str> = allow
        .iter()
        .copied()
        .filter(|c| !codes.contains(*c))
        .collect();
    assert!(
        stale.is_empty(),
        "stale UNTRIGGERABLE_CODES entries: {stale:?}"
    );
}

#[test]
fn every_failure_event_is_exercised_or_allowlisted() {
    let events_src = read("src/events.rs");
    // The curated failure list must only name real event constants.
    let undefined: Vec<&str> = FAILURE_EVENTS
        .iter()
        .filter(|e| !events_src.contains(&format!("pub const {e}")))
        .copied()
        .collect();
    assert!(
        undefined.is_empty(),
        "FAILURE_EVENTS names no such LEV_EVENT_* constant: {undefined:?}"
    );

    let tests = names_used_in_tests();
    let allow: BTreeSet<&str> = UNTRIGGERABLE_EVENTS.iter().map(|(n, _)| *n).collect();

    let mut missing: Vec<&str> = FAILURE_EVENTS
        .iter()
        .filter(|e| !tests.contains(**e) && !allow.contains(**e))
        .copied()
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "failure events triggered by no test (add a triggering test, or an \
         UNTRIGGERABLE_EVENTS entry with a reason): {missing:?}"
    );

    // Honest allowlist: every entry must be a curated failure event.
    let listed: BTreeSet<&str> = FAILURE_EVENTS.iter().copied().collect();
    let stale: Vec<&str> = allow
        .iter()
        .copied()
        .filter(|e| !listed.contains(*e))
        .collect();
    assert!(
        stale.is_empty(),
        "stale UNTRIGGERABLE_EVENTS entries (not in FAILURE_EVENTS): {stale:?}"
    );
}
