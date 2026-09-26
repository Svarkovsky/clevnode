//! Poison-tolerant locking for the daemon's `std::sync::Mutex` state, plus a
//! self-deadlock tripwire (Codeberg #198).
//!
//! The whole node state (`StdNodeCore`) and several interface-side maps live
//! behind `std::sync::Mutex`. A `std::sync::Mutex` POISONS: if any task panics
//! while holding the guard, every subsequent `.lock().unwrap()` panics too,
//! turning one isolated task panic into a whole-daemon crash cascade.
//!
//! For a long-running network daemon that must not fall over because one
//! interface had a bad moment, continuing degraded beats crashing. This trait
//! recovers the guard on poison instead of propagating the panic.
//!
//! # The tripwire
//!
//! `93ba351` shipped a seam ([`crate::driver::CoreProcessor`]) that runs
//! consumer code while the driver holds the core lock. `std::sync::Mutex` is
//! not reentrant, so any handle the consumer smuggled in that re-locks the core
//! hangs the node — in safe synchronous code, with no `.await`, with nothing
//! for a compiler to catch. `ReticulumNode::has_path` is one line and one of
//! roughly forty.
//!
//! A deadlock is the worst failure this daemon produces: no stack, no log, no
//! exit code. The node stops and looks alive to every supervisor. So every
//! acquisition records the mutex's address in a thread-local set and checks it
//! first: an address already present means *this thread* is about to wait for a
//! lock only *this thread* can release. That never wakes, so we say so instead
//! of hanging. See [`ReentrantLock`] for the shape and the reasoning, and
//! `docs/src/concepts/self-deadlock-tripwire.md` for the decisions.

use std::cell::Cell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Flips true the first time any poisoned mutex is recovered in this process,
/// so the warning below fires once instead of on every relock of a mutex that
/// stays poisoned forever.
static POISON_WARNED: AtomicBool = AtomicBool::new(false);

/// How many simultaneously-held mutexes one thread is tracked for.
///
/// Nesting depth in this crate is 1 in the overwhelming majority of frames and
/// 4 at the deepest point measured (`dispatch_output` taking the core lock and
/// then the three interface maps). 32 is two orders of magnitude of headroom in
/// a fixed-size array, which is what keeps the fast path free of an allocation
/// and of a `RefCell` borrow flag.
///
/// A thread that exceeds it is not silently untracked — see
/// [`HeldLocks::overflowed`].
const MAX_TRACKED_DEPTH: usize = 32;

/// The mutex addresses this thread currently holds through [`MutexRecover`].
///
/// `Cell` rather than `RefCell`: there is no borrow to hold across a call, so
/// the flag would be pure overhead. A fixed array rather than a `Vec`: the
/// first acquisition on a thread would otherwise allocate, and this sits under
/// every lock in the crate.
struct HeldLocks {
    /// Number of live entries in `addrs`.
    depth: Cell<usize>,
    addrs: [Cell<usize>; MAX_TRACKED_DEPTH],
    /// Set if this thread ever exceeded [`MAX_TRACKED_DEPTH`].
    ///
    /// Past that point the set is incomplete, so a *negative* answer stops
    /// meaning anything and the tripwire would degrade to a check that has
    /// quietly stopped checking — the failure mode
    /// `docs/src/concepts/checks-and-citations.md` exists to remove. It is
    /// reported once, loudly, rather than absorbed.
    overflowed: Cell<bool>,
}

thread_local! {
    /// `const`-initialised on purpose: a `thread_local!` with a const
    /// initialiser and no `Drop` compiles to a plain `#[thread_local]` access
    /// with no lazy-initialisation branch, which is what makes the fast path
    /// cost a register-relative load rather than a TLS-init check.
    static HELD: HeldLocks = const {
        HeldLocks {
            depth: Cell::new(0),
            addrs: [const { Cell::new(0) }; MAX_TRACKED_DEPTH],
            overflowed: Cell::new(false),
        }
    };
}

