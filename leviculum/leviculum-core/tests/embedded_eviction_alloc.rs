//! Allocation regression guard for EmbeddedStorage insert-with-eviction
//! (Codeberg #65).
//!
//! Root cause of the LNode self-reset under load: the previous `map_set`
//! evicted the oldest entry on every insert into a FULL capacity-bounded map
//! by COPYING THE WHOLE MAP through two transient heap `Vec`s
//! (`Vec<K>` + `Vec<(K, V)>`). Under churn the maps sit at capacity, so this
//! fired per insert; on a near-full 64 KiB heap the multi-KB transient OOMs
//! the `embedded_alloc` allocator and resets the node.
//!
//! The fix makes eviction allocation-free (a companion inline insertion-order
//! index). This test drives a bounded map PAST capacity in steady state and
//! asserts the GROSS heap bytes allocated DURING the eviction loop is ZERO.
//!
//! Note on the metric: the #65 failure is a TRANSIENT allocation spike, not a
//! leak. The pre-fix `map_set` allocated its scratch `Vec`s and freed them
//! within the same insert, so a NET live-bytes metric (allocated minus freed)
//! reads zero for both versions and would NOT catch the regression. The OOM
//! happens at the moment the multi-KB request hits a near-full heap with no
//! large-enough free block, regardless of the later free. So we count GROSS
//! bytes allocated and the number of allocation CALLS in the eviction window.
//! Maps with `Copy` values (path_states) and the dedup tag set allocate
//! nothing at all on the value side, so a clean eviction path makes ZERO
//! allocation calls; the pre-fix code made ~2 calls (~1 KB) per eviction.
//!
//! This file is its own test binary, so its `#[global_allocator]` does not
//! affect the rest of the suite (same pattern as `tests/heap_leak.rs`).
//!
//! Run: `cargo test -p leviculum-core --test embedded_eviction_alloc -- --nocapture`

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::storage_types::{PathEntry, PathState};
use leviculum_core::traits::Storage;
use leviculum_core::EmbeddedStorage;

// ----------------------------------------------------------------------------
// Counting global allocator: net live bytes (allocated minus freed).
// ----------------------------------------------------------------------------

struct CountingAlloc;

// The counters are PER THREAD, and that is load-bearing rather than tidy.
//
// They used to be process-global `AtomicUsize`es, on the reasoning that a
// binary exposing a single `#[test]` has no intra-binary parallelism and so no
// other thread to pollute a measured window. That reasoning is wrong by one
// thread: libtest runs the test on a spawned thread while its MAIN thread
// stays alive waiting on the result channel, and that main thread allocates
// when it wakes. Under CPU load it wakes inside the window — reproduced at
// 3 failures in 25 runs against six busy cores, every one of them the same
// four calls and 900 gross bytes, in the first scenario, with an allocation
// rate (0.0001 calls/iter over 50 000 iterations) that no per-insert
// regression could produce. A guard that fails 12 % of the time under load
// while the code it guards is correct teaches the opposite of what it exists
// to teach.
//
// Counting on the allocating thread makes the window measure exactly what the
// assertion claims: allocations made BY THE CODE UNDER TEST, which runs on one
// thread. Nothing is lost — `set_path_state` and friends are called only from
// the measuring thread, so a real regression is still counted in full.
//
// `const`-initialised `Cell<usize>` on purpose: it has no destructor, so TLS
// needs no lazy init and no destructor registration, and therefore never
// allocates. A `thread_local!` that allocated on first touch inside a global
// allocator would recurse.
thread_local! {
    /// Gross bytes ever requested on this thread (never decremented). The
    /// transient-spike metric: the #65 failure is a spike, not a leak.
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
    /// Number of allocation calls on this thread (alloc + alloc_zeroed +
    /// realloc).
    static ALLOC_CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Record one allocation of `size` bytes against the calling thread.
///
/// `try_with` rather than `with`: after TLS destruction has begun on a thread,
/// `with` panics, and a panic inside the global allocator is an abort. There
/// is nothing to count that late anyway.
fn record(size: usize) {
    let _ = ALLOCATED.try_with(|c| c.set(c.get().wrapping_add(size)));
    let _ = ALLOC_CALLS.try_with(|c| c.set(c.get().wrapping_add(1)));
}

// SAFETY: every branch forwards to the System allocator with the same layout
// it was handed; the thread-local counters never touch the returned memory.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            // A realloc that grows is a fresh large request against the heap.
            record(new_size);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn allocated_bytes() -> usize {
    ALLOCATED.with(Cell::get)
}

fn alloc_calls() -> usize {
    ALLOC_CALLS.with(Cell::get)
}

// This binary still exposes a SINGLE `#[test]` that runs every scenario
// sequentially. With per-thread counters that is no longer load-bearing for
// correctness, but it keeps the scenarios' windows from interleaving and keeps
// the output readable, so it stays.

fn key_th(i: usize) -> [u8; TRUNCATED_HASHBYTES] {
    let mut h = [0u8; TRUNCATED_HASHBYTES];
    h[..8].copy_from_slice(&(i as u64).to_le_bytes());
    h
}

fn key32(i: usize) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&(i as u64).to_le_bytes());
    h
}

