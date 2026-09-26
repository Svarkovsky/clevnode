//! The proof that a supervised child dies with its parent because the *kernel*
//! kills it, and not because any of our code ran.
//!
//! `SIGKILL` is the case no cleanup code can survive: the parent gets no
//! unwinding, no destructor, no signal handler, no last write. So a parent that
//! is `SIGKILL`ed and a child that is gone a moment later is a statement about
//! `PR_SET_PDEATHSIG` and about nothing else.
//!
//! # Shape
//!
//! Three processes. The test spawns `supervised-spawn-probe parent <mode>`,
//! which spawns `supervised-spawn-probe child`, which records its own pid and
//! its own `PR_GET_PDEATHSIG` and then sleeps. The test `SIGKILL`s the parent
//! and watches the child.
//!
//! # The negative control
//!
//! "The child is gone" is satisfiable by a child that never started, by a test
//! reading the wrong pid, by a harness that killed it itself. So the same
//! experiment runs a second time with the parent spawning through a bare
//! `Command::spawn` — the pre-fix shape — and the child must **still be alive**
//! after the same deadline. If the mechanism regresses the first arm fails; if
//! the demonstration itself rots, the control fails.
//!
//! The `PR_GET_PDEATHSIG` value the child reports is the same pair in its
//! cheapest form — `SIGKILL` under the helper, unset under a bare spawn — so a
//! refactor that quietly drops the `pre_exec` is named in milliseconds, before
//! the five-second arm even starts.
//!
//! # Bounds
//!
//! Every wait in here has a deadline and fails loudly on it, because a gate that
//! hangs reports nothing at all — which is the whole of the 2026-08-07 incident
//! this batch exists for. Both probe roles also stand themselves down after a
//! minute regardless of what the test does, so a regression that leaks one leaks
//! it for a minute and not for four hours.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leviculum_std::process::spawn_supervised;

/// The two-role helper. Cargo builds it for this test and hands us its path, so
/// there is no locate-or-build dance and no chance of testing a stale copy.
const PROBE: &str = env!("CARGO_BIN_EXE_supervised-spawn-probe");

/// How long the test waits for the child to appear and report itself.
const REPORT_DEADLINE: Duration = Duration::from_secs(20);
/// How long the test waits for a supervised child to disappear after its parent
/// is `SIGKILL`ed — and, in the control arm, how long a bare child must survive.
///
/// The kernel delivers `PDEATHSIG` from the dying parent's own exit path, so the
/// real figure is microseconds. Five seconds is orders of magnitude of slack for
/// a loaded CI host, and it is a *deadline*, not a sleep: the passing arm returns
/// on its first poll.
const KILL_DEADLINE: Duration = Duration::from_secs(5);
/// Poll interval while watching `/proc`.
const POLL: Duration = Duration::from_millis(25);

/// A `SIGKILL`ed parent takes its supervised child with it.
#[test]
fn a_sigkilled_parent_takes_its_supervised_child_with_it() {
    let arm = run_arm("supervised");

    assert_eq!(
        arm.child_pdeathsig,
        libc::SIGKILL,
        "the helper no longer sets PR_SET_PDEATHSIG on the child \
         (got {}, want SIGKILL): the pre_exec has been dropped or defeated",
        arm.child_pdeathsig,
    );

    let Some(elapsed) = arm.gone_after else {
        // Loudly, and bounded: say the child is still alive and reap it, rather
        // than leaving the rest of the suite to find out.
        kill_marked(arm.child_pid, &arm.marker);
        panic!(
            "child {} outlived its SIGKILLed parent by more than {:?}; \
             PR_SET_PDEATHSIG is not in force",
            arm.child_pid, KILL_DEADLINE,
        );
    };
    assert!(elapsed <= KILL_DEADLINE);
}

