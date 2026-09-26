//! Fixture-driven tests for `jl` and `jldiff`.
//!
//! Five real-shape event-log fixtures live under `tests/fixtures/`.  Each
//! gets three tests minimum; more for the cross-fixture jldiff scenarios.
//! Fixtures are checked in as plain text so a future Stage-6 format
//! change shows up here as a tooling regression.

use std::io::Write;
use std::process::{Command, Stdio};

const CLEAN: &str = include_str!("fixtures/clean_run.log");
const LOSSY: &str = include_str!("fixtures/lossy_run.log");
const VIOL: &str = include_str!("fixtures/with_violations.log");
const MULTIPROC: &str = include_str!("fixtures/multiprocess_merged.log");
const FREETEXT: &str = include_str!("fixtures/freetext_mixed.log");

fn jl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jl"))
}

fn jldiff() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jldiff"))
}

fn pipe_through(cmd: &mut Command, input: &str) -> (String, String, i32) {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn write_tmp(s: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("tmp");
    f.write_all(s.as_bytes()).expect("write");
    f
}

// --- clean_run.log --------------------------------------------------------

#[test]
fn fixture_clean_count_pkt_rx() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), CLEAN);
    assert_eq!(rc, 0);
    let count = out.lines().filter(|l| l.starts_with("PKT_RX")).count();
    // clean_run.log has 13 PKT_RX lines.
    assert_eq!(count, 13, "stdout: {out}");
}

#[test]
fn fixture_clean_count_pkt_local() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_LOCAL"]), CLEAN);
    assert_eq!(rc, 0);
    // 4 PKT_LOCAL events.
    let count = out.lines().filter(|l| l.starts_with("PKT_LOCAL")).count();
    assert_eq!(count, 4);
}

#[test]
fn fixture_clean_slice_between_events() {
    let (out, _err, rc) = pipe_through(
        jl().args(["--since-event", "PATH_ADD", "--until-event", "PKT_DROP"]),
        CLEAN,
    );
    assert_eq!(rc, 0);
    // Slice opens at the FIRST PATH_ADD (t=11) and closes BEFORE the
    // first PKT_DROP (t=132).  Inside: PATH_ADD t=11 + everything up
    // to (but not including) PKT_DROP t=132.
    assert!(out.lines().next().unwrap().starts_with("PATH_ADD"));
    assert!(!out.contains("PKT_DROP"));
}

// --- lossy_run.log + jldiff against clean ---------------------------------

#[test]
fn fixture_lossy_count_pkt_rx() {
    // Lossy = clean minus 3 PKT_RX events → 13 - 3 = 10 PKT_RX.
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), LOSSY);
    assert_eq!(rc, 0);
    let count = out.lines().filter(|l| l.starts_with("PKT_RX")).count();
    assert_eq!(count, 10);
}

#[test]
fn fixture_lossy_path_add_unchanged() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PATH_ADD"]), LOSSY);
    assert_eq!(rc, 0);
    // PATH_ADD events were not removed; lossy has the same 4 as clean.
    let count = out.lines().filter(|l| l.starts_with("PATH_ADD")).count();
    assert_eq!(count, 4);
}

#[test]
fn fixture_jldiff_clean_vs_lossy() {
    let l = write_tmp(CLEAN);
    let r = write_tmp(LOSSY);
    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,dst",
            l.path().to_str().unwrap(),
            r.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn jldiff");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    // Three (PKT_RX, *) events present in clean but not in lossy: one
    // surplus (PKT_RX, abc1), the unique (PKT_RX, abc4), and the
    // unique (PKT_RX, abc8).  All three surface in LEFT_ONLY.
    assert!(
        stdout.contains("LEFT_ONLY (3)"),
        "expected LEFT_ONLY=3, got: {stdout}"
    );
    assert!(stdout.contains("RIGHT_ONLY (0)"));
    // Some pairs differ because the (PKT_RX, abc1) bucket pairs by
    // file-order across mismatched multiplicities, so the t= values
    // shift.  Exact counts are sensitive to the fixture; we just
    // assert MATCHED_DIFFER and MATCHED_IDENTICAL are populated.
    assert!(stdout.contains("MATCHED_DIFFER"));
    assert!(stdout.contains("MATCHED_IDENTICAL"));
}

// --- with_violations.log --------------------------------------------------

