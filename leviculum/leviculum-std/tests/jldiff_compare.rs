//! Integration tests for the `jldiff` compare binary.  Stage 7.

use std::io::Write;
use std::process::{Command, Stdio};

fn jldiff() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jldiff"))
}

fn write_tmp(contents: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("tmp");
    f.write_all(contents.as_bytes()).expect("write");
    f
}

fn run(args: &[&str]) -> (String, String, i32) {
    let out = jldiff()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn jldiff");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn test_identical_files() {
    let log = "\
PKT_RX node=alpha dst=abc1 hops=0 t=10
ANN_RX node=alpha dst=abc1 hops=0 t=20
PATH_ADD node=alpha dst=abc1 next_hop=alpha t=30
";
    let l = write_tmp(log);
    let r = write_tmp(log);
    let (out, _err, rc) = run(&[
        "--align-on",
        "event,dst",
        l.path().to_str().unwrap(),
        r.path().to_str().unwrap(),
    ]);
    assert_eq!(rc, 0);
    assert!(out.contains("=== LEFT_ONLY (0) ==="));
    assert!(out.contains("=== RIGHT_ONLY (0) ==="));
    assert!(out.contains("=== MATCHED_DIFFER (0 pairs) ==="));
    assert!(out.contains("=== MATCHED_IDENTICAL (3 pairs) ==="));
}

#[test]
fn test_left_only_event() {
    let left = "\
PKT_RX node=alpha dst=abc1 t=10
ANN_RX node=alpha dst=abc1 t=20
PATH_ADD node=alpha dst=abc1 t=30
";
    let right = "\
PKT_RX node=alpha dst=abc1 t=10
PATH_ADD node=alpha dst=abc1 t=30
";
    let l = write_tmp(left);
    let r = write_tmp(right);
    let (out, _err, rc) = run(&[
        "--align-on",
        "event,dst",
        l.path().to_str().unwrap(),
        r.path().to_str().unwrap(),
    ]);
    assert_eq!(rc, 0);
    assert!(out.contains("=== LEFT_ONLY (1) ==="));
    assert!(out.contains("ANN_RX node=alpha dst=abc1 t=20"));
    assert!(out.contains("=== RIGHT_ONLY (0) ==="));
}

#[test]
fn test_right_only_event() {
    let left = "\
PKT_RX node=alpha dst=abc1 t=10
PATH_ADD node=alpha dst=abc1 t=30
";
    let right = "\
PKT_RX node=alpha dst=abc1 t=10
ANN_RX node=alpha dst=abc1 t=20
PATH_ADD node=alpha dst=abc1 t=30
";
    let l = write_tmp(left);
    let r = write_tmp(right);
    let (out, _err, rc) = run(&[
        "--align-on",
        "event,dst",
        l.path().to_str().unwrap(),
        r.path().to_str().unwrap(),
    ]);
    assert_eq!(rc, 0);
    assert!(out.contains("=== LEFT_ONLY (0) ==="));
    assert!(out.contains("=== RIGHT_ONLY (1) ==="));
    assert!(out.contains("ANN_RX node=alpha dst=abc1 t=20"));
}

#[test]
fn test_matched_differ() {
    let left = "PKT_RX node=alpha dst=abc1 hops=1 t=10\n";
    let right = "PKT_RX node=alpha dst=abc1 hops=2 t=10\n";
    let l = write_tmp(left);
    let r = write_tmp(right);
    let (out, _err, rc) = run(&[
        "--align-on",
        "event,dst",
        l.path().to_str().unwrap(),
        r.path().to_str().unwrap(),
    ]);
    assert_eq!(rc, 0);
    assert!(out.contains("=== MATCHED_DIFFER (1 pairs) ==="));
    assert!(out.contains("DIFF: hops=1|2"), "missing diff line: {out}");
}