/// What the tripwire found: this thread already holds the mutex it is about to
/// block on.
///
/// Carried out of the check as a value rather than reported inside it so the
/// cold path stays out of the inlined fast path.
struct ReentrantLock {
    addr: usize,
    depth: usize,
}

/// Record `addr` as held by this thread, or report that it already is.
///
/// Returns `Err` exactly when this thread would be waiting for itself.
#[inline]
fn push_held(addr: usize) -> Result<bool, ReentrantLock> {
    HELD.with(|held| {
        let depth = held.depth.get();
        for slot in held.addrs.iter().take(depth) {
            if slot.get() == addr {
                return Err(ReentrantLock { addr, depth });
            }
        }
        if depth == MAX_TRACKED_DEPTH {
            // Report once per thread, then carry on untracked. Losing the
            // tripwire is worth saying out loud; losing the daemon is not.
            if !held.overflowed.replace(true) {
                tracing::error!(
                    event = "LOCK_DEPTH_OVERFLOW",
                    max_depth = MAX_TRACKED_DEPTH,
                    "this thread holds more than {MAX_TRACKED_DEPTH} mutexes at once; \
                     the self-deadlock tripwire is INACTIVE on this thread from here on"
                );
            }
            return Ok(false);
        }
        held.addrs[depth].set(addr);
        held.depth.set(depth + 1);
        Ok(true)
    })
}

/// Drop `addr` from this thread's held set.
///
/// Removal is **by address, not by position**. Guards are values and nothing
/// forces them to be dropped in acquisition order — `let a = m1.lock_recover();
/// let b = m2.lock_recover(); drop(a);` is ordinary code — so popping the top
/// would evict the wrong entry and leave a stale address behind, which is a
/// false positive on the next acquisition of `m2`. The vacated slot is filled
/// from the top, which is order-destroying and set-preserving; order is not
/// something this structure means anything by.
#[inline]
fn pop_held(addr: usize) {
    HELD.with(|held| {
        let depth = held.depth.get();
        for i in (0..depth).rev() {
            if held.addrs[i].get() == addr {
                held.addrs[i].set(held.addrs[depth - 1].get());
                held.depth.set(depth - 1);
                return;
            }
        }
    });
}

/// Report a self-deadlock and take the process's thread down with a panic.
///
/// # Why panic, and why in release too
///
/// The alternative is to log and let the `lock()` below block, which is not
/// degraded operation: it is a stopped thread wearing a healthy face. Do not
/// read `MutexRecover`'s poison trade-off as precedent — there, continuing
/// really does keep the node serving the mesh, because the panicking task has
/// already unwound and every other task still runs. Here the thread that is
/// about to block is usually the driver's event loop, and once it stops the
/// node forwards nothing, answers no path requests and maintains no link while
/// still holding its listening socket open. Priority 1 is packet delivery;
/// zero is the worst delivery there is, and a supervisor cannot see it.
///
/// Panicking is also the *cheapest possible* recovery for the hazard #198 is
/// about, because the seam already catches it. `run_event_tap` and `run_tick`
/// wrap each hook in `catch_unwind`: the unwind releases the outer core guard
/// (poisoned, which `lock_recover` recovers), the driver logs
/// `CORE_PROCESSOR_PANICKED`, detaches the offending processor permanently,
/// emits `NodeEvent::CoreProcessorPanicked`, and carries on serving the mesh
/// without it. That is the graceful degradation the project policy asks for,
/// and log-and-hang forfeits it.
///
/// Away from the seam a panic here means an ordinary lock-order bug in our own
/// code, on a path a test exercises. Failing loudly at the point of the defect
/// beats hanging at it.
#[cold]
#[inline(never)]
fn report_reentrant_lock(found: ReentrantLock) -> ! {
    let ReentrantLock { addr, depth } = found;
    // The structured line first, so it reaches the event log even if the panic
    // machinery is what goes wrong next.
    tracing::error!(
        event = "REENTRANT_LOCK",
        mutex = format_args!("{addr:#x}"),
        held_depth = depth,
    );
    let backtrace = std::backtrace::Backtrace::force_capture();
    panic!(
        "re-entrant lock: this thread already holds the mutex at {addr:#x} and \
         is locking it again, which can never succeed (std::sync::Mutex is not \
         reentrant). This thread holds {depth} lock(s). The usual cause is a \
         CoreProcessor hook calling back into the node it runs inside — every \
         synchronous ReticulumNode accessor re-locks the core; use the \
         `&mut StdNodeCore` handle instead (Codeberg #198).\n{backtrace}"
    );
}

