//! Host-side localisation of the #65 management-announce broadcast path.
//!
//! An on-device backtrace pinned two growing ~256 B allocation sites that drive
//! the T114 heap toward exhaustion under load, both on the management-announce
//! broadcast path:
//!   LEAK 1: `build_announce_payload` (the announce payload Vec), reached via
//!           `Destination::announce` -> `NodeCore::check_mgmt_announces` ->
//!           `handle_timeout`.
//!   LEAK 2: `EmbeddedInterface::try_send`'s `data.to_vec()` (the serial TX
//!           channel), reached via `Transport::dispatch`/`send_on_all_interfaces`.
//!
//! The existing `heap_leak.rs` harness never fired a management announce
//! dispatched through an interface, so this path was untested. This file drives
//! exactly that path on `EmbeddedStorage` (the storage the firmware runs) and
//! measures net live bytes with the same counting allocator the sibling files
//! use, so a per-announce leak is visible without a rig.
//!
//! Findings (this pass, host-side, EmbeddedStorage):
//! - LEAK 1 build_announce_payload is BOUNDED. The payload Vec is moved into the
//!   announce Packet, copied into a stack buffer by `Packet::pack`, then the
//!   packet (and its Vec) is dropped. The only retained copy is the announce
//!   cache entry, which `set_announce_cache` keys by destination hash and
//!   overwrites in place (remove old Vec, insert new). With a single management
//!   destination that is one slot, refreshed each announce: net live bytes are
//!   FLAT. `mgmt_announce_path_live_bytes_flat` is the #65 regression guard.
//! - LEAK 2 the serial `to_vec` is BOUNDED by construction. The firmware's
//!   `EmbeddedInterface` pushes into an Embassy channel of capacity 8 and drops
//!   on overflow (`try_send` returns `BufferFull`). The accumulation is capped
//!   at 8 queued frames whether or not the TX task drains it, so it is a bounded
//!   send channel, not an unbounded core leak. `serial_tx_queue_is_bounded`
//!   pins that: undrained net growth stays a few KB total regardless of the
//!   announce count.
//!
//! Run: `cargo test -p leviculum-core --test heap_leak_mgmt -- --nocapture`

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rand_core::OsRng;

use leviculum_core::constants::MGMT_ANNOUNCE_INTERVAL_MS;
use leviculum_core::traits::{Clock, Storage};
use leviculum_core::{Action, EmbeddedStorage, NodeCore, NodeCoreBuilder, TickOutput};

// ----------------------------------------------------------------------------
// Counting global allocator: net live bytes, mirrors firmware [HEAP] telemetry.
// (Identical to the sibling heap_leak.rs; this is its own test binary, so the
// allocator does not affect the rest of the suite.)
// ----------------------------------------------------------------------------

struct CountingAlloc;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every branch forwards to the System allocator with the same layout it
// was handed; the atomic counters never touch the returned memory.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        FREED.fetch_add(layout.size(), Ordering::Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            FREED.fetch_add(layout.size(), Ordering::Relaxed);
            ALLOCATED.fetch_add(new_size, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

/// Net live heap bytes (allocated minus freed).
fn live_bytes() -> i64 {
    ALLOCATED.load(Ordering::Relaxed) as i64 - FREED.load(Ordering::Relaxed) as i64
}

/// The allocator counters are process-global, so two measurement tests running
/// concurrently would corrupt each other's deltas. Every test that reads the
/// counter holds this lock for the whole measure-and-assert. Poison is ignored:
/// a real assertion failure in one test must not cascade-fail the others.
static MEASURE_LOCK: Mutex<()> = Mutex::new(());

fn measure_guard() -> MutexGuard<'static, ()> {
    MEASURE_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Per-iteration net-bytes ceiling below which an activity counts as flat. A
/// retained announce payload would add hundreds of B/iter; this separates flat
/// from a real regression without tripping on allocator jitter.
const FLAT_CEILING_PER_ITER: f64 = 128.0;

// ----------------------------------------------------------------------------
// Host doubles.
// ----------------------------------------------------------------------------

const START_MS: u64 = 1_000_000;

/// Deterministic steppable clock, shareable so the harness keeps a handle after
/// the clock is moved into the node.
#[derive(Clone)]
struct StepClock(Rc<Cell<u64>>);

impl StepClock {
    fn new(ms: u64) -> Self {
        Self(Rc::new(Cell::new(ms)))
    }
    fn advance(&self, ms: u64) {
        self.0.set(self.0.get() + ms);
    }
}

impl Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// Mirrors the firmware `EmbeddedInterface`: `try_send` pushes `data.to_vec()`
/// into a bounded channel (capacity 8) and drops the frame on overflow
/// (`BufferFull`). `drain` models the `retic_serial_task` consuming frames.
const SERIAL_CAP: usize = 8;

struct SerialTxQueue {
    queue: VecDeque<Vec<u8>>,
    accepted: u64,
    dropped: u64,
}

impl SerialTxQueue {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            accepted: 0,
            dropped: 0,
        }
    }

    /// Push one frame, dropping it if the bounded channel is full.
    fn try_send(&mut self, data: &[u8]) {
        if self.queue.len() < SERIAL_CAP {
            self.queue.push_back(data.to_vec());
            self.accepted += 1;
        } else {
            self.dropped += 1;
        }
    }

    /// Consume every queued frame (what a serviced TX task does each tick).
    fn drain(&mut self) {
        self.queue.clear();
    }
}

