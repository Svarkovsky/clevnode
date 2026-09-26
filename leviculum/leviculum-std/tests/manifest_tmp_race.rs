//! Two concurrent same-gate invocations of `scripts/run-with-manifest.py`
//! must both exit 0 (Codeberg #224).
//!
//! The defect this pins: the wrapper wrote `<gate>.json.tmp` and then
//! `os.replace`d it into `<gate>.json`. Two concurrent invocations of the
//! same gate — observed as a pre-push `just fast` while `just standard` was
//! running in the same tree — shared that tmp path, one replace consumed the
//! file and the other crashed with FileNotFoundError, reporting a green gate
//! as a failed push. The tmp name is per-invocation now (`<gate>.json.<pid>
//! .tmp`), so any interleaving of two runs ends with both exiting 0 and the
//! last writer's manifest in place.
//!
//! This concurrent run is deterministic in the direction that matters: with
//! per-invocation tmp names there is no shared path left to race on, so both
//! invocations succeed under EVERY interleaving, not just the ones this test
//! happens to produce. (Against the pre-fix code it reproduces the crash only
//! when the write→replace windows actually interleave, which a two-process
//! test cannot force from outside; the naming and cleanup assertions below
//! are the deterministic pins for the mechanism itself.)

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Repo root, from the crate dir cargo hands every test.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("leviculum-std sits one level under the repo root")
        .to_path_buf()
}

/// One wrapper invocation of a trivial no-tests gate, stdio captured so the
/// child's `[manifest]` lines cannot leak into this binary's libtest stream
/// (the outer gate's manifest parser reads exactly such lines).
fn wrapper_command(manifest_dir: &Path, gate: &str) -> Command {
    let root = repo_root();
    let mut cmd = Command::new("python3");
    cmd.arg(root.join("scripts/run-with-manifest.py"))
        .args(["--gate", gate, "--no-tests", "--", "sleep", "0.4"])
        .current_dir(&root)
        .env("LEVICULUM_MANIFEST_DIR", manifest_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// The race itself: two concurrent invocations of the same trivial gate must
/// both exit 0, and the gate's manifest must exist afterwards with no tmp
/// file left behind.
#[test]
fn concurrent_same_gate_invocations_both_exit_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate = "tmp-race-gate";

    let children: Vec<_> = (0..2)
        .map(|i| {
            wrapper_command(dir.path(), gate)
                .spawn()
                .unwrap_or_else(|e| panic!("spawn wrapper {i}: {e}"))
        })
        .collect();

    for (i, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().expect("wait for wrapper");
        assert!(
            out.status.success(),
            "wrapper invocation {i} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    assert!(
        dir.path().join(format!("{gate}.json")).is_file(),
        "the last writer's manifest must be in place"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read manifest dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a finished invocation must not leave its tmp behind: {leftovers:?}"
    );
}

/// The startup sweep: a stale per-invocation tmp (a crashed run's leftover,
/// older than a day) is removed, a fresh one (possibly a live concurrent
/// run's) is kept, and the tmp of an unrelated gate is not touched.
#[test]
fn startup_sweep_unlinks_only_this_gates_stale_tmps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate = "tmp-sweep-gate";

    let stale = dir.path().join(format!("{gate}.json.11111.tmp"));
    let fresh = dir.path().join(format!("{gate}.json.22222.tmp"));
    let other = dir.path().join("other-gate.json.33333.tmp");
    for p in [&stale, &fresh, &other] {
        std::fs::write(p, "{}").expect("seed tmp");
    }
    let two_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
    let f = std::fs::File::options()
        .write(true)
        .open(&stale)
        .expect("open stale tmp");
    f.set_modified(two_days_ago).expect("age the stale tmp");
    let f = std::fs::File::options()
        .write(true)
        .open(&other)
        .expect("open other gate's tmp");
    f.set_modified(two_days_ago)
        .expect("age the other gate's tmp");

    let out = wrapper_command(dir.path(), gate)
        .spawn()
        .expect("spawn wrapper")
        .wait_with_output()
        .expect("wait for wrapper");
    assert!(
        out.status.success(),
        "wrapper exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(!stale.exists(), "a stale tmp of this gate must be swept");
    assert!(
        fresh.exists(),
        "a fresh tmp may belong to a live concurrent run and must survive"
    );
    assert!(
        other.exists(),
        "another gate's tmp is that gate's business, stale or not"
    );
}