/// Lock a `std::sync::Mutex`, recovering the guard if the mutex is poisoned.
pub(crate) trait MutexRecover<T> {
    /// Acquire the lock, ignoring poison.
    ///
    /// Trade-off: on poison we return the guard anyway
    /// (via [`std::sync::PoisonError::into_inner`]), so the protected state may reflect a
    /// partially-applied mutation from the task that panicked while holding the
    /// lock. We accept that because for a daemon, continuing degraded beats
    /// crashing every later locker — and the panic that poisoned the mutex has
    /// already unwound and logged on its own task. Use this instead of
    /// `.lock().unwrap()` / `.lock().expect(...)` on every non-test std mutex.
    ///
    /// # Panics
    ///
    /// If this thread already holds this mutex. See [`report_reentrant_lock`].
    fn lock_recover(&self) -> TrackedGuard<'_, T>;
}

impl<T> MutexRecover<T> for Mutex<T> {
    #[inline]
    fn lock_recover(&self) -> TrackedGuard<'_, T> {
        let addr = std::ptr::from_ref(self) as usize;
        // Checked BEFORE `lock()`. After it there is nothing left to report
        // from: the thread is already parked forever.
        let tracked = match push_held(addr) {
            Ok(tracked) => tracked,
            Err(found) => report_reentrant_lock(found),
        };
        let guard = self.lock().unwrap_or_else(|poison| {
            // Cold path: a task panicked while holding this lock. Surface it
            // once — a poisoned mutex stays poisoned, so an unguarded warn
            // would spam on every subsequent relock.
            if !POISON_WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "a task panicked while holding a lock; the daemon is \
                     recovering and continuing in a degraded state (node \
                     state may be mid-mutation). Further poison recoveries \
                     this process are silent."
                );
            }
            poison.into_inner()
        });
        TrackedGuard {
            guard,
            addr: tracked.then_some(addr),
        }
    }
}

/// A `MutexGuard` that unregisters its mutex from the thread's held set when it
/// drops.
///
/// Transparent by `Deref`/`DerefMut`, so call sites read exactly as they did
/// when `lock_recover` returned a `MutexGuard` directly. It is `!Send` for the
/// same reason a `MutexGuard` is, which is also what makes the thread-local set
/// sound: a registration can never be observed from a thread other than the one
/// that made it.
pub(crate) struct TrackedGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    /// `None` when the acquisition went untracked (depth overflow), so `Drop`
    /// does not evict an address it never inserted.
    addr: Option<usize>,
}

impl<T> Deref for TrackedGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for TrackedGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

/// Mirrors `MutexGuard`'s own `Debug`, so `{:?}` on a guard keeps working.
impl<T: std::fmt::Debug> std::fmt::Debug for TrackedGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.guard, f)
    }
}

/// Mirrors `MutexGuard`'s own `Display`, for the same reason.
impl<T: std::fmt::Display> std::fmt::Display for TrackedGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.guard, f)
    }
}