type MgmtNode = NodeCore<OsRng, StepClock, EmbeddedStorage>;

/// Build a transport node with the probe responder enabled. That registers the
/// `rnstransport.probe` destination into `mgmt_destinations` and arms
/// `next_mgmt_announce_ms`, so `handle_timeout` fires a management announce once
/// the clock passes the deadline.
fn build_mgmt_node() -> (MgmtNode, StepClock, [u8; 16]) {
    let clock = StepClock::new(START_MS);
    let node = NodeCoreBuilder::new()
        .enable_transport(true)
        .respond_to_probes(true)
        .build(OsRng, clock.clone(), EmbeddedStorage::new());
    let probe = node
        .probe_dest_hash()
        .expect("respond_to_probes must register a probe destination")
        .into_bytes();
    (node, clock, probe)
}

/// Count the broadcast frames in a tick and feed them to the serial mock. The
/// management announce is the only broadcast this node emits (no peers, no
/// paths), so the broadcast count is the announce count.
fn dispatch(out: &TickOutput, serial: &mut SerialTxQueue) -> u64 {
    let mut broadcasts = 0;
    for action in &out.actions {
        match action {
            Action::Broadcast { data, .. } => {
                broadcasts += 1;
                serial.try_send(data);
            }
            Action::SendPacket { data, .. } => {
                serial.try_send(data);
            }
        }
    }
    broadcasts
}

struct MgmtOutcome {
    net_bytes: i64,
    announces: u64,
    serial_accepted: u64,
    serial_dropped: u64,
    cache_populated: bool,
}

/// Drive `warmup + iters` management-announce cycles. Each cycle advances the
/// clock past the next management-announce deadline, runs `handle_timeout`, and
/// dispatches the resulting actions through the serial mock. `drain_serial`
/// selects whether the TX task services the queue each tick. Net live bytes are
/// sampled around the measured `iters` only.
fn run_mgmt(iters: u64, warmup: u64, drain_serial: bool) -> MgmtOutcome {
    let (mut node, clock, probe) = build_mgmt_node();
    let mut serial = SerialTxQueue::new();

    let tick = |node: &mut MgmtNode, serial: &mut SerialTxQueue| -> u64 {
        // Step past the next deadline; the interval resets +2h after each fire,
        // so advancing one full interval fires exactly one announce per tick.
        clock.advance(MGMT_ANNOUNCE_INTERVAL_MS + 1);
        let out = node.handle_timeout();
        let broadcasts = dispatch(&out, serial);
        if drain_serial {
            serial.drain();
        }
        broadcasts
    };

    for _ in 0..warmup {
        tick(&mut node, &mut serial);
    }

    let before = live_bytes();
    let mut announces = 0;
    for _ in 0..iters {
        announces += tick(&mut node, &mut serial);
    }
    let net_bytes = live_bytes() - before;

    let cache_populated = node.storage().get_announce_cache(&probe).is_some();
    MgmtOutcome {
        net_bytes,
        announces,
        serial_accepted: serial.accepted,
        serial_dropped: serial.dropped,
        cache_populated,
    }
}