/// The same experiment without the fix: the child survives, which is what makes
/// the arm above a measurement rather than a tautology.
#[test]
fn without_the_fix_the_child_survives_the_same_kill() {
    let arm = run_arm("bare");

    assert_eq!(
        arm.child_pdeathsig, 0,
        "the control arm was armed after all, so it is no longer a control",
    );

    let outcome = arm.gone_after;
    // Reap before asserting: a failing control must not also leak.
    kill_marked(arm.child_pid, &arm.marker);

    assert!(
        outcome.is_none(),
        "the bare-spawned child died {:?} after its parent was SIGKILLed. \
         Something other than PR_SET_PDEATHSIG is reaping these processes, so \
         the arm above proves nothing about the kernel.",
        outcome.unwrap(),
    );
}

// =========================================================================
// Machinery
// =========================================================================

struct Arm {
    child_pid: i32,
    child_pdeathsig: i32,
    /// `Some(elapsed)` if the child was gone within [`KILL_DEADLINE`].
    gone_after: Option<Duration>,
    marker: String,
}

/// Run one arm end to end: spawn the parent, wait for the child to report, kill
/// the parent, watch the child.
fn run_arm(mode: &str) -> Arm {
    let dir = tempfile::tempdir().expect("arm work dir");
    let report = dir.path().join("child-report");
    // The report path is unique to this arm and is the child's `argv[2]`, so it
    // doubles as the identity check in `/proc`: a recycled pid is not our child.
    let marker = report.display().to_string();

    let mut cmd = Command::new(PROBE);
    cmd.args(["parent", mode])
        .arg(&report)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Supervised even here: if the test binary itself dies, the whole
    // experiment goes with it rather than becoming the leak it is about.
    let mut parent = spawn_supervised(cmd).expect("spawn the probe parent");

    let (child_pid, child_pdeathsig) = read_report(&report, REPORT_DEADLINE);
    assert!(
        marked_alive(child_pid, &marker),
        "the {mode} child ({child_pid}) was not alive before its parent was killed"
    );

    // SIGKILL: the parent runs no destructor, no handler, no line of our code.
    // SAFETY: `parent` has not been reaped, so its pid is still ours to signal.
    let rc = unsafe { libc::kill(parent.id() as libc::pid_t, libc::SIGKILL) };
    assert_eq!(
        rc,
        0,
        "kill the probe parent: {}",
        std::io::Error::last_os_error()
    );
    parent.wait().expect("reap the probe parent");

    let started = Instant::now();
    let mut gone_after = None;
    while started.elapsed() < KILL_DEADLINE {
        if !marked_alive(child_pid, &marker) {
            gone_after = Some(started.elapsed());
            break;
        }
        std::thread::sleep(POLL);
    }

    Arm {
        child_pid,
        child_pdeathsig,
        gone_after,
        marker,
    }
}

/// Poll `path` for the child's `<pid> <pdeathsig>` line, failing on the deadline
/// rather than waiting forever.
fn read_report(path: &Path, deadline: Duration) -> (i32, i32) {
    let started = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let mut fields = text.split_whitespace();
            if let (Some(pid), Some(sig)) = (fields.next(), fields.next()) {
                return (
                    pid.parse().expect("child pid"),
                    sig.parse().expect("child pdeathsig"),
                );
            }
        }
        assert!(
            started.elapsed() < deadline,
            "no child report at {} within {deadline:?}",
            path.display()
        );
        std::thread::sleep(POLL);
    }
}

/// Is `pid` a live process whose command line still carries `marker`?
///
/// The command-line check is what makes this safe against pid reuse: a recycled
/// pid is a different process with a different `argv`, and a zombie's `cmdline`
/// reads empty because its address space is already gone.
fn marked_alive(pid: i32, marker: &str) -> bool {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => bytes.split(|b| *b == 0).any(|arg| arg == marker.as_bytes()),
        Err(_) => false,
    }
}

/// Kill `pid`, but only while it is still the process the marker names.
fn kill_marked(pid: i32, marker: &str) {
    if marked_alive(pid, marker) {
        // SAFETY: the marker check above establishes that `pid` is still our
        // child rather than a recycled number.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}
