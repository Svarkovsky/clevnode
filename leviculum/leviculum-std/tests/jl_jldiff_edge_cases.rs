//! Edge-case + adversarial tests for `jl` and `jldiff`.

use std::io::Write;
use std::process::{Command, Stdio};

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
    // Forgive a broken pipe — the child may have exited (e.g. arg-parse
    // error → exit 2) before our stdin write completes.  We capture
    // exit code below regardless.
    let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
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

#[test]
fn test_jl_empty_input() {
    let (out, err, rc) = pipe_through(&mut jl(), "");
    assert_eq!(rc, 0);
    assert!(out.is_empty());
    assert!(err.is_empty());
}

#[test]
fn test_jl_single_event_no_filter() {
    let input = "PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10\n";
    let (out, _err, rc) = pipe_through(&mut jl(), input);
    assert_eq!(rc, 0);
    assert_eq!(out, input);
}

#[test]
fn test_jl_thousand_events_smoke() {
    let mut input = String::new();
    for i in 0..1000 {
        let event = if i % 3 == 0 { "PKT_RX" } else { "ANN_RX" };
        input.push_str(&format!(
            "{event} node=alpha dst=abc{i} hops=0 iface=lora0 len=64 type=Data path_response=false t={i}\n"
        ));
    }
    let start = std::time::Instant::now();
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), &input);
    let elapsed = start.elapsed();
    assert_eq!(rc, 0);
    let count = out.lines().filter(|l| l.starts_with("PKT_RX")).count();
    // 0..1000 has 334 multiples of 3 (0, 3, 6, ..., 999).
    assert_eq!(count, 334);
    assert!(elapsed.as_secs() < 5, "1000-event run took {elapsed:?}");
}

#[test]
fn test_jl_t_zero_and_huge_t() {
    let input = "\
PKT_LOCAL node=alpha dst=zero iface=lora0 matched=true t=0
PKT_LOCAL node=alpha dst=huge iface=lora0 matched=true t=18446744073709551615
";
    // t>=0 includes both (zero is >= zero).
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "t>=0"]), input);
    assert_eq!(rc, 0);
    assert!(out.contains("dst=zero"));
    assert!(out.contains("dst=huge"));

    // t<10 includes only the zero one.
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "t<10"]), input);
    assert_eq!(rc, 0);
    assert!(out.contains("dst=zero"));
    assert!(!out.contains("dst=huge"));
}

#[test]
fn test_jl_filter_no_match() {
    let input = "PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10\n";
    let (out, err, rc) = pipe_through(jl().args(["--filter", "event=NONEXISTENT_EVENT"]), input);
    assert_eq!(rc, 0);
    assert!(out.is_empty());
    assert!(err.is_empty());
}

#[test]
fn test_jl_contradictory_filters() {
    let input = "PKT_LOCAL node=alpha dst=abc1 iface=lora0 matched=true t=150\n";
    let (out, _err, rc) =
        pipe_through(jl().args(["--filter", "t<100", "--filter", "t>200"]), input);
    assert_eq!(rc, 0);
    assert!(out.is_empty());
}

#[test]
fn test_jl_t_compare_non_integer() {
    let input = "";
    let (_out, err, rc) = pipe_through(jl().args(["--filter", "t<abc"]), input);
    assert_eq!(rc, 2);
    assert!(err.contains("invalid filter"));
}

#[test]
fn test_jl_malformed_freetext_with_equals() {
    // Lines that have `=` somewhere in them but don't fit the
    // EVENT_NAME-then-tokens shape are non-events: pass through.
    let input = "\
just a free text line with key=value embedded
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
";
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), input);
    assert_eq!(rc, 0);
    assert!(out.contains("just a free text line"));
    assert!(out.contains("PKT_RX node=alpha"));
}