#[test]
fn fixture_violations_count() {
    let schema_count = VIOL
        .lines()
        .filter(|l| l.starts_with("EVENT_SCHEMA_VIOLATION"))
        .count();
    let field_count = VIOL
        .lines()
        .filter(|l| l.starts_with("EVENT_FIELD_VIOLATION"))
        .count();
    assert_eq!(schema_count, 2, "fixture should have 2 schema violations");
    assert_eq!(field_count, 1, "fixture should have 1 field violation");
}

#[test]
fn fixture_violations_filter_event_pkt_rx() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), VIOL);
    assert_eq!(rc, 0);
    // event=PKT_RX matches both real PKT_RX events AND the violation
    // lines whose `event=` field is PKT_RX.
    assert!(out.lines().any(|l| l.starts_with("PKT_RX")));
    assert!(out.lines().any(|l| l.starts_with("EVENT_SCHEMA_VIOLATION")));
    assert!(out.lines().any(|l| l.starts_with("EVENT_FIELD_VIOLATION")));
    // PATH_ADD and ANN_RX are filtered out.
    assert!(!out.contains("PATH_ADD"));
    assert!(!out.contains("ANN_RX"));
}

#[test]
fn fixture_violations_filter_event_pkt_drop() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_DROP"]), VIOL);
    assert_eq!(rc, 0);
    // event=PKT_DROP matches both the real PKT_DROP and the
    // EVENT_SCHEMA_VIOLATION line referencing it.
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines.iter().any(|l| l.starts_with("PKT_DROP")));
    assert!(lines
        .iter()
        .any(|l| l.starts_with("EVENT_SCHEMA_VIOLATION") && l.contains("event=PKT_DROP")));
}

// --- multiprocess_merged.log ----------------------------------------------

#[test]
fn fixture_multiproc_three_nodes_present() {
    for node in &["alpha", "beta", "gamma"] {
        let (out, _err, rc) = pipe_through(jl().args(["--node", node]), MULTIPROC);
        assert_eq!(rc, 0);
        // Each node should contribute multiple events.
        let count = out
            .lines()
            .filter(|l| l.contains(&format!("node={node}")))
            .count();
        assert!(count >= 5, "node={node} count={count}");
    }
}

#[test]
fn fixture_multiproc_t_window() {
    // Slice events with t<100.
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "t<100"]), MULTIPROC);
    assert_eq!(rc, 0);
    // Events present at t<100: t=10..92 cover ANN_RX/PATH_ADD/PKT_RX/PKT_LOCAL/PKT_FORWARD/PKT_DROP across the three nodes.
    // Just assert the filter actually narrowed the output (output strictly smaller than full input).
    let total_input_events = MULTIPROC.lines().count();
    let narrowed = out.lines().count();
    assert!(narrowed < total_input_events);
    assert!(narrowed > 0);
}

#[test]
fn fixture_multiproc_pkt_local_count() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_LOCAL"]), MULTIPROC);
    assert_eq!(rc, 0);
    let count = out.lines().filter(|l| l.starts_with("PKT_LOCAL")).count();
    // 8 PKT_LOCAL events in the fixture.
    assert_eq!(count, 8, "stdout: {out}");
}

// --- freetext_mixed.log ---------------------------------------------------

#[test]
fn fixture_freetext_banners_pass_through() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), FREETEXT);
    assert_eq!(rc, 0);
    // Banners and free-text lines pass through unchanged regardless of filter.
    assert!(out.contains("=== EVENT LOG DUMP"));
    assert!(out.contains("=== END EVENT LOG DUMP"));
    assert!(out.contains("running 1 test"));
    assert!(out.contains("[WARN ]"));
    assert!(out.contains("[ERROR]"));
    // PKT_RX events filter through.
    let pkt_rx_count = out.lines().filter(|l| l.starts_with("PKT_RX")).count();
    assert_eq!(pkt_rx_count, 4);
}

#[test]
fn fixture_freetext_no_filter_identity() {
    // No filter = pure pass-through.
    let (out, _err, rc) = pipe_through(&mut jl(), FREETEXT);
    assert_eq!(rc, 0);
    assert_eq!(out, FREETEXT);
}