impl<T> Drop for TrackedGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // Runs before the fields drop, so the address leaves the set a moment
        // before the mutex actually unlocks. Nothing can observe that window:
        // the only reader of the set is this thread, and this thread is inside
        // `drop`.
        if let Some(addr) = self.addr {
            pop_held(addr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::Arc;

    /// The panic message a `catch_unwind`ed [`report_reentrant_lock`] carries,
    /// or `None` if the closure did not panic.
    fn panic_message<R>(f: impl FnOnce() -> R) -> Option<String> {
        // The default hook would print a backtrace per expected panic and bury
        // the run's real output.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = catch_unwind(AssertUnwindSafe(f));
        std::panic::set_hook(previous);
        match result {
            Ok(_) => None,
            Err(payload) => Some(
                payload
                    .downcast_ref::<&'static str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string()),
            ),
        }
    }

    /// Standing canary, positive half (Codeberg #198,
    /// `docs/src/concepts/checks-and-citations.md` §Standing canaries).
    ///
    /// A tripwire that has silently stopped tripping satisfies "no deadlock
    /// detected" forever, so a re-entry the detector is *meant* to see is
    /// re-proved on every run rather than demonstrated once at implementation
    /// time. Paired with
    /// [`canary_distinct_mutexes_nest_without_tripping`], which must not fire.
    #[test]
    fn canary_re_entering_one_mutex_trips() {
        let m = Mutex::new(0_u32);
        let message = panic_message(|| {
            let _outer = m.lock_recover();
            let _inner = m.lock_recover();
        })
        .expect("re-locking a held mutex on the same thread must be reported");
        assert!(
            message.contains("re-entrant lock"),
            "the report must name the failure: {message}"
        );
        assert!(
            message.contains(&format!("{:#x}", std::ptr::from_ref(&m) as usize)),
            "the report must name the mutex: {message}"
        );
    }

    /// Standing canary, negative half: legitimately nested *different* mutexes
    /// must not fire. Without this the positive canary is satisfied by a
    /// tripwire that reports everything.
    #[test]
    fn canary_distinct_mutexes_nest_without_tripping() {
        let a = Mutex::new(1_u32);
        let b = Mutex::new(2_u32);
        let c = Mutex::new(3_u32);
        let ga = a.lock_recover();
        let gb = b.lock_recover();
        let gc = c.lock_recover();
        assert_eq!((*ga, *gb, *gc), (1, 2, 3));
    }

    /// The set is keyed by address, so a guard released before an outer one
    /// must evict *its own* entry. Popping the top instead would leave the
    /// inner address behind and report a false positive here.
    #[test]
    fn out_of_order_guard_drops_leave_no_stale_entry() {
        let a = Mutex::new(1_u32);
        let b = Mutex::new(2_u32);

        let ga = a.lock_recover();
        let gb = b.lock_recover();
        drop(ga); // the OUTER guard first
        drop(gb);

        // Both must now be re-lockable. If `pop_held` popped positionally,
        // `a`'s address would still sit in the set and this would panic.
        assert!(
            panic_message(|| {
                let _ = a.lock_recover();
            })
            .is_none(),
            "re-locking after an out-of-order drop must not be reported"
        );
        assert!(
            panic_message(|| {
                let _ = b.lock_recover();
            })
            .is_none(),
            "re-locking after an out-of-order drop must not be reported"
        );
    }

    /// Sequential acquisitions of the same mutex are the common case and must
    /// stay silent: the tripwire keys on *simultaneously* held, not on
    /// ever-held.
    #[test]
    fn sequential_acquisitions_do_not_trip() {
        let m = Mutex::new(0_u32);
        for i in 0..1000_u32 {
            let mut g = m.lock_recover();
            *g = i;
        }
        assert_eq!(*m.lock_recover(), 999);
    }

    /// The set is per-thread: another thread holding the mutex is an ordinary
    /// wait, not a self-deadlock, and must not be reported.
    #[test]
    fn another_thread_holding_the_mutex_is_not_reentrancy() {
        let m = Arc::new(Mutex::new(0_u32));
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = Arc::clone(&m);
        let t = std::thread::spawn(move || {
            let _g = holder.lock_recover();
            tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        rx.recv().unwrap();
        // This thread has never locked `m`, so its own set is empty: no report,
        // just an ordinary block until the other thread lets go.
        release_tx.send(()).unwrap();
        t.join().unwrap();
        let _g = m.lock_recover();
    }

    #[test]
    fn lock_recover_returns_usable_guard_after_poison() {
        let m = Arc::new(Mutex::new(41_u32));

        // Poison the mutex: a thread panics while holding the guard.
        let poisoner = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let mut g = poisoner.lock().unwrap();
            *g = 1; // partially-applied mutation before the panic
            panic!("poison");
        })
        .join();

        // RED-before: a plain `.lock().unwrap()` panics on the poisoned mutex.
        let plain = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = m.lock().unwrap();
        }));
        assert!(
            plain.is_err(),
            "plain lock().unwrap() must panic when poisoned"
        );

        // GREEN-after: `lock_recover()` returns a usable guard, no panic, and
        // the value is still readable and writable.
        let mut g = m.lock_recover();
        let observed = *g;
        *g = 99;
        drop(g);
        assert_eq!(observed, 1, "recovered guard exposes the poisoned state");
        assert_eq!(*m.lock_recover(), 99, "recovered guard is writable");
    }

    #[test]
    fn lock_recover_warns_once_on_poison() {
        let m = Arc::new(Mutex::new(0_u32));

        // Poison the mutex, then recover it. Recovery must still hand back a
        // usable guard.
        let poisoner = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = poisoner.lock().unwrap();
            panic!("poison");
        })
        .join();
        let _g = m.lock_recover();
        drop(_g);

        // The process-wide once-guard is now tripped. It is monotonic (never
        // reset outside this assertion path), so a second recovery observes it
        // already set and skips the warn: `swap` returns the prior `true`.
        assert!(
            POISON_WARNED.load(Ordering::Relaxed),
            "recovering a poisoned mutex must trip the once-guard"
        );
        assert!(
            POISON_WARNED.swap(true, Ordering::Relaxed),
            "a second poison recovery must not re-warn"
        );
    }

    #[test]
    fn lock_recover_matches_lock_when_unpoisoned() {
        let m = Mutex::new(7_u32);
        {
            let mut g = m.lock_recover();
            assert_eq!(*g, 7);
            *g = 8;
        }
        assert_eq!(*m.lock().unwrap(), 8);
    }

    /// An unwind out of a hook must leave the set clean, or the next
    /// acquisition on the same thread reports a re-entry that is not one — the
    /// tripwire's own worst failure mode, since it would fire in production on
    /// a path that merely panicked.
    #[test]
    fn a_panic_through_a_guard_leaves_no_stale_entry() {
        let m = Mutex::new(0_u32);
        let first = panic_message(|| {
            let _g = m.lock_recover();
            panic!("unwind past the guard");
        });
        assert_eq!(first.as_deref(), Some("unwind past the guard"));
        assert!(
            panic_message(|| {
                let _ = m.lock_recover();
            })
            .is_none(),
            "the guard's Drop must have run during the unwind"
        );
    }

    /// Past [`MAX_TRACKED_DEPTH`] the set is incomplete, so it says so once and
    /// keeps the daemon running rather than reporting a re-entry it can no
    /// longer distinguish from a first acquisition.
    #[test]
    fn exceeding_the_tracked_depth_is_reported_and_survivable() {
        let mutexes: Vec<Mutex<usize>> = (0..MAX_TRACKED_DEPTH + 4).map(Mutex::new).collect();
        let guards: Vec<_> = mutexes.iter().map(|m| m.lock_recover()).collect();
        assert_eq!(guards.len(), MAX_TRACKED_DEPTH + 4);
        assert!(
            HELD.with(|h| h.overflowed.get()),
            "exceeding the depth must be recorded, not absorbed"
        );
        drop(guards);
        // The untracked tail left nothing behind: the set is empty again.
        assert_eq!(HELD.with(|h| h.depth.get()), 0);
        HELD.with(|h| h.overflowed.set(false));
    }
}
