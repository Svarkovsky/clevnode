//! Helper binary for `leviculum-std/tests/supervised_spawn.rs`.
//!
//! Two roles, selected by `argv[1]`:
//!
//! ```text
//! supervised-spawn-probe child  <report-path>
//! supervised-spawn-probe parent <supervised|bare> <report-path>
//! ```
//!
//! `parent` spawns one `child` — through
//! [`leviculum_std::process::spawn_supervised`] or through a
//! bare [`Command::spawn`] for the negative control — and then sits still until
//! the test `SIGKILL`s it. `child` reports its pid and its own
//! `PR_GET_PDEATHSIG`, then sleeps.
//!
//! # Why a binary and not the test binary re-run with `--exact`
//!
//! `PR_SET_PDEATHSIG` is stored per *task*, and `copy_process()` clears it for
//! every new task — including threads. libtest runs a test body on a spawned
//! thread even under `--test-threads=1`, so a child role written as a `#[test]`
//! reads its own parent-death signal as 0 while the process's main thread
//! carries `SIGKILL`. The measurement was wrong, not the mechanism. A plain
//! `fn main` reads it on the task that actually holds it.
//!
//! That is the same fact as trap 1 in [`leviculum_std::process`], observed from
//! the other side: per-task means per-task in both directions.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use leviculum_std::process::spawn_supervised;

/// Self-bound on both roles. A regression that leaks one of these leaks it for
/// a minute, not for the four hours the 2026-08-07 orphans managed.
const MAX_LIFETIME: Duration = Duration::from_secs(60);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("child") => child(Path::new(args.get(2).unwrap_or_else(|| usage()))),
        Some("parent") => parent(
            args.get(2).unwrap_or_else(|| usage()),
            Path::new(args.get(3).unwrap_or_else(|| usage())),
        ),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: supervised-spawn-probe child <report-path>\n\
         \x20      supervised-spawn-probe parent <supervised|bare> <report-path>"
    );
    std::process::exit(2);
}

/// Report `<pid> <pdeathsig>` and then sleep.
fn child(report: &Path) {
    let line = format!("{} {}\n", std::process::id(), pdeathsig());
    // Write-then-rename, because the test polls this path and a half-written
    // line would parse as a pid that is not ours.
    let partial = report.with_extension("partial");
    let mut f = std::fs::File::create(&partial).expect("create the report");
    f.write_all(line.as_bytes()).expect("write the report");
    f.sync_all().expect("flush the report");
    drop(f);
    std::fs::rename(&partial, report).expect("publish the report");

    std::thread::sleep(MAX_LIFETIME);
}

/// Spawn one child the way `mode` asks for, then sit still.
///
/// Deliberately does nothing after the spawn. Anything it did once the test
/// `SIGKILL`s it would be evidence about our code rather than about the kernel
/// — and after `SIGKILL` there is nothing it *can* do, which is the point.
// The child is never waited on here on purpose: this process is about to be
// SIGKILLed, and reaping the child is the test's job — the whole experiment is
// about what happens to it when nothing in this process runs again.
#[allow(clippy::zombie_processes)]
fn parent(mode: &str, report: &Path) {
    let exe = std::env::current_exe().expect("own path");
    let mut cmd = Command::new(exe);
    cmd.arg("child")
        .arg(report)
        // The child must inherit none of our pipes. A leaked grandchild holding
        // a gate's stdout is exactly how `just standard` was held open for two
        // hours on 2026-08-07.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let _child = match mode {
        // The negative control: the pre-fix shape, kept executable so the proof
        // has something to be a proof *against*. This is the one bare process
        // spawn scripts/check-supervised-spawns.py allowlists.
        "bare" => cmd.spawn().expect("bare spawn"),
        "supervised" => spawn_supervised(cmd).expect("supervised spawn"),
        _ => usage(),
    };

    std::thread::sleep(MAX_LIFETIME);
}

/// This task's parent-death signal, 0 when none is set.
#[cfg(target_os = "linux")]
fn pdeathsig() -> i32 {
    let mut sig: libc::c_int = 0;
    // SAFETY: `PR_GET_PDEATHSIG` writes one `int` through arg2 and reads
    // nothing else; `sig` is a live, aligned, exclusively borrowed `c_int`.
    // glibc and musl both read four `unsigned long` varargs unconditionally, so
    // all four are supplied.
    let rc = unsafe {
        libc::prctl(
            libc::PR_GET_PDEATHSIG,
            &mut sig as *mut libc::c_int,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    assert_eq!(
        rc,
        0,
        "PR_GET_PDEATHSIG: {}",
        std::io::Error::last_os_error()
    );
    sig
}

/// No `PR_GET_PDEATHSIG` outside Linux — same split as `process::arm`. The
/// probe still builds and reports its pid everywhere (the Windows and macOS
/// lanes build every bin), and 0 (none set) is the only honest answer.
#[cfg(not(target_os = "linux"))]
fn pdeathsig() -> i32 {
    0
}