#[test]
fn test_align_on_multikey() {
    // Three events with the SAME (event, dst) pair but DIFFERENT iface;
    // align-on event,dst,iface partitions them into three distinct buckets.
    let left = "\
PKT_RX node=alpha dst=abc1 iface=lora0 t=10
PKT_RX node=alpha dst=abc1 iface=tcp1 t=20
PKT_RX node=alpha dst=abc1 iface=ble0 t=30
";
    let right = "\
PKT_RX node=alpha dst=abc1 iface=lora0 t=15
PKT_RX node=alpha dst=abc1 iface=ble0 t=35
";
    let l = write_tmp(left);
    let r = write_tmp(right);
    let (out, _err, rc) = run(&[
        "--align-on",
        "event,dst,iface",
        l.path().to_str().unwrap(),
        r.path().to_str().unwrap(),
    ]);
    assert_eq!(rc, 0);
    // 2 paired (lora0+lora0, ble0+ble0), 1 left-only (tcp1), 0 right-only,
    // each matched pair has differing t= → MATCHED_DIFFER not IDENTICAL.
    assert!(out.contains("=== LEFT_ONLY (1) ==="));
    assert!(out.contains("iface=tcp1"));
    assert!(out.contains("=== RIGHT_ONLY (0) ==="));
    assert!(out.contains("=== MATCHED_DIFFER (2 pairs) ==="));
}

#[test]
fn test_unalignable_event() {
    let left = "\
PKT_RX node=alpha hops=1 t=10
ANN_RX node=alpha dst=abc1 hops=0 t=20
";
    let right = "\
ANN_RX node=alpha dst=abc1 hops=0 t=20
";
    let l = write_tmp(left);
    let r = write_tmp(right);
    let (out, _err, rc) = run(&[
        "--align-on",
        "event,dst",
        l.path().to_str().unwrap(),
        r.path().to_str().unwrap(),
    ]);
    assert_eq!(rc, 0);
    // The PKT_RX line is missing `dst` → unalignable, surfaces in LEFT_ONLY
    // with the [unalignable: missing key dst] note.
    assert!(out.contains("=== LEFT_ONLY (1) ==="));
    assert!(
        out.contains("[unalignable: missing key dst]"),
        "missing unalignable note: {out}"
    );
    // ANN_RX matches in both, no difference → MATCHED_IDENTICAL (1 pairs)
    assert!(out.contains("=== MATCHED_IDENTICAL (1 pairs) ==="));
}

#[test]
fn test_align_on_required() {
    // No --align-on → clap's required=true rejects with a usage error.
    let l = write_tmp("PKT_RX dst=abc1 t=10\n");
    let r = write_tmp("PKT_RX dst=abc1 t=10\n");
    let (_out, err, rc) = run(&[l.path().to_str().unwrap(), r.path().to_str().unwrap()]);
    assert_eq!(rc, 2);
    assert!(
        err.contains("--align-on") || err.contains("required"),
        "stderr did not flag missing --align-on: {err}"
    );
}

#[test]
fn test_multiplicity_extra_left() {
    // Same (event, dst) appears 3× left and 2× right.  First two pair;
    // the 3rd left occurrence goes to LEFT_ONLY.  Different t= per
    // occurrence so each pair is reported as MATCHED_DIFFER (t differs).
    let left = "\
PKT_RX node=alpha dst=abc1 t=10
PKT_RX node=alpha dst=abc1 t=20
PKT_RX node=alpha dst=abc1 t=30
";
    let right = "\
PKT_RX node=alpha dst=abc1 t=15
PKT_RX node=alpha dst=abc1 t=25
";
    let l = write_tmp(left);
    let r = write_tmp(right);
    let (out, _err, rc) = run(&[
        "--align-on",
        "event,dst",
        l.path().to_str().unwrap(),
        r.path().to_str().unwrap(),
    ]);
    assert_eq!(rc, 0);
    assert!(out.contains("=== LEFT_ONLY (1) ==="));
    assert!(out.contains("=== RIGHT_ONLY (0) ==="));
    assert!(out.contains("=== MATCHED_DIFFER (2 pairs) ==="));
    // The third left (t=30) is the unpaired surplus.
    assert!(out.contains("PKT_RX node=alpha dst=abc1 t=30"));
}