/// Warm `body` over `[0, WARMUP)` to settle any one-time cold-path init, then
/// measure `iters` more calls (indices continue past warmup so they never
/// collide) and assert ZERO heap allocations. The value side of the maps under
/// test owns no heap, so a clean eviction path allocates nothing at all; the
/// pre-fix rebuild made ~2 calls / ~1 KB per eviction. A strict zero is the
/// right bar: nothing else runs in the measured window.
const WARMUP: usize = 1_000;

fn assert_alloc_free(label: &str, iters: u64, mut body: impl FnMut(usize)) {
    for i in 0..WARMUP {
        body(i);
    }
    let bytes0 = allocated_bytes();
    let calls0 = alloc_calls();
    for i in 0..iters as usize {
        body(WARMUP + i);
    }
    let bytes = allocated_bytes() - bytes0;
    let calls = alloc_calls() - calls0;
    println!(
        "[evict_alloc] {label:<24} iters={iters:<6} gross={bytes:>12} B  calls={calls:>10}  ({:.4} B/iter, {:.4} calls/iter)",
        bytes as f64 / iters as f64,
        calls as f64 / iters as f64,
    );
    assert_eq!(
        calls, 0,
        "{label}: {calls} heap allocations ({bytes} B gross) during steady-state eviction; the eviction path is not allocation-free"
    );
}

/// Run every allocation-free scenario sequentially in one test (see the note
/// above on why this is a single test rather than four).
#[test]
fn eviction_paths_are_allocation_free() {
    check_path_states_eviction();
    check_path_table_eviction();
    check_path_table_refresh();
    check_path_request_tag_set_eviction();
}

/// path_states: `PathState` is a `Copy` enum, so the value side allocates
/// nothing. Once the table is full (cap 32), every distinct-key insert evicts.
/// Gross heap growth across the measured window must be zero.
fn check_path_states_eviction() {
    let cap = 32usize;
    let mut s = EmbeddedStorage::new();

    // Fill to capacity, then warm up well past it so the table is in steady
    // eviction and any one-time setup has settled.
    for i in 0..(cap * 4) {
        s.set_path_state(key_th(i), PathState::Unresponsive);
    }

    let iters = 50_000u64;
    let base = cap * 4;
    // Distinct fresh key every time -> guaranteed eviction every insert.
    assert_alloc_free("path_states_evict", iters, |i| {
        s.set_path_state(key_th(base + i), PathState::Responsive);
    });

    // Sanity: the table is still exactly at capacity (not vacuous). The last
    // measured key index is base + WARMUP + iters - 1.
    let last = base + WARMUP + iters as usize;
    let live = ((last - cap)..last)
        .filter(|i| s.get_path_state(&key_th(*i)).is_some())
        .count();
    assert_eq!(
        live, cap,
        "table must remain full; measurement otherwise vacuous"
    );
}