// ----------------------------------------------------------------------------
// LEAK 1 (and the drained LEAK 2): the management-announce path is flat.
// ----------------------------------------------------------------------------

/// #65 regression guard. The management-announce broadcast path, dispatched
/// through a serviced serial interface on `EmbeddedStorage`, must net ~0 B per
/// announce. A retained announce payload (LEAK 1 as a true leak) would show up
/// as hundreds of B/iter here.
#[test]
fn mgmt_announce_path_live_bytes_flat() {
    let _guard = measure_guard();
    let iters = 5_000;
    let warmup = 200;
    let outcome = run_mgmt(iters, warmup, true);

    let per_iter = outcome.net_bytes as f64 / iters as f64;
    println!(
        "[heap_leak_mgmt] mgmt_announce(drained)      warmup={warmup:<5} iters={iters:<5} \
         net={:>10} B  per_iter={per_iter:>8.2} B  announces={}  serial_acc={}",
        outcome.net_bytes, outcome.announces, outcome.serial_accepted,
    );

    // Non-vacuous: an announce must fire every measured tick, reach the serial
    // interface, and populate the announce cache, or a flat result proves
    // nothing.
    //
    // TWO broadcasts per tick, not one: a management announce is emitted once
    // directly and once more by the announce-retry scheduler
    // (`Transport::schedule_own_announce_retry`, the reference's local-client
    // shape). Each tick steps a full `MGMT_ANNOUNCE_INTERVAL_MS`, so the
    // pending retry and the fresh announce both fall inside it. The leak
    // question is unchanged and sharper: the retry parks a copy of the
    // announce in the announce table, so a per-iteration cost here would now
    // catch a retry entry that fails to retire as well.
    assert_eq!(
        outcome.announces,
        2 * iters,
        "every measured tick must broadcast the two-step management announce ladder"
    );
    assert!(
        outcome.serial_accepted >= iters,
        "management announces must reach the serial interface ({} of {})",
        outcome.serial_accepted,
        iters,
    );
    assert!(
        outcome.cache_populated,
        "management announce must populate the announce cache for the probe dest"
    );

    assert!(
        per_iter.abs() < FLAT_CEILING_PER_ITER,
        "mgmt announce path grew {per_iter:.1} B/iter (ceiling {FLAT_CEILING_PER_ITER:.0}): \
         possible heap leak"
    );
}

// ----------------------------------------------------------------------------
// LEAK 2: the serial TX `to_vec` is a bounded channel, not a core leak.
// ----------------------------------------------------------------------------

/// The firmware serial interface caps its queue at 8 frames and drops on
/// overflow. Even with the TX task NEVER draining, accumulation is bounded by
/// that cap: a few KB total regardless of how many announces are emitted, so
/// the `to_vec` site is a bounded send channel and not an unbounded core leak.
#[test]
fn serial_tx_queue_is_bounded() {
    let _guard = measure_guard();
    let iters = 5_000;
    let warmup = 16; // enough to fill the cap-8 channel before measuring
    let outcome = run_mgmt(iters, warmup, false);

    let per_iter = outcome.net_bytes as f64 / iters as f64;
    println!(
        "[heap_leak_mgmt] mgmt_announce(undrained)    warmup={warmup:<5} iters={iters:<5} \
         net={:>10} B  per_iter={per_iter:>8.2} B  serial_acc={}  serial_dropped={}",
        outcome.net_bytes, outcome.serial_accepted, outcome.serial_dropped,
    );

    // The channel fills to its cap during warmup, then every further announce is
    // dropped: the queue never holds more than SERIAL_CAP frames.
    assert!(
        outcome.serial_accepted <= SERIAL_CAP as u64,
        "an undrained cap-{SERIAL_CAP} channel cannot accept more than {SERIAL_CAP} frames, \
         accepted {}",
        outcome.serial_accepted,
    );
    assert!(
        outcome.serial_dropped >= iters,
        "an undrained channel must drop the overflow ({} dropped over {} measured ticks)",
        outcome.serial_dropped,
        iters,
    );

    // Bounded: the whole measured window adds at most a handful of KB no matter
    // how many announces ran, i.e. ~0 B/iter as the announce count grows.
    assert!(
        per_iter.abs() < FLAT_CEILING_PER_ITER,
        "undrained serial queue grew {per_iter:.1} B/iter: not bounded by its cap"
    );
}