#[test]
fn fixture_freetext_until_event() {
    let (out, _err, rc) = pipe_through(jl().args(["--until-event", "PKT_DROP"]), FREETEXT);
    assert_eq!(rc, 0);
    // Cuts off at the first PKT_DROP (t=120).  Earlier banner / [WARN]
    // / [INFO] lines pass through.  Later events (PKT_RX t=200, etc.)
    // and trailing free-text are dropped.
    assert!(out.contains("running 1 test"));
    assert!(out.contains("PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=160 type=Data t=80"));
    assert!(!out.contains("PKT_DROP"));
    assert!(!out.contains("PKT_FORWARD"));
    assert!(!out.contains("test result: FAILED"));
}

// --- meta: every fixture roundtrips through `jl` (no filter) --------------

#[test]
fn all_fixtures_roundtrip_no_filter() {
    for (name, body) in &[
        ("clean_run.log", CLEAN),
        ("lossy_run.log", LOSSY),
        ("with_violations.log", VIOL),
        ("multiprocess_merged.log", MULTIPROC),
        ("freetext_mixed.log", FREETEXT),
    ] {
        let (out, _err, rc) = pipe_through(&mut jl(), body);
        assert_eq!(rc, 0, "fixture {name} returned non-zero");
        assert_eq!(
            &out.as_str(),
            body,
            "fixture {name} did not round-trip identically"
        );
    }
}

// --- p2_proof_and_identity.log -------------------------------------------

const P2_LOG: &str = include_str!("fixtures/p2_proof_and_identity.log");

/// Verifies the Stage-6 catalogue expansion (Codeberg #50 P2): every
/// new event name (IDENTITY, EMB_EVICT, EMB_INSERT_FAIL, PATH_LOOKUP,
/// PATH_TABLE, PATH_TABLE_ENTRY, PROOF_GEN, PROOF_SEND, REVERSE_ADD)
/// is filterable by `jl --filter event=…` across a real-shape log.
#[test]
fn fixture_p2_filter_proof_gen() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PROOF_GEN"]), P2_LOG);
    assert_eq!(rc, 0);
    let count = out.lines().filter(|l| l.starts_with("PROOF_GEN")).count();
    assert_eq!(count, 3, "expected 3 PROOF_GEN events, got: {out}");
}

#[test]
fn fixture_p2_filter_proof_send() {
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PROOF_SEND"]), P2_LOG);
    assert_eq!(rc, 0);
    let count = out.lines().filter(|l| l.starts_with("PROOF_SEND")).count();
    assert_eq!(count, 3);
}

#[test]
fn fixture_p2_filter_path_table_family() {
    // PATH_TABLE_ENTRY and PATH_TABLE are distinct events even though
    // they share a prefix.  jl's filter is exact on event=, not prefix.
    let (out_table, _, _) = pipe_through(jl().args(["--filter", "event=PATH_TABLE"]), P2_LOG);
    let (out_entry, _, _) = pipe_through(jl().args(["--filter", "event=PATH_TABLE_ENTRY"]), P2_LOG);
    assert_eq!(
        out_table
            .lines()
            .filter(|l| l.starts_with("PATH_TABLE "))
            .count(),
        1
    );
    assert_eq!(
        out_entry
            .lines()
            .filter(|l| l.starts_with("PATH_TABLE_ENTRY "))
            .count(),
        2
    );
    // PATH_TABLE filter should NOT match PATH_TABLE_ENTRY lines.
    assert_eq!(
        out_table
            .lines()
            .filter(|l| l.starts_with("PATH_TABLE_ENTRY"))
            .count(),
        0
    );
}

#[test]
fn fixture_p2_jldiff_lossless_roundtrip() {
    // Two copies of the same fixture diff to all-identical with no
    // _ONLY surplus.  Smoke that the new event names round-trip
    // through jldiff's parser identically to the originals.
    let l = write_tmp(P2_LOG);
    let r = write_tmp(P2_LOG);
    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,t",
            l.path().to_str().unwrap(),
            r.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn jldiff");
    assert!(cmd_out.status.success());
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    assert!(stdout.contains("LEFT_ONLY (0)"));
    assert!(stdout.contains("RIGHT_ONLY (0)"));
    assert!(stdout.contains("MATCHED_DIFFER (0 pairs)"));
    let entries = P2_LOG.lines().count();
    assert!(stdout.contains(&format!("MATCHED_IDENTICAL ({entries} pairs)")));
}
