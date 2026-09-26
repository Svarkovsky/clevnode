//! Host-side heap leak localisation harness for the LNode self-reset bug
//! (Codeberg #65).
//!
//! Field symptom: LNodes self-reset under sustained load from heap exhaustion.
//! The lab has only a few peers, so this cannot be unbounded growth of distinct
//! state. It must be a true leak (allocate-without-free) inside an operation the
//! node repeats forever: announce RX, link churn, or data packet RX/TX. The
//! LNode runs leviculum-core's Transport/Node synchronously in its embedded
//! loop, so the same leak reproduces host-side against leviculum-core directly.
//! The host has spare RAM, so it does not crash, but live allocation still grows
//! monotonically and is measurable here without any radio or rig.
//!
//! This file is its own test binary, so its `#[global_allocator]` does not
//! affect the rest of the suite. The counting allocator mirrors the firmware
//! `[HEAP]` telemetry: it tracks net LIVE bytes (allocated minus freed). Each
//! test warms up to let bounded caches reach steady state, samples live bytes,
//! runs N more iterations of one repeated activity, samples again, and reports
//! net bytes plus bytes-per-iteration. A monotonic per-iteration figure is the
//! leak; near-zero is a clean activity.
//!
//! Findings (this pass, host-side):
//! - announce RX, link establish+teardown, and data packet RX/TX over a link
//!   are ALL flat (~0 B/iter) in shared core, provided the driver drains both
//!   actions AND events each tick. No allocate-without-free in the shared core
//!   paths exercised here.
//! - The ONE way to make announce RX grow monotonically is to skip
//!   `Transport::drain_events()`: each accepted announce then retains a
//!   `TransportEvent::AnnounceReceived` (~470 B) forever. That is the concrete
//!   lead for #65 (`undrained_events_grow_under_announce_load`): the on-device
//!   reviewer should verify the firmware loop drains events unconditionally
//!   every tick, including offline/early-continue branches.
//!
//! Run: `cargo test -p leviculum-core --test heap_leak -- --nocapture`

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rand_core::OsRng;

use leviculum_core::constants::MTU;
use leviculum_core::traits::Clock;
use leviculum_core::transport::{Transport, TransportConfig};
use leviculum_core::{
    Action, Destination, DestinationHash, DestinationType, Direction, Identity, InterfaceId,
    LinkId, MemoryStorage, NoStorage, NodeCore, NodeCoreBuilder, ProofStrategy, SendError,
    TickOutput,
};

