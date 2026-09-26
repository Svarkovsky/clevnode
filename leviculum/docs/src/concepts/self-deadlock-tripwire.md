# The Self-Deadlock Tripwire

A deadlock is the worst failure this daemon produces. A panic leaves a stack
and an exit code; a dropped packet leaves a counter; a wrong answer leaves a
log line somebody can grep. A self-deadlock leaves nothing at all. The node
stops, keeps its listening socket open, and reads as healthy to every
supervisor watching it.

This page records what detects one, what it costs, which decisions were made
against project precedent, and what it does not reach.

## The hazard

`93ba351` shipped [`CoreProcessor`], a seam that runs consumer code *inside*
the driver's tick. Both entry points — `run_event_tap` and the timer branch's
`run_tick` — call the consumer's hook with the core `std::sync::Mutex` guard
live, because handing the hook `&mut StdNodeCore` is the entire point of the
seam.

`std::sync::Mutex` is not reentrant. So any handle the consumer smuggled in
that re-locks the core parks the driver's event loop on a lock only that same
loop can release. `ReticulumNode::has_path`
(`leviculum-std/src/driver/mod.rs:2632-2634`) does it in one line, and it is
one of roughly forty synchronous `pub fn`s on `ReticulumNode` shaped exactly
like it. No `.await`, no `unsafe`, no channel — nothing a compiler or a
compile-fail fixture can see.

The seam's own defence is a construction-order barrier, not a guarantee: a
processor is registered on the *builder*, before the node exists, so it cannot
be built holding a handle to the node it will run inside. Getting one takes a
deliberate `OnceLock`/`Weak` cycle. That is why the hazard is theoretical
rather than routine — and it is exactly what a future `set_core_processor` on
a live node would give up.

## Two things that were tried on paper and are not the answer

**`try_lock` that reports instead of blocking is strictly worse.** From the
same thread `try_lock` returns `WouldBlock`, so `has_path` would answer
`false` for a destination that *has* a path. A loud deadlock becomes a silent
wrong answer inside routing, which is the worse failure for Priority 1. It
would also mean turning roughly forty public accessors fallible, a breaking
change for every consumer, to defend against a hazard reachable only from
inside the seam.

**Type-system prevention does not exist.** The processor is
`Box<dyn CoreProcessor>` and its struct is opaque by construction. Every bound
available (`'static`, `Send`) is already applied and none can express "holds
no `Arc<ReticulumNode>`".

Detection is the remaining avenue.

## The mechanism

`MutexRecover::lock_recover` (`leviculum-std/src/sync_ext.rs`) is the single
choke point: every `std::sync::Mutex` acquisition in `leviculum-std` goes
through it, 149 call sites against one implementation.

Before it blocks, it records the mutex's address in a thread-local set. An
address already present means *this thread* is about to wait for a lock only
*this thread* can release, which never wakes. The guard it returns
(`TrackedGuard`) removes the address on `Drop`.

Three properties are worth naming because each is a defect if it is wrong:

* **Removal is by address, not by position.** Guards are values and nothing
  forces them to drop in acquisition order. Popping the top would evict the
  wrong entry and leave a stale address behind — a false positive on the next
  acquisition of a mutex that was released long ago.
* **The set is per thread.** Another thread holding the mutex is an ordinary
  wait, not a self-deadlock. `TrackedGuard` is `!Send` for the same reason a
  `MutexGuard` is, which is also what makes the thread-local sound: a
  registration can never be read from a thread other than the one that made
  it.
* **An unwind must clean up.** A hook that panics for an unrelated reason
  unwinds through the guard, and if the address survived that, the *next*
  acquisition on that thread would report a re-entry that is not one. The
  tripwire's worst failure mode is a false positive in production, so this has
  its own test.

Depth is capped at 32 held mutexes per thread — the crate's deepest measured
nesting is 4 — and exceeding it is reported as `LOCK_DEPTH_OVERFLOW` rather
than absorbed. Past that point a *negative* answer means nothing, and a check
that has quietly stopped checking is the failure mode
[Checks That Are Actually Checks](checks-and-citations.md) exists to remove.

## The measurement that chose the shape

Two shapes were on the table. The narrow one — a `static CORE_LOCK_OWNER:
AtomicU64` written by the tap and the timer branch, read in `lock_recover`
under `#[cfg(debug_assertions)]` — is scoped to the one lock and the two
callers we currently suspect, and does nothing in release, which is where the
deadlock actually bites. The broad one is the thread-local address set above,
which covers every mutex and every caller and needs nobody to have guessed
right.

The question that settles it is what the broad shape costs on a real workload.

**Per acquisition** (release, `opt-level=3`, uncontended mutex, 20 M
iterations, best of 5, four-core host):

| shape | ns/acquisition | delta |
|---|---|---|
| no tripwire (previous behaviour) | 3.024 | — |
| narrow: one relaxed atomic load | 3.021 | +0.00 |
| broad: thread-local set, depth 1 | 3.482 | **+0.458** |
| broad: thread-local set, depth 4 | 3.867 | +0.843 |

In an unoptimised build the same three are 24.6 / 26.5 / 67.1 ns, because
nothing inlines; that is a test-run cost, not a shipped one.