#[test]
fn test_jl_lowercase_first_token_is_freetext() {
    // EVENT_NAME must be ALL_UPPERCASE_OR_DIGITS_OR_UNDERSCORES; a
    // first token like "info:" is treated as free text and passes
    // through filters unchanged.
    let input = "\
info: something happened key=value t=10
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
";
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), input);
    assert_eq!(rc, 0);
    assert!(out.contains("info: something happened"));
    assert!(out.contains("PKT_RX node=alpha"));
}

#[test]
fn test_jl_value_contains_brackets_and_punctuation() {
    // Values like `missing=[hops,len]` (synthetic violation lines) must
    // round-trip and match prefix filters correctly.
    let input = "\
EVENT_SCHEMA_VIOLATION event=PKT_RX missing=[hops,len] caller=transport.rs:1074 t=100
EVENT_FIELD_VIOLATION event=PKT_RX field=note value_problem=whitespace caller=transport.rs:1074 t=120
";
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "missing=[*"]), input);
    assert_eq!(rc, 0);
    assert!(out.contains("EVENT_SCHEMA_VIOLATION"));
    assert!(!out.contains("EVENT_FIELD_VIOLATION"));
}

#[test]
fn test_jldiff_both_empty_files() {
    let l = write_tmp("");
    let r = write_tmp("");
    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,dst",
            l.path().to_str().unwrap(),
            r.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    assert!(cmd_out.status.success());
    assert!(stdout.contains("LEFT_ONLY (0)"));
    assert!(stdout.contains("RIGHT_ONLY (0)"));
    assert!(stdout.contains("MATCHED_DIFFER (0 pairs)"));
    assert!(stdout.contains("MATCHED_IDENTICAL (0 pairs)"));
}

#[test]
fn test_jldiff_one_empty_file() {
    let right = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PKT_RX node=alpha dst=abc2 hops=0 iface=lora0 len=64 type=Data t=20
PKT_RX node=alpha dst=abc3 hops=0 iface=lora0 len=64 type=Data t=30
PKT_RX node=alpha dst=abc4 hops=0 iface=lora0 len=64 type=Data t=40
PKT_RX node=alpha dst=abc5 hops=0 iface=lora0 len=64 type=Data t=50
";
    let l = write_tmp("");
    let r = write_tmp(right);
    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,dst",
            l.path().to_str().unwrap(),
            r.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    assert!(stdout.contains("LEFT_ONLY (0)"));
    assert!(stdout.contains("RIGHT_ONLY (5)"));
    for i in 1..=5 {
        assert!(stdout.contains(&format!("dst=abc{i}")));
    }
}

#[test]
fn test_jl_multiple_input_files() {
    // Two files in order; output is concatenation in order, filter applied to each.
    let f1 = write_tmp("PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10\n");
    let f2 = write_tmp(
        "\
PKT_RX node=beta dst=abc2 hops=0 iface=lora0 len=64 type=Data t=20
ANN_RX node=beta dst=abc2 hops=0 iface=lora0 path_response=false t=21
",
    );
    let cmd_out = jl()
        .args([
            "--filter",
            "event=PKT_RX",
            f1.path().to_str().unwrap(),
            f2.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    assert!(cmd_out.status.success());
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("dst=abc1"));
    assert!(lines[1].contains("dst=abc2"));
}

#[test]
fn test_jl_multiple_since_event_errors() {
    let input = "PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10\n";
    let (_out, err, rc) = pipe_through(
        jl().args(["--since-event", "ANN_RX", "--since-event", "PKT_DROP"]),
        input,
    );
    assert_eq!(rc, 2);
    assert!(err.contains("--since-event"));
}

#[test]
fn test_jl_multiple_until_event_errors() {
    let input = "PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10\n";
    let (_out, err, rc) = pipe_through(
        jl().args(["--until-event", "ANN_RX", "--until-event", "PKT_DROP"]),
        input,
    );
    assert_eq!(rc, 2);
    assert!(err.contains("--until-event"));
}
