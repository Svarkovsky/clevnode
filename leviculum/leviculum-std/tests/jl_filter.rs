//! Integration tests for the `jl` filter binary.  Stage 7 / Codeberg #39.

use std::io::Write;
use std::process::{Command, Stdio};

fn jl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jl"))
}

fn run_with_stdin(args: &[&str], input: &str) -> (String, String, i32) {
    let mut child = jl()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jl");
    // The child may exit before consuming all of stdin (e.g. it rejects a
    // malformed --filter argument up front, closing its stdin). A broken pipe
    // here is therefore expected, not a test failure; surface any other error.
    if let Err(e) = child.stdin.take().unwrap().write_all(input.as_bytes()) {
        assert_eq!(
            e.kind(),
            std::io::ErrorKind::BrokenPipe,
            "write stdin failed: {e}"
        );
    }
    let out = child.wait_with_output().expect("wait jl");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

const SAMPLE: &str = "\
PKT_RX node=alice dst=abc1 hops=0 t=100
ANN_RX node=alice dst=abc1 hops=0 t=200
PATH_ADD node=alice dst=abc1 next_hop=bob t=300
PKT_RX node=bob dst=abc2 hops=1 t=600
PKT_FORWARD node=bob dst=abc2 next_hop=carol t=700
ANN_RX node=carol dst=def3 hops=2 t=900
PKT_RX node=carol dst=def3 hops=0 t=1100
LINK_CLOSE node=carol dst=def3 reason=normal t=1300
PKT_RX node=carol dst=def4 hops=0 t=1400
";

#[test]
fn test_no_filter_passes_through() {
    let (out, _err, rc) = run_with_stdin(&[], SAMPLE);
    assert_eq!(rc, 0);
    assert_eq!(out, SAMPLE);
}

#[test]
fn test_exact_filter() {
    // `event=` is the canonical filter for "this kind of event"; it
    // matches against both EVENT_NAME (first token) and any explicit
    // `event=` field.  In the SAMPLE, PKT_RX events have EVENT_NAME ==
    // PKT_RX, so the filter matches the three PKT_RX lines.
    let (out, _err, rc) = run_with_stdin(&["--filter", "event=PKT_RX"], SAMPLE);
    assert_eq!(rc, 0);
    let lines: Vec<&str> = out.lines().collect();
    // SAMPLE has 4 PKT_RX lines.
    assert_eq!(lines.len(), 4);
    for line in &lines {
        assert!(line.starts_with("PKT_RX"), "non-PKT_RX line: {line}");
    }
}

#[test]
fn test_wildcard_filter() {
    let (out, _err, rc) = run_with_stdin(&["--filter", "dst=ab*"], SAMPLE);
    assert_eq!(rc, 0);
    // Lines whose dst starts with "ab": dst=abc1 (3×), dst=abc2 (2×)
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5);
    for line in &lines {
        assert!(
            line.contains("dst=abc1") || line.contains("dst=abc2"),
            "unexpected line: {line}"
        );
    }
}

#[test]
fn test_t_range() {
    let (out, _err, rc) = run_with_stdin(&["--filter", "t>500", "--filter", "t<1000"], SAMPLE);
    assert_eq!(rc, 0);
    // 500 < t < 1000: t=600, t=700, t=900
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("t=600"));
    assert!(lines[1].contains("t=700"));
    assert!(lines[2].contains("t=900"));
}

#[test]
fn test_node_shorthand() {
    let (out_short, _, _) = run_with_stdin(&["--node", "alice"], SAMPLE);
    let (out_long, _, _) = run_with_stdin(&["--filter", "node=alice"], SAMPLE);
    assert_eq!(out_short, out_long);
    let lines: Vec<&str> = out_short.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert!(line.contains("node=alice"));
    }
}

#[test]
fn test_since_event() {
    let (out, _err, rc) = run_with_stdin(&["--since-event", "PATH_ADD"], SAMPLE);
    assert_eq!(rc, 0);
    let lines: Vec<&str> = out.lines().collect();
    // PATH_ADD itself + everything after = 7 lines (PATH_ADD, PKT_RX@600,
    // PKT_FORWARD, ANN_RX@900, PKT_RX@1100, LINK_CLOSE, PKT_RX@1400)
    assert_eq!(lines.len(), 7);
    assert!(lines[0].starts_with("PATH_ADD"));
}

#[test]
fn test_until_event() {
    let (out, _err, rc) = run_with_stdin(&["--until-event", "LINK_CLOSE"], SAMPLE);
    assert_eq!(rc, 0);
    let lines: Vec<&str> = out.lines().collect();
    // Everything before LINK_CLOSE (excluded): PKT_RX, ANN_RX, PATH_ADD,
    // PKT_RX, PKT_FORWARD, ANN_RX, PKT_RX = 7 lines
    assert_eq!(lines.len(), 7);
    assert!(!out.contains("LINK_CLOSE"));
}

#[test]
fn test_freetext_pass_through() {
    let input = "\
=== EVENT LOG DUMP (test panicked, 3 lines) ===
PKT_RX node=alice dst=abc1 hops=0 t=100
PKT_RX node=alice dst=abc1 hops=0 t=200
=== END EVENT LOG DUMP ===
some random free text line
";
    let (out, _err, rc) = run_with_stdin(&["--filter", "node=alice"], input);
    assert_eq!(rc, 0);
    // Free-text lines pass through unchanged; only event lines are filtered.
    // Banners and "some random free text line" are not parseable as events
    // (start tokens don't match all-uppercase event-name shape), so they pass.
    assert!(out.contains("=== EVENT LOG DUMP"));
    assert!(out.contains("=== END EVENT LOG DUMP"));
    assert!(out.contains("some random free text"));
    assert!(out.contains("PKT_RX node=alice dst=abc1 hops=0 t=100"));
    assert!(out.contains("PKT_RX node=alice dst=abc1 hops=0 t=200"));
}

#[test]
fn test_violation_lines_match_event_field() {
    // Synthetic violation lines reference the offending event via an
    // explicit `event=` field.  The filter must match those lines too,
    // so the event=-field convention is consistent for both real events
    // (matched via EVENT_NAME) and synthetic ones (matched via field).
    let input = "\
ANN_RX node=alice dst=abc1 hops=0 t=200
EVENT_SCHEMA_VIOLATION event=PKT_RX missing=[hops,len] caller=transport.rs:1074 t=150
EVENT_FIELD_VIOLATION event=PKT_RX field=note value_problem=whitespace caller=transport.rs:1074 t=160
";
    let (out, _err, rc) = run_with_stdin(&["--filter", "event=PKT_RX"], input);
    assert_eq!(rc, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("EVENT_SCHEMA_VIOLATION"));
    assert!(lines[1].starts_with("EVENT_FIELD_VIOLATION"));
}

#[test]
fn test_malformed_filter_errors() {
    let (out, err, rc) = run_with_stdin(&["--filter", "hops==2"], SAMPLE);
    assert_eq!(rc, 2);
    assert!(out.is_empty());
    assert!(
        err.contains("invalid filter") && err.contains("hops==2"),
        "stderr did not describe the bad filter: {err:?}"
    );
}