**Acquisition rate on a real workload.** The TCP-hub load test
(`leviculum-std/tests/rnsd_interop/loadtest_tcp_hub_tests.rs`) driving the
real `lnsd` binary at 128 steady connections, one packet per connection every
15 ms for 40 s, 377,572 packets forwarded at 100 % delivery: **2,061,220
acquisitions in 42.56 s = 48,428/s**, or 5.46 acquisitions per forwarded
packet.

**So the prediction is** 2,061,220 × 0.458 ns = **0.94 ms of CPU across the
whole 42.6 s run** — 0.0055 % of the hub's 17.1 s of CPU time, and 2.5 ns
against the 45.4 µs of CPU the hub spends per forwarded packet, about one part
in eighteen thousand.

**And the end-to-end A/B agrees, which is the point of doing both.** Two
release `lnsd` binaries differing only in the tripwire, alternated over the
same load:

| | hub CPU per run | mean |
|---|---|---|
| no tripwire | 18.07, 17.54, 15.83 s | 17.147 s |
| tripwire | 17.33, 16.61, 17.72, 16.90 s | 17.140 s |

Delta −0.04 %, against a run-to-run spread of ±6 % on the same binary.
Delivery was 100.0000 % on every run of both. A null A/B on its own would only
say the effect is below the noise floor; paired with the prediction it says
*why* — the effect is three orders of magnitude below it, and no amount of
extra runs would resolve it.

It is noise. The broad shape wins, on the criterion set before the numbers
were taken.

## Panic, not log-and-hang — and in release too

Both decisions follow from the same observation, and neither inherits from the
poison precedent above it in `sync_ext.rs`.

**Why not log-and-hang.** Project policy prefers a degraded daemon to a
crashing one, and `MutexRecover`'s poison recovery argues exactly that. But
that argument does not transfer, because there the daemon really is degraded
and still serving: the panicking task has unwound, every other task still
runs, and the node keeps forwarding. Here the thread about to block is
normally the driver's event loop, and once it stops the node forwards nothing,
answers no path request and maintains no link — while holding its listening
socket open. That is not degraded operation. It is a stopped node wearing a
healthy face, and Priority 1 is packet delivery, of which this delivers zero.

Panicking is also, for the hazard this exists for, the *cheapest possible*
recovery — because the seam already catches it. `run_event_tap` and `run_tick`
wrap every hook in `catch_unwind`. The unwind releases the outer core guard
(poisoned, which `lock_recover` then recovers), the driver logs
`CORE_PROCESSOR_PANICKED`, detaches the offending processor permanently, emits
`NodeEvent::CoreProcessorPanicked` on the application's control plane, and
carries on serving the mesh without it. That is precisely the graceful
degradation the policy asks for, and log-and-hang forfeits it. Away from the
seam, a report here means an ordinary lock-order bug in our own code on a path
some test exercises, and failing at the defect beats hanging at it.

**Why release carries it.** Debug-only was the cheap answer and it does
nothing where the deadlock actually bites: a `#[cfg(debug_assertions)]` check
is absent from every binary an operator runs. The only argument for compiling
it out is cost, and cost is 0.0055 % of a busy hub's CPU. There is nothing to
trade.

## The standing canaries

Per [Checks That Are Actually Checks](checks-and-citations.md): a tripwire
that has silently stopped tripping satisfies "no deadlock detected" forever,
so the demonstration is permanent rather than one-time. Both halves live in
`leviculum-std/src/sync_ext.rs` and run in `just fast`
(`cargo test --workspace --lib`):

* `canary_re_entering_one_mutex_trips` — a re-entry the detector is meant to
  see, asserting the report names both the failure and the mutex.
* `canary_distinct_mutexes_nest_without_tripping` — legitimately nested
  *different* mutexes, which must not fire. Without it the positive canary is
  satisfied by a tripwire that reports everything, which would take the daemon
  down on its first tick.

The acceptance test is `leviculum-std/tests/mvr/core_lock_reentrancy.rs`: a
registered `CoreProcessor` calling `has_path` on the node it runs inside, plus
a negative control that is the same node with a processor that does not
re-enter. **That test hung forever before this landed**, which is why it
carries two independent bounds — a `tokio::time::timeout` on a runtime the
node does not own, and `Drop for ReticulumNode`, which polls for at most 400 ms
and then calls `Runtime::shutdown_background`. Verified by disabling the
tripwire by hand: the test fails in 15 s with a message naming the deadlock,
and the binary exits normally.

## What this does not reach

* **Deadlocks between two threads and two mutexes.** A holds M1 wanting M2
  while B holds M2 wanting M1 is a lock-*order* inversion, and neither
  thread's own set contains what it is waiting for. Detecting that needs a
  wait-for graph across threads, which is a different mechanism and a
  different cost.
* **Mutexes not taken through `lock_recover`.** `tokio::sync::Mutex`, any
  `RwLock`, any `.lock().unwrap()` that skipped the trait, and every mutex
  outside `leviculum-std`. The choke point is what makes this cheap and it is
  also its boundary.
* **Blocking that is not a mutex.** A hook that blocks on a channel, a socket
  or a `block_on` stalls the loop just as completely and is invisible here.
  [The core lock budget](core-lock-budget.md) is the surface for that, and it
  reports after the fact rather than preventing.
* **Depth beyond 32 on one thread**, which is reported and then untracked.

## See also

- [The core lock budget](core-lock-budget.md) — what a hook may cost while it
  holds the lock, as opposed to whether it may re-take it.
- [Checks That Are Actually Checks](checks-and-citations.md) — the
  standing-canary rule this page applies.
