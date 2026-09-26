//! Spawning an external process that the kernel takes down with us.
//!
//! The rule this module implements is in
//! `docs/src/concepts/checks-and-citations.md`:
//!
//! > A harness that spawns a long-lived external process must ensure it dies
//! > with the harness, however the harness dies. Cleanup code is a
//! > convenience; the kernel is the guarantee.
//!
//! `Drop` satisfies "however" only for the clean path. An abort — a panic in a
//! destructor during cleanup, which is exactly what happened on 2026-08-07 —
//! skips unwinding, so no destructor runs; a `SIGKILL` of the parent skips
//! everything by definition. Seven orphaned `scripts/test_daemon.py` processes
//! were found alive that afternoon, the oldest over four hours old and from
//! several different runs, and one of them held a pipe open and hung
//! `just standard` for two hours with its verdict already decided.
//!
//! The mechanism is `PR_SET_PDEATHSIG`: the kernel signals the child when its
//! parent goes away, whatever the parent's cause of death. It is set in the
//! child between `fork` and `exec`, via
//! [`CommandExt::pre_exec`](std::os::unix::process::CommandExt::pre_exec).
//!
//! # The four traps
//!
//! **1. `PR_SET_PDEATHSIG` is per-thread, not per-process.** The kernel stores
//! it on the child task and delivers it from `forget_original_parent()`, which
//! runs when the *forking task* exits — not when the forking task's process
//! exits. A tokio worker thread or a `spawn_blocking` thread that finishes
//! while the test is still running would therefore kill the daemon under it.
//! That turns the fix into a flake generator, so it is the trap this module is
//! shaped around: **every supervised spawn is forked from one dedicated thread
//! that never exits**, so the only event that ends it is the
//! process ending. Note that trap 2's `getppid()` check cannot substitute for
//! this: `getppid()` reports the parent's *thread group leader*, so a forking
//! thread exiting while its process lives leaves it unchanged.
//!
//! **2. The race between `fork` and `prctl`.** If the parent dies inside that
//! window the signal is already missed, and the child would run on forever.
//! After setting the flag the child re-reads `getppid()` and `_exit`s with
//! [`ORPHANED_EXIT_CODE`] if it is no longer the process that spawned it.
//!
//! **3. It does not reach grandchildren.** A process spawned this way is
//! deliberately *not* `setsid()`-ed and stays in its parent's process group,
//! so `scripts/run-with-manifest.py` — which kills the whole group after the
//! gate's child exits — still reaches everything below it. `setsid()` here
//! would buy nothing and would break that net, which the concept page names as
//! the one thing the wrapper cannot reach. Where a supervised process spawns
//! its own long-lived children, that is a separate link and needs the same
//! treatment at its own spawn site.
//!
//! **4. Signal choice: `SIGKILL`.** `PDEATHSIG` fires only in the state where
//! the parent is *already dead*, so there is nobody left to wait for a polite
//! exit, and a signal the child may catch and ignore is not a guarantee — it is
//! the same "cleanup that usually runs" the mechanism exists to replace. The
//! polite path is still tried first, by the owning `Drop`; this is the backstop
//! underneath it, and the backstop ends in the same `SIGKILL` those destructors
//! already end in. What `SIGKILL` gives up is the child's own last-wishes code:
//! `scripts/test_daemon.py` removes its `mkdtemp` config directory in a
//! `finally:` block, and under `SIGKILL` that directory is left behind. Ports,
//! listening sockets, ptys and `flock`s are released by the kernel on death, so
//! the leak is bounded to a few KiB under `/tmp` in a run whose parent has
//! already crashed.

use std::io;
use std::process::{Child, Command};
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};
use std::sync::OnceLock;

/// Exit code of a supervised child that lost its parent between `fork` and
/// `prctl` (trap 2 above) and stood itself down rather than run on orphaned.
///
/// Outside the ranges a shell assigns (126, 127, 128+n) so it is not confused
/// with "could not execute" or "killed by signal".
pub const ORPHANED_EXIT_CODE: i32 = 190;

/// A spawn request handed to the one thread allowed to fork.
enum Job {
    Std(Command, SyncSender<io::Result<Child>>),
    Async(
        tokio::process::Command,
        tokio::runtime::Handle,
        SyncSender<io::Result<tokio::process::Child>>,
    ),
}

/// The channel to the process-wide forking thread, created on first use.
///
/// The thread must outlive every process it forks (trap 1), which here means
/// "must never exit": the `Sender` lives in a `'static` `OnceLock` and is
/// therefore never dropped, so the receive loop cannot end, and the `park()`
/// below is the belt to that braces.
fn spawner() -> &'static Sender<Job> {
    static SPAWNER: OnceLock<Sender<Job>> = OnceLock::new();
    SPAWNER.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("pdeathsig-spawner".to_owned())
            .spawn(move || {
                for job in &rx {
                    match job {
                        Job::Std(mut cmd, reply) => {
                            let _ = reply.send(cmd.spawn());
                        }
                        Job::Async(mut cmd, handle, reply) => {
                            // tokio registers the child with the runtime that
                            // is current at spawn time; this thread has none of
                            // its own, so it borrows the caller's.
                            let _guard = handle.enter();
                            let _ = reply.send(cmd.spawn());
                        }
                    }
                }
                loop {
                    std::thread::park();
                }
            })
            .expect("spawn the pdeathsig forking thread");
        tx
    })
}