/// path_table with empty `random_blobs` and `next_hop: None`: the value owns
/// no heap, so this isolates the eviction path the same way path_states does,
/// while also exercising the refresh-on-re-insert branch.
fn check_path_table_eviction() {
    let cap = 32usize;
    let entry = PathEntry {
        hops: 1,
        expires_ms: 10_000,
        interface_index: 0,
        random_blobs: Vec::new(),
        next_hop: None,
        via_peer: None,
    };

    let mut s = EmbeddedStorage::new();
    for i in 0..(cap * 4) {
        s.set_path(key_th(i), entry.clone());
    }

    let iters = 50_000u64;
    let base = cap * 4;
    // Empty `random_blobs` clones without allocating, so any heap call here is
    // the eviction path itself.
    assert_alloc_free("path_table_evict", iters, |i| {
        s.set_path(key_th(base + i), entry.clone());
    });

    assert_eq!(
        s.path_count(),
        cap,
        "table must remain full; otherwise vacuous"
    );
}

/// Refresh-on-re-insert must also be allocation-free: repeatedly re-insert
/// keys already present (the `set_path` re-announce path) while the table is
/// full. No eviction occurs here, but the order-index move-to-back must not
/// allocate.
fn check_path_table_refresh() {
    let cap = 32usize;
    let entry = PathEntry {
        hops: 1,
        expires_ms: 10_000,
        interface_index: 0,
        random_blobs: Vec::new(),
        next_hop: None,
        via_peer: None,
    };

    let mut s = EmbeddedStorage::new();
    for i in 0..cap {
        s.set_path(key_th(i), entry.clone());
    }

    let iters = 50_000u64;
    // Re-insert an existing key (cycles through the full key set).
    assert_alloc_free("path_table_refresh", iters, |i| {
        s.set_path(key_th(i % cap), entry.clone());
    });

    assert_eq!(
        s.path_count(),
        cap,
        "refresh must not change membership count"
    );
}

/// The path-request dedup tag set (OrderedSet) had the same KB-per-eviction
/// rebuild; its eviction must now be allocation-free too.
fn check_path_request_tag_set_eviction() {
    let cap = 32usize;
    let mut s = EmbeddedStorage::new();

    for i in 0..(cap * 4) {
        s.check_path_request_tag(&key32(i));
    }

    let iters = 50_000u64;
    let base = cap * 4;
    assert_alloc_free("tag_set_evict", iters, |i| {
        let seen = s.check_path_request_tag(&key32(base + i));
        assert!(!seen, "fresh tag wrongly reported as seen");
    });
}

/// Canary: the counter still counts.
///
/// Every assertion above is of the form "the count is zero", and a counter
/// that has quietly stopped counting satisfies all of them forever — the same
/// hazard `doc_citations.rs` keeps `citation_guard_canary` for. This drives a
/// body that allocates on purpose through the same accessors and requires the
/// allocations to be seen.
///
/// It is also what makes the move to per-thread counters checkable: had
/// `record` been unable to touch TLS from inside the allocator, this would
/// read zero while the four windows above went on passing.
///
/// A second `#[test]` in this binary is safe now that the counters are
/// per-thread. libtest may run the two concurrently, and each thread counts
/// only its own allocations.
#[test]
fn the_allocation_counter_counts() {
    const BLOCKS: usize = 64;
    const BLOCK: usize = 256;

    let bytes0 = allocated_bytes();
    let calls0 = alloc_calls();
    let mut sink: Vec<Vec<u8>> = Vec::with_capacity(BLOCKS);
    for _ in 0..BLOCKS {
        sink.push(vec![0u8; BLOCK]);
    }
    let calls = alloc_calls() - calls0;
    let bytes = allocated_bytes() - bytes0;

    // `>=`: the outer `Vec` allocates once as well, and `vec![0u8; n]` may go
    // through `alloc_zeroed`. The floor is what the inner blocks must cost.
    assert!(
        calls >= BLOCKS,
        "the allocation counter saw {calls} calls for {BLOCKS} heap blocks — \
         it has stopped counting, and every zero-allocation assertion in this \
         file is now vacuous"
    );
    assert!(
        bytes >= BLOCKS * BLOCK,
        "the allocation counter saw {bytes} gross bytes for {} — it is \
         undercounting",
        BLOCKS * BLOCK
    );
    drop(sink);
}