// ----------------------------------------------------------------------------
// Counting global allocator: net live bytes, mirrors firmware [HEAP] telemetry.
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
            // Old block of `layout.size()` is gone, new block of `new_size` exists.
            FREED.fetch_add(layout.size(), Ordering::Relaxed);
            ALLOCATED.fetch_add(new_size, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

/// Net live heap bytes (allocated minus freed). Signed: transient orderings can
/// dip below zero, but the steady-state delta we measure is what matters.
fn live_bytes() -> i64 {
    ALLOCATED.load(Ordering::Relaxed) as i64 - FREED.load(Ordering::Relaxed) as i64
}

/// The allocator counters are process-global, so two measurement tests running
/// concurrently (default `cargo test` is parallel) would count each other's
/// allocations and corrupt each other's live_bytes() deltas. Every test that
/// reads the counter holds this lock for the whole measure-and-assert, so only
/// one mutates/reads the global counter at a time. Poison is ignored: a panic in
/// one test (a real assertion failure) must not cascade-fail the others.
static MEASURE_LOCK: Mutex<()> = Mutex::new(());

fn measure_guard() -> MutexGuard<'static, ()> {
    MEASURE_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

// ----------------------------------------------------------------------------
// Minimal host doubles (test_utils is pub(crate), so reproduce what we need).
// ----------------------------------------------------------------------------

const START_MS: u64 = 1_000_000;

/// Deterministic steppable clock. Shareable so the harness keeps a handle to
/// advance the same clock after it has been moved into a node.
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

// leviculum-core is sans-I/O in production: the driver owns interfaces and
// dispatches `TickOutput.actions`, so core only ever sees an interface INDEX.
// These tests pass index 0 as a routing tag and read outbound bytes from the
// returned actions; no interface object is registered (`register_interface` is
// test-only inside the crate and unavailable here).
const IFACE_IDX: usize = 0;

// ----------------------------------------------------------------------------
// Measurement reporting.
// ----------------------------------------------------------------------------

struct Sample {
    label: &'static str,
    warmup: u64,
    iters: u64,
    net_bytes: i64,
}

impl Sample {
    fn per_iter(&self) -> f64 {
        self.net_bytes as f64 / self.iters as f64
    }
    fn report(&self) {
        println!(
            "[heap_leak] {:<28} warmup={:<5} iters={:<5} net={:>12} B  per_iter={:>10.2} B",
            self.label,
            self.warmup,
            self.iters,
            self.net_bytes,
            self.per_iter(),
        );
    }
    /// Report, then guard: a clean activity nets ~0 B/iter. The leak signal we
    /// chase is hundreds of B/iter (an un-drained announce event is ~475 B), so
    /// a generous per-iteration ceiling separates "flat" from a real regression
    /// without tripping on allocator jitter.
    fn assert_flat(&self) {
        self.report();
        assert!(
            self.per_iter().abs() < FLAT_CEILING_PER_ITER,
            "{} grew {:.1} B/iter (ceiling {:.0}): possible heap leak",
            self.label,
            self.per_iter(),
            FLAT_CEILING_PER_ITER,
        );
    }
}

/// Per-iteration net-bytes ceiling below which an activity counts as flat.
const FLAT_CEILING_PER_ITER: f64 = 128.0;

// ----------------------------------------------------------------------------
// Helpers shared by the node-level (link/data) tests.
// ----------------------------------------------------------------------------

type EndpointNode = NodeCore<OsRng, StepClock, NoStorage>;

const IFACE: InterfaceId = InterfaceId(0);

/// All bytes a node wants on the wire this step (SendPacket + Broadcast).
fn outbound(out: &TickOutput) -> Vec<Vec<u8>> {
    out.actions
        .iter()
        .map(|a| match a {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data.clone(),
        })
        .collect()
}

/// Drive two endpoints to quiescence over a lossless point-to-point "wire":
/// everything `a` emits is delivered to `b` and vice versa, seeded by `a`'s
/// initial output, until neither side has anything left to say (bounded rounds).
fn settle(a: &mut EndpointNode, b: &mut EndpointNode, seed_from_a: TickOutput) {
    let mut to_b = outbound(&seed_from_a);
    let mut to_a: Vec<Vec<u8>> = Vec::new();
    for _ in 0..8 {
        if to_a.is_empty() && to_b.is_empty() {
            break;
        }
        for d in std::mem::take(&mut to_b) {
            let out = b.handle_packet(IFACE, &d);
            to_a.extend(outbound(&out));
        }
        for d in std::mem::take(&mut to_a) {
            let out = a.handle_packet(IFACE, &d);
            to_b.extend(outbound(&out));
        }
    }
}

/// Build a link-accepting responder destination plus the bytes A needs to learn
/// it. Returns (responder node, clock handle, dest_hash, signing_key, announce).
fn make_responder(clock_ms: u64) -> (EndpointNode, StepClock, DestinationHash, [u8; 32], Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "heapleak",
        &["responder"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();

    // Pack a direct announce before moving the destination into the node, so the
    // initiator installs a 1-hop path and remembers the signing identity.
    let announce = dest
        .announce(None, &mut OsRng, clock_ms, clock_ms / 1000)
        .unwrap();
    let mut buf = [0u8; MTU];
    let len = announce.pack(&mut buf).unwrap();
    let announce_bytes = buf[..len].to_vec();

    let clock = StepClock::new(clock_ms);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock.clone(), NoStorage);
    node.register_destination(dest);
    (node, clock, dest_hash, signing_key, announce_bytes)
}

fn make_initiator(clock_ms: u64) -> (EndpointNode, StepClock) {
    let clock = StepClock::new(clock_ms);
    let node = NodeCoreBuilder::new().build(OsRng, clock.clone(), NoStorage);
    (node, clock)
}

/// One full connect -> proof -> established round trip. Returns the established
/// link id, or None if the handshake did not complete.
fn establish(
    a: &mut EndpointNode,
    b: &mut EndpointNode,
    dest_hash: DestinationHash,
    key: &[u8; 32],
) -> Option<LinkId> {
    let (link_id, _routed, out) = a.connect(dest_hash, key);
    settle(a, b, out);
    if a.link(&link_id).map(|l| l.is_active()).unwrap_or(false) {
        Some(link_id)
    } else {
        None
    }
}

// ----------------------------------------------------------------------------
// Activity 1: announce RX (the most frequent LoRa activity).
// ----------------------------------------------------------------------------

const ANNOUNCE_PEERS: usize = 3;
const ANNOUNCE_STEP_MS: u64 = 3_000; // > ANNOUNCE_RATE_LIMIT_MS (2000)
const ANNOUNCE_WARMUP: u64 = 200;

/// Outcome of one announce-RX run: net live-byte delta plus the number of peer
/// paths present at the end (proof the announces were actually accepted, so a
/// flat result cannot be a vacuous no-op).
struct AnnounceOutcome {
    net_bytes: i64,
    paths_installed: usize,
}

/// A small fixed peer set (mirrors the lab), each re-announcing with a fresh
/// random hash every iteration, exactly like a real periodic re-announce. The
/// packet-hash dedup store is cleared each iteration so the bounded dedup cache
/// does not fill toward its cap and masquerade as growth; what remains is the
/// announce ACCEPTANCE path (path table, announce table, random-blob list,
/// known identity cache, plus the rebroadcast queue when transport is on).
///
/// `feed` selects what is measured. With `feed = false` only the announces are
/// generated and dropped (the peer-side TX cost); with `feed = true` they are
/// also fed into the transport. The RX cost is the difference, which is what an
/// LNode actually pays: on the air it only receives announces, never generates
/// the ones it must store.
///
/// `drain_events` models the driver contract: a real embedded loop drains both
/// actions AND events every tick. Setting it false reproduces the un-drained
/// case on purpose (see `undrained_events_grow_under_announce_load`).
fn run_announce(iters: u64, feed: bool, drain_events: bool) -> AnnounceOutcome {
    let clock = StepClock::new(START_MS);
    let identity = Identity::generate(&mut OsRng);
    let config = TransportConfig {
        // LNodes are transport nodes; enabling rebroadcast also exercises the
        // announce queue. (Measured identical with it off: the leak path here is
        // transport-mode independent.)
        enable_transport: true,
        // Long expiry: keep paths stable so we isolate acceptance churn rather
        // than path expire/recreate cycles.
        path_expiry_secs: 365 * 24 * 3600,
        ..TransportConfig::default()
    };
    let mut transport = Transport::new(config, clock, MemoryStorage::with_defaults(), identity);

    // Fixed set of peer destinations; we re-announce each one every iteration.
    let mut peers: Vec<Destination> = (0..ANNOUNCE_PEERS)
        .map(|i| {
            let id = Identity::generate(&mut OsRng);
            let aspect = ["peer0", "peer1", "peer2"][i % 3];
            Destination::new(
                Some(id),
                Direction::In,
                DestinationType::Single,
                "heapleak",
                &[aspect],
            )
            .unwrap()
        })
        .collect();
    let peer_hashes: Vec<[u8; 16]> = peers.iter().map(|d| d.hash().into_bytes()).collect();

    let step = |transport: &mut Transport<StepClock, MemoryStorage>, peers: &mut [Destination]| {
        let now = transport.clock().now_ms();
        // Clear dedup so fresh-hash announces are processed, and the store stays
        // tiny instead of growing toward its eviction cap.
        transport.storage_mut().clear_packet_hashes();
        for dest in peers.iter_mut() {
            let announce = dest.announce(None, &mut OsRng, now, now / 1000).unwrap();
            let mut buf = [0u8; MTU];
            let len = announce.pack(&mut buf).unwrap();
            if feed {
                let _ = transport.process_incoming(IFACE_IDX, &buf[..len]);
            }
        }
        if feed {
            transport.poll();
            let _ = transport.drain_actions();
            if drain_events {
                let _ = transport.drain_events().count();
            }
        }
        transport.clock().advance(ANNOUNCE_STEP_MS);
    };

    for _ in 0..ANNOUNCE_WARMUP {
        step(&mut transport, &mut peers);
    }
    let before = live_bytes();
    for _ in 0..iters {
        step(&mut transport, &mut peers);
    }
    let net_bytes = live_bytes() - before;

    let paths_installed = peer_hashes.iter().filter(|h| transport.has_path(h)).count();
    AnnounceOutcome {
        net_bytes,
        paths_installed,
    }
}

#[test]
fn announce_rx_live_bytes() {
    let _guard = measure_guard();
    let iters = 5_000;
    // Generation only (peer TX side) vs generation + RX. The transport never
    // generates the announces it stores, so the cost attributable to RX is the
    // difference between the two.
    let gen = run_announce(iters, false, true);
    let rx = run_announce(iters, true, true);
    let delta = rx.net_bytes - gen.net_bytes;

    assert_eq!(
        rx.paths_installed, ANNOUNCE_PEERS,
        "announce RX did not actually install peer paths; measurement is vacuous"
    );

    Sample {
        label: "announce_gen_only(tx)",
        warmup: ANNOUNCE_WARMUP,
        iters,
        net_bytes: gen.net_bytes,
    }
    .assert_flat();
    Sample {
        label: "announce_gen+rx",
        warmup: ANNOUNCE_WARMUP,
        iters,
        net_bytes: rx.net_bytes,
    }
    .assert_flat();
    Sample {
        label: "announce_rx_only(delta)",
        warmup: ANNOUNCE_WARMUP,
        iters,
        net_bytes: delta,
    }
    .assert_flat();
}

/// Lead for #65, locked in as a guard: the shared core's announce RX is leak
/// free ONLY while the driver drains events every tick. A driver that skips
/// draining (e.g. an embedded loop that early-continues when an interface is
/// offline) grows `Transport::events` by one `AnnounceReceived` per accepted
/// announce, never reclaimed. With a few peers re-announcing forever that is a
/// monotonic allocate-without-free matching the field symptom. This test pins
/// the contrast so the hazard cannot silently regress into "looks fine".
#[test]
fn undrained_events_grow_under_announce_load() {
    let _guard = measure_guard();
    let iters = 2_000;
    let drained = run_announce(iters, true, true);
    let undrained = run_announce(iters, true, false);

    let drained_per_iter = drained.net_bytes as f64 / iters as f64;
    let undrained_per_iter = undrained.net_bytes as f64 / iters as f64;
    println!(
        "[heap_leak] events drained={drained_per_iter:.2} B/iter  undrained={undrained_per_iter:.2} B/iter"
    );

    assert!(
        drained_per_iter.abs() < FLAT_CEILING_PER_ITER,
        "draining events should be flat, got {drained_per_iter:.1} B/iter"
    );
    // Each iteration accepts ANNOUNCE_PEERS announces; un-drained that is several
    // retained events per iteration, far above the flat ceiling.
    assert!(
        undrained_per_iter > FLAT_CEILING_PER_ITER * 4.0,
        "un-drained events should grow markedly, got {undrained_per_iter:.1} B/iter"
    );
}

// ----------------------------------------------------------------------------
// Activity 2: link establish + teardown cycle, repeated.
// ----------------------------------------------------------------------------

/// Each iteration: A learns the responder, opens a link, the proof returns, the
/// link establishes, then A closes it (close packet delivered to B). A leak here
/// is link/destination/channel state that close() fails to free.
fn run_link_cycle(iters: u64) -> (i64, u64) {
    let (mut b, b_clock, dest_hash, key, announce) = make_responder(START_MS);
    let (mut a, a_clock) = make_initiator(START_MS);

    let established = Cell::new(0u64);
    let cycle = |a: &mut EndpointNode, b: &mut EndpointNode| {
        // Re-teach the path each time (paths/identity may expire or be pruned).
        let out = a.handle_packet(IFACE, &announce);
        settle(a, b, out);
        if let Some(link_id) = establish(a, b, dest_hash, &key) {
            established.set(established.get() + 1);
            let out = a.close_link(&link_id);
            settle(a, b, out);
        }
        // Advance so retry/keepalive timers do not accumulate against one instant.
        a_clock.advance(100);
        b_clock.advance(100);
        let _ = a.handle_timeout();
        let _ = b.handle_timeout();
    };

    for _ in 0..100 {
        cycle(&mut a, &mut b);
    }
    established.set(0);
    let before = live_bytes();
    for _ in 0..iters {
        cycle(&mut a, &mut b);
    }
    // (net bytes, links established during the measured window)
    (live_bytes() - before, established.get())
}

#[test]
fn link_cycle_live_bytes() {
    let _guard = measure_guard();
    let iters = 2_000;
    let (net, established) = run_link_cycle(iters);
    assert_eq!(
        established, iters,
        "every measured cycle must establish a link, else the result is vacuous"
    );
    Sample {
        label: "link_establish_teardown",
        warmup: 100,
        iters,
        net_bytes: net,
    }
    .assert_flat();
}

// ----------------------------------------------------------------------------
// Activity 3: data packet RX/TX over an established link, repeated.
// ----------------------------------------------------------------------------

/// Establish one link, then push a small data packet A -> B every iteration,
/// flushing acks back each time. A leak here is per-packet channel/receipt
/// bookkeeping that is never reclaimed.
fn run_data_packet(iters: u64) -> (i64, u64) {
    let (mut b, b_clock, dest_hash, key, announce) = make_responder(START_MS);
    let (mut a, a_clock) = make_initiator(START_MS);

    let out = a.handle_packet(IFACE, &announce);
    settle(&mut a, &mut b, out);
    let link_id = establish(&mut a, &mut b, dest_hash, &key)
        .expect("link must establish for the data-packet measurement");

    let payload = b"heap-leak-probe-payload-0123456789";

    let sent_ok = Cell::new(0u64);
    let busy = Cell::new(0u64);
    let tick = |a: &mut EndpointNode, b: &mut EndpointNode| {
        match a.send_on_link(&link_id, payload) {
            Ok(out) => {
                sent_ok.set(sent_ok.get() + 1);
                settle(a, b, out)
            }
            Err(SendError::Busy) | Err(SendError::PacingDelay { .. }) => {
                // Window momentarily full: let acks drain, then continue.
                busy.set(busy.get() + 1);
            }
            Err(e) => panic!("unexpected send error: {e}"),
        }
        // Advance and flush both sides so receipts/acks/keepalives settle and do
        // not pile up as apparent growth.
        a_clock.advance(50);
        b_clock.advance(50);
        let out = a.handle_timeout();
        settle(a, b, out);
        let out = b.handle_timeout();
        settle(b, a, out);
    };

    for _ in 0..200 {
        tick(&mut a, &mut b);
    }
    sent_ok.set(0);
    busy.set(0);
    let before = live_bytes();
    for _ in 0..iters {
        tick(&mut a, &mut b);
    }
    // (net bytes, data packets actually sent during the measured window)
    (live_bytes() - before, sent_ok.get())
}

#[test]
fn data_packet_live_bytes() {
    let _guard = measure_guard();
    let iters = 5_000;
    let (net, sent_ok) = run_data_packet(iters);
    // At most one send per iteration; the rare Busy tick is fine. Require the
    // vast majority to land so the measurement is not a vacuous no-op.
    assert!(
        sent_ok >= iters * 9 / 10,
        "data path must keep sending ({sent_ok} of {iters} iters), else result is vacuous"
    );
    Sample {
        label: "data_packet_rx_tx",
        warmup: 200,
        iters,
        net_bytes: net,
    }
    .assert_flat();
}