/// Arm `cmd` so the kernel `SIGKILL`s the child when this process dies.
///
/// Everything inside the closure runs between `fork` and `exec` in a process
/// that holds a copy of our address space and exactly one thread, so it must be
/// async-signal-safe: no allocation, no locks, no non-reentrant libc. `prctl`,
/// `getppid` and `_exit` are all on the async-signal-safe list, the closure
/// captures one `pid_t` by copy, and `io::Error::last_os_error` only reads
/// `errno` into a packed repr.
///
/// `PR_SET_PDEATHSIG` survives the `execve` that follows (it is cleared only
/// across a set-user-ID / set-group-ID / file-capabilities exec), which is what
/// makes setting it here work at all.
#[cfg(target_os = "linux")]
fn arm(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    let parent = std::process::id() as libc::pid_t;
    // SAFETY: see the async-signal-safety argument above. The closure calls
    // only async-signal-safe libc functions and captures a single `pid_t`.
    unsafe {
        cmd.pre_exec(move || {
            // glibc's and musl's `prctl` both read four `unsigned long`
            // varargs unconditionally and pass all of them to the syscall, so
            // all four are supplied even though the kernel reads only arg2 for
            // this option.
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            // Trap 2. If the parent died inside the fork → prctl window the
            // signal is already missed, and `getppid()` now names the reaper
            // instead. Stand down rather than run on orphaned. The parent, if
            // any is left to look, sees a `Child` that has already exited:
            // `_exit` closes the CLOEXEC error pipe, which `Command::spawn`
            // reads as a successful exec.
            if libc::getppid() != parent {
                libc::_exit(ORPHANED_EXIT_CODE);
            }
            Ok(())
        });
    }
}

/// No kernel-enforced parent-death link outside Linux; the caller's `Drop` is
/// then the only cleanup, with the limits documented at the top of this module.
#[cfg(not(target_os = "linux"))]
fn arm(_cmd: &mut Command) {}

/// Spawn `cmd` so that the kernel `SIGKILL`s it when this process dies, however
/// this process dies.
///
/// The command is taken by value rather than as `&mut Command` because the fork
/// has to happen on the forking thread and not on the caller's. That is not a
/// detail of the plumbing — it is trap 1, and the by-value signature is what
/// makes a supervised spawn look different from a bare one at every call site,
/// which is what `scripts/check-supervised-spawns.py` reads.
///
/// ```no_run
/// use std::process::{Command, Stdio};
/// use leviculum_std::process::spawn_supervised;
///
/// let mut cmd = Command::new("python3");
/// cmd.arg("daemon.py").stdout(Stdio::piped());
/// let child = spawn_supervised(cmd)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn spawn_supervised(mut cmd: Command) -> io::Result<Child> {
    arm(&mut cmd);
    let (tx, rx) = sync_channel(0);
    dispatch(Job::Std(cmd, tx))?;
    rx.recv().map_err(spawner_died)?
}

/// [`spawn_supervised`] for a `tokio::process::Command`.
///
/// Must be called from within a tokio runtime: the child is registered with the
/// runtime that is current on the *caller's* thread, which the forking thread
/// enters for the duration of the spawn.
pub fn spawn_supervised_async(
    mut cmd: tokio::process::Command,
) -> io::Result<tokio::process::Child> {
    arm(cmd.as_std_mut());
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|e| io::Error::other(format!("supervised spawn needs a tokio runtime: {e}")))?;
    let (tx, rx) = sync_channel(0);
    dispatch(Job::Async(cmd, handle, tx))?;
    rx.recv().map_err(spawner_died)?
}

fn dispatch(job: Job) -> io::Result<()> {
    spawner().send(job).map_err(|_| spawner_died_raw())
}

fn spawner_died<E>(_: E) -> io::Error {
    spawner_died_raw()
}

fn spawner_died_raw() -> io::Error {
    io::Error::other("the pdeathsig forking thread is gone")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// `sh -c "exit N"` where there is a sh, `cmd /C exit N` where there is
    /// not. What is under test is the spawn plumbing, not the shell.
    fn exit_with(code: u32) -> Command {
        #[cfg(unix)]
        let mut cmd = Command::new("/bin/sh");
        #[cfg(unix)]
        cmd.arg("-c").arg(format!("exit {code}"));
        #[cfg(windows)]
        let mut cmd = Command::new("cmd");
        #[cfg(windows)]
        cmd.arg("/C").arg(format!("exit {code}"));
        cmd
    }

    /// The helper is a spawn, not only a flag-setter: an ordinary command runs
    /// and reports its status through it.
    #[test]
    fn a_supervised_child_runs_and_is_reaped_normally() {
        let mut cmd = exit_with(7);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = spawn_supervised(cmd).expect("spawn");
        let status = child.wait().expect("wait");
        assert_eq!(status.code(), Some(7));
    }

    /// The forking thread is a singleton and is reused: two spawns in a row
    /// must both succeed, which they cannot if the first one ended the loop.
    #[test]
    fn the_forking_thread_serves_more_than_one_spawn() {
        for _ in 0..3 {
            let mut cmd = exit_with(0);
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let mut child = spawn_supervised(cmd).expect("spawn");
            assert!(child.wait().expect("wait").success());
        }
    }

    /// A spawn failure still surfaces as an error rather than as a hang or a
    /// panic on the forking thread.
    #[test]
    fn a_missing_program_is_an_error_not_a_hang() {
        let cmd = Command::new("/nonexistent/leviculum-supervised-spawn-probe");
        assert!(spawn_supervised(cmd).is_err());
    }

    #[tokio::test]
    async fn a_supervised_async_child_runs() {
        let mut cmd = tokio::process::Command::from(exit_with(3));
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = spawn_supervised_async(cmd).expect("spawn");
        let status = child.wait().await.expect("wait");
        assert_eq!(status.code(), Some(3));
    }
}
