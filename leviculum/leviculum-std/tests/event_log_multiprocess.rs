//! Phase-B multi-process integration test: spawn two
//! `event-log-helper` processes with overlapping `t=` cadences,
//! merge their per-process log files, and assert the merged log is
//! monotone-ordered with both `node=` keys present.

use std::path::{Path, PathBuf};
use std::process::Command;

use leviculum_std::event_log::merge_event_logs;

/// Locate the compiled `event-log-helper` binary, honouring
/// `CARGO_TARGET_DIR` and the `--target=x86_64-unknown-linux-musl`
/// workspace default in `.cargo/config.toml`.  Builds the binary if
/// missing.
fn helper_bin() -> PathBuf {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let triple = std::env::var("CARGO_BUILD_TARGET")
        .unwrap_or_else(|_| "x86_64-unknown-linux-musl".to_string());

    let candidates = [
        target_dir
            .join(&triple)
            .join("debug")
            .join("event-log-helper"),
        target_dir.join("debug").join("event-log-helper"),
    ];
    for c in &candidates {
        if c.exists() {
            return c.clone();
        }
    }

    let status = Command::new(env!("CARGO"))
        .args(["build", "--bin", "event-log-helper", "-p", "leviculum-std"])
        .status()
        .expect("cargo build for event-log-helper");
    assert!(status.success(), "cargo build failed");

    candidates
        .iter()
        .find(|c| c.exists())
        .unwrap_or_else(|| panic!("event-log-helper not found after build; tried: {candidates:?}"))
        .clone()
}

#[test]
fn test_two_process_merge_monotone_alternating() {
    let bin = helper_bin();

    let log_a = std::env::temp_dir().join(format!("event-log-helper-a-{}.log", std::process::id()));
    let log_b = std::env::temp_dir().join(format!("event-log-helper-b-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_a);
    let _ = std::fs::remove_file(&log_b);

    // Each helper has its own subscriber-init time, so per-process
    // `t=` values are local to that process — not a shared
    // wall-clock.  Within each process `t=` is monotone increasing;
    // after merge_event_logs the union is sorted by `t=`.
    let mut child_a = Command::new(&bin)
        .args(["0", "100"])
        .env("LEVICULUM_EVENT_LOG", &log_a)
        .env("LEVICULUM_EVENT_NODE", "node-a")
        .spawn()
        .expect("spawn helper a");
    let mut child_b = Command::new(&bin)
        .args(["50", "100"])
        .env("LEVICULUM_EVENT_LOG", &log_b)
        .env("LEVICULUM_EVENT_NODE", "node-b")
        .spawn()
        .expect("spawn helper b");

    let status_a = child_a.wait().expect("wait helper a");
    let status_b = child_b.wait().expect("wait helper b");
    assert!(status_a.success(), "helper a exited non-zero: {status_a:?}");
    assert!(status_b.success(), "helper b exited non-zero: {status_b:?}");

    let a_text = std::fs::read_to_string(&log_a).expect("read log_a");
    let b_text = std::fs::read_to_string(&log_b).expect("read log_b");
    let a_lines: Vec<&str> = a_text.lines().filter(|l| !l.is_empty()).collect();
    let b_lines: Vec<&str> = b_text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        a_lines.len(),
        3,
        "helper a should emit 3 events: {a_lines:?}"
    );
    assert_eq!(
        b_lines.len(),
        3,
        "helper b should emit 3 events: {b_lines:?}"
    );

    let merged = merge_event_logs(&[log_a.clone(), log_b.clone()]);
    assert_eq!(
        merged.len(),
        6,
        "merged log should have 6 lines: {merged:?}"
    );

    for line in &merged {
        assert!(line.starts_with("HELPER_TICK "), "wrong prefix: {line}");
        assert!(
            line.contains("node=node-a") || line.contains("node=node-b"),
            "missing node= field: {line}",
        );
        let last = line.split_whitespace().next_back().unwrap_or("");
        assert!(last.starts_with("t="), "t= not last: {line}");
    }

    let ts: Vec<u128> = merged
        .iter()
        .map(|l| {
            l.split_whitespace()
                .rev()
                .find_map(|t| t.strip_prefix("t="))
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    for w in ts.windows(2) {
        assert!(w[0] <= w[1], "merged log not monotone: {ts:?}");
    }

    let saw_a = merged.iter().any(|l| l.contains("node=node-a"));
    let saw_b = merged.iter().any(|l| l.contains("node=node-b"));
    assert!(saw_a, "node-a missing from merged: {merged:?}");
    assert!(saw_b, "node-b missing from merged: {merged:?}");

    let _ = std::fs::remove_file(&log_a);
    let _ = std::fs::remove_file(&log_b);
}

/// L-0022: a `LEVICULUM_EVENT_LOG` that cannot be opened must be reported
/// loudly, once — not cached as a silent `None` for the process lifetime.
/// The helper still exits 0 (a broken log path must not kill a daemon),
/// but stderr has to name the path and say that event logging is off.
#[test]
fn test_unopenable_event_log_path_is_reported_on_stderr() {
    let bin = helper_bin();
    let bad_path = "/nonexistent-dir-l0022/events.log";

    let output = Command::new(&bin)
        .args(["0", "1"])
        .env("LEVICULUM_EVENT_LOG", bad_path)
        .env("LEVICULUM_EVENT_NODE", "node-broken")
        .output()
        .expect("run helper with broken log path");

    assert!(
        output.status.success(),
        "a broken event-log path must not kill the process: {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(bad_path) && stderr.contains("LEVICULUM_EVENT_LOG"),
        "L-0022: the open failure must reach the operator on stderr, \
         naming the env var and the path; stderr was: {stderr:?}"
    );
}
