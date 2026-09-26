//! Codeberg #236: what one telemetry report costs the heap.
//!
//! The T114 port is a board with ~13 KiB of stack margin and the #50 OOM
//! history behind it, and the issue's order asks for this number *before*
//! that port, as its baseline. So it is measured here on the host, where a
//! counting global allocator can see every request, rather than inferred
//! from the firmware afterwards.
//!
//! **Gross, not net.** Live-bytes-at-the-end is the number that misses the
//! peak: a construction that allocates a 1 KiB buffer, copies it and drops
//! the original nets zero and still needs 2 KiB of headroom at its worst
//! moment. Both are recorded — the gross total and the peak live figure —
//! and the budget is asserted against the peak, because that is what an
//! allocator has to have available.
//!
//! **A global allocator is process-wide, and the test harness is not.**
//! Counting everything would make the numbers depend on what the other
//! test in this binary happened to be doing, which is a flaky test with a
//! plausible-looking output. Two things make it deterministic instead: the
//! counters only move on a thread that has armed itself as the recorder,
//! and a mutex admits one recorder at a time. `--test-threads` then
//! changes nothing.
//!
//! Run with `--nocapture` to read the numbers; the assertions are
//! ceilings, not the measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use leviculum_core::Identity;
use leviculum_lxmf::msgpack::Number;
use leviculum_lxmf::telemetry::{build_report, Battery, Location, Telemetry};

static GROSS: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only inside [`Recorder`]. Const-initialised `Cell<bool>`, so
    /// reading it from inside the allocator neither allocates nor
    /// recurses.
    static RECORDING: Cell<bool> = const { Cell::new(false) };
}

fn recording() -> bool {
    RECORDING.try_with(Cell::get).unwrap_or(false)
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if recording() {
            GROSS.fetch_add(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if recording() {
            LIVE.fetch_sub(
                layout.size().min(LIVE.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if recording() {
            // A grow is a fresh allocation as far as the peak is
            // concerned: the allocator may well have to move the block.
            GROSS.fetch_add(new_size, Ordering::Relaxed);
            let live = LIVE.fetch_add(new_size, Ordering::Relaxed) + new_size;
            PEAK.fetch_max(live, Ordering::Relaxed);
            LIVE.fetch_sub(
                layout.size().min(LIVE.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static ONE_RECORDER: Mutex<()> = Mutex::new(());

/// Arms this thread as the only counting thread for its lifetime.
struct Recorder(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Recorder {
    fn start() -> Self {
        let guard = ONE_RECORDER.lock().unwrap_or_else(|e| e.into_inner());
        GROSS.store(0, Ordering::Relaxed);
        LIVE.store(0, Ordering::Relaxed);
        PEAK.store(0, Ordering::Relaxed);
        RECORDING.with(|r| r.set(true));
        Self(guard)
    }

    /// Gross bytes requested, peak live bytes, live bytes still held.
    fn read(&self) -> (usize, usize, usize) {
        (
            GROSS.load(Ordering::Relaxed),
            PEAK.load(Ordering::Relaxed),
            LIVE.load(Ordering::Relaxed),
        )
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        RECORDING.with(|r| r.set(false));
    }
}

fn source_identity() -> Identity {
    let bytes = hex::decode(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
         202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
    )
    .unwrap();
    Identity::from_private_key_bytes(&bytes).unwrap()
}

/// The heaviest report an LNode builds: position plus battery, which is
/// what a tracker with a fix emits on every cadence tick.
fn full_reading() -> Telemetry {
    Telemetry {
        time: Some(1_790_000_000),
        location: Some(Location {
            latitude_e6: 53_551_086,
            longitude_e6: 9_993_682,
            altitude_e2: 1_200,
            speed_e2: 140,
            bearing_e2: 9_000,
            accuracy_e2: 600,
            last_update: 1_790_000_000,
        }),
        battery: Some(Battery {
            charge_percent: Number::Int(63),
            charging: None,
            temperature: None,
        }),
        ..Telemetry::default()
    }
}

fn heartbeat_reading() -> Telemetry {
    Telemetry {
        time: Some(1_790_000_000),
        battery: Some(Battery {
            charge_percent: Number::Int(63),
            charging: None,
            temperature: None,
        }),
        ..Telemetry::default()
    }
}

/// Measure one report construction, warming up first so whatever
/// ed25519 initialises lazily is not charged to a per-report figure.
fn measure(reading: &Telemetry) -> (usize, usize, usize, usize) {
    let source = source_identity();
    let warm = build_report([0x11; 16], [0x22; 16], &source, 1_790_000_000.0, reading).unwrap();
    drop(warm.on_air().unwrap());
    drop(warm);

    let recorder = Recorder::start();
    let message = build_report([0x11; 16], [0x22; 16], &source, 1_790_000_000.0, reading).unwrap();
    let on_air = message.on_air().unwrap();
    let (gross, peak, live) = recorder.read();
    drop(recorder);
    (gross, peak, live, on_air.len())
}

#[test]
fn one_report_construction_stays_inside_its_heap_budget() {
    let (gross, peak, live, on_air) = measure(&full_reading());
    println!(
        "TELEMETRY_REPORT_ALLOC gross={gross} peak_live={peak} live_after={live} \
         on_air_bytes={on_air}"
    );

    // Ceilings, not measurements. They exist so a refactor that starts
    // copying the payload three more times fails here instead of on a
    // board with 96 KiB of heap.
    assert!(
        peak <= 2_048,
        "peak live heap for one report is {peak} B, budget 2048 B"
    );
    assert!(
        gross <= 4_096,
        "gross allocation for one report is {gross} B, budget 4096 B"
    );
}

#[test]
fn a_heartbeat_without_a_position_costs_less_than_a_full_report() {
    let (gross, peak, _, on_air) = measure(&heartbeat_reading());
    println!("TELEMETRY_HEARTBEAT_ALLOC gross={gross} peak_live={peak} on_air_bytes={on_air}");
    let (full_gross, _, _, full_on_air) = measure(&full_reading());
    assert!(
        gross < full_gross,
        "heartbeat {gross} B is not cheaper than a full report {full_gross} B"
    );
    assert!(on_air < full_on_air);
    assert!(gross <= 4_096, "gross allocation {gross} B, budget 4096 B");
}
