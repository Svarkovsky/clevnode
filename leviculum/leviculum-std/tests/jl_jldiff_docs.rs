//! Doc-mirror tests: every executable example in
//! `docs/src/jl-jldiff.md` is reproduced here byte-for-byte so a
//! drift between the doc and the binary triggers a test failure.
//! Anti-amnesia mechanism for the docs (per Codeberg #39 piece 4).

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
fn doc_example_1_filter_event_name() {
    let input = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=21
PKT_RX node=alpha dst=abc2 hops=1 iface=lora0 len=64 type=Data t=80
";
    let expected = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PKT_RX node=alpha dst=abc2 hops=1 iface=lora0 len=64 type=Data t=80
";
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), input);
    assert_eq!(rc, 0);
    assert_eq!(out, expected);
}

#[test]
fn doc_example_2_slice_between_markers() {
    let input = "\
PKT_LOCAL node=alpha dst=abc1 iface=lora0 matched=true t=10
PKT_LOCAL node=alpha dst=abc1 iface=lora0 matched=true t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=30
PKT_RX node=alpha dst=abc2 hops=0 iface=lora0 len=64 type=Data t=40
PKT_RX node=alpha dst=abc3 hops=0 iface=lora0 len=64 type=Data t=50
PKT_DROP node=alpha dst=abc4 hops=3 iface_in=lora0 reason=ttl_expired type=Data t=60
PKT_RX node=alpha dst=abc5 hops=0 iface=lora0 len=64 type=Data t=70
";
    let expected = "\
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=30
PKT_RX node=alpha dst=abc2 hops=0 iface=lora0 len=64 type=Data t=40
PKT_RX node=alpha dst=abc3 hops=0 iface=lora0 len=64 type=Data t=50
";
    let (out, _err, rc) = pipe_through(
        jl().args(["--since-event", "PATH_ADD", "--until-event", "PKT_DROP"]),
        input,
    );
    assert_eq!(rc, 0);
    assert_eq!(out, expected);
}

#[test]
fn doc_example_3_time_window() {
    let input = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=21
PKT_RX node=alpha dst=abc2 hops=1 iface=lora0 len=64 type=Data t=80
PKT_RX node=alpha dst=abc3 hops=2 iface=lora0 len=64 type=Data t=200
";
    let expected = "\
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=21
PKT_RX node=alpha dst=abc2 hops=1 iface=lora0 len=64 type=Data t=80
";
    let (out, _err, rc) =
        pipe_through(jl().args(["--filter", "t>=20", "--filter", "t<100"]), input);
    assert_eq!(rc, 0);
    assert_eq!(out, expected);
}

#[test]
fn doc_example_4_jldiff_basic_ab() {
    let a = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=11
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
";
    let b = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=15
PATH_ADD node=alpha dst=abc1 hops=2 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=16
";
    let expected = "\
=== LEFT_ONLY (1) ===
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20

=== RIGHT_ONLY (0) ===

=== MATCHED_DIFFER (2 pairs) ===
L: PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
R: PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=15
   DIFF: t=10|15

L: PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=11
R: PATH_ADD node=alpha dst=abc1 hops=2 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=16
   DIFF: hops=0|2 t=11|16

=== MATCHED_IDENTICAL (0 pairs) ===
";
    let l = write_tmp(a);
    let r = write_tmp(b);
    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,dst",
            l.path().to_str().unwrap(),
            r.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    assert!(cmd_out.status.success());
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    assert_eq!(stdout, expected);
}

#[test]
fn doc_example_5_jldiff_multikey() {
    let a = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PKT_RX node=alpha dst=abc1 hops=0 iface=tcp1 len=64 type=Data t=20
";
    let b = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=12
PKT_RX node=alpha dst=abc1 hops=0 iface=tcp1 len=64 type=Data t=22
";
    let expected = "\
=== LEFT_ONLY (0) ===

=== RIGHT_ONLY (0) ===

=== MATCHED_DIFFER (2 pairs) ===
L: PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
R: PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=12
   DIFF: t=10|12

L: PKT_RX node=alpha dst=abc1 hops=0 iface=tcp1 len=64 type=Data t=20
R: PKT_RX node=alpha dst=abc1 hops=0 iface=tcp1 len=64 type=Data t=22
   DIFF: t=20|22

=== MATCHED_IDENTICAL (0 pairs) ===
";
    let l = write_tmp(a);
    let r = write_tmp(b);
    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,dst,iface",
            l.path().to_str().unwrap(),
            r.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    assert!(cmd_out.status.success());
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    assert_eq!(stdout, expected);
}
