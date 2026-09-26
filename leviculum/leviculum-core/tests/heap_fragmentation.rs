//! Host-side reproduction of the LNode heap exhaustion (Codeberg #65) as
//! ALLOCATOR FRAGMENTATION, not as a leak.
//!
//! Batch 3 (`heap_leak.rs`) proved the shared core does not LEAK: net live
//! bytes are flat across announce RX, link churn, and data packet RX/TX. But
//! the firmware does not run on a System allocator. It runs on
//! `embedded_alloc::LlffHeap` (linked-list first-fit) over a fixed 64 KiB pool
//! (leviculum-nrf/src/lib.rs). A first-fit allocator can FRAGMENT under churny
//! variable-size alloc/free even while live bytes stay flat: a large request
//! eventually fails although total free exceeds it. Flat-live-bytes tests
//! cannot see that. To see it we replay the core's real allocation pattern
//! against the real allocator at the real size.
//!
//! Method (record + replay, single test so the recording flag is never racing
//! another test in this binary):
//!  1. RECORD: a recording global allocator wraps System and, only while the
//!     RECORDING flag is set, logs every op as `Alloc{ptr,size,align}` /
//!     `Dealloc{ptr}` in order. The flag is set only around a churny core
//!     activity loop (announce RX + link establish/teardown + data packet
//!     RX/TX), so setup and harness allocations are excluded. Re-entry from the
//!     trace Vec's own growth is gated by a thread-local guard, so the logger
//!     never records itself.
//!  2. REPLAY: feed the trace, in order, to a fresh `LlffHeap` initialised on a
//!     64 KiB buffer (HEAP_SIZE). A map ties each recorded ptr to the ptr the
//!     LlffHeap returned. Alloc calls `heap.alloc(layout)`; a null return while
//!     `heap.free() >= size` is FRAGMENTATION-DRIVEN EXHAUSTION (a null while
//!     `free < size` is the pool being genuinely full). Dealloc frees the
//!     mapped ptr. `used`, `free`, and the largest still-allocatable block are
//!     sampled along the way.
//!
//! Because System addresses are unique across live allocations and an address
//! cannot be handed out twice without an intervening free, keying the replay
//! map by recorded address is sound: at most one live entry per address.
//!
//! Run: `cargo test -p leviculum-core --test heap_fragmentation -- --nocapture`

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use embedded_alloc::LlffHeap;
use rand_core::OsRng;

use leviculum_core::constants::MGMT_ANNOUNCE_INTERVAL_MS;
use leviculum_core::constants::MTU;
use leviculum_core::traits::Clock;
use leviculum_core::transport::{Transport, TransportConfig};
use leviculum_core::{
    Action, Destination, DestinationHash, DestinationType, Direction, EmbeddedStorage, Identity,
    InterfaceId, LinkId, MemoryStorage, NoStorage, NodeCore, NodeCoreBuilder, ProofStrategy,
    SendError, TickOutput,
};

// ----------------------------------------------------------------------------
// Recording global allocator: forwards to System, logs ops while RECORDING.
// ----------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Op {
    Alloc {
        ptr: usize,
        size: usize,
        align: usize,
    },
    Dealloc {
        ptr: usize,
    },
}

struct RecordingAlloc;

static RECORDING: AtomicBool = AtomicBool::new(false);
static TRACE: Mutex<Vec<Op>> = Mutex::new(Vec::new());

thread_local! {
    // Set while we are inside `record`, so the trace Vec's own (de)allocations
    // do not recurse into the logger. const-init: no allocation on first touch.
    static IN_RECORD: Cell<bool> = const { Cell::new(false) };
}

fn record(op: Op) {
    IN_RECORD.with(|flag| {
        if flag.get() {
            return;
        }
        flag.set(true);
        if let Ok(mut trace) = TRACE.lock() {
            trace.push(op);
        }
        flag.set(false);
    });
}

// SAFETY: every op forwards to System with the same layout it was handed; the
// logger only reads the returned address as an integer and never touches the
// returned memory. realloc is intentionally NOT overridden so the default impl
// decomposes it into a recorded alloc + dealloc, matching what LlffHeap sees.
unsafe impl GlobalAlloc for RecordingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() && RECORDING.load(Ordering::Relaxed) {
            record(Op::Alloc {
                ptr: ptr as usize,
                size: layout.size(),
                align: layout.align(),
            });
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if RECORDING.load(Ordering::Relaxed) {
            record(Op::Dealloc { ptr: ptr as usize });
        }
        System.dealloc(ptr, layout);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() && RECORDING.load(Ordering::Relaxed) {
            record(Op::Alloc {
                ptr: ptr as usize,
                size: layout.size(),
                align: layout.align(),
            });
        }
        ptr
    }
}

#[global_allocator]
static GLOBAL: RecordingAlloc = RecordingAlloc;

// ----------------------------------------------------------------------------
// Host doubles (mirrors heap_leak.rs; test_utils is pub(crate)).
// ----------------------------------------------------------------------------

const START_MS: u64 = 1_000_000;
const IFACE_IDX: usize = 0;
const IFACE: InterfaceId = InterfaceId(0);

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

type EndpointNode = NodeCore<OsRng, StepClock, NoStorage>;

fn outbound(out: &TickOutput) -> Vec<Vec<u8>> {
    out.actions
        .iter()
        .map(|a| match a {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data.clone(),
        })
        .collect()
}

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

fn make_responder(clock_ms: u64) -> (EndpointNode, StepClock, DestinationHash, [u8; 32], Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "heapfrag",
        &["responder"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();

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
// Churn: one round mixes the three repeated LNode activities so the recorded
// trace has the same variable-size alloc/free interleaving the firmware sees.
// ----------------------------------------------------------------------------

const ANNOUNCE_PEERS: usize = 3;
const ANNOUNCE_STEP_MS: u64 = 3_000;
const DATA_PACKETS_PER_ROUND: u64 = 4;

/// Mirror of the firmware `EmbeddedInterface` serial channel: capacity 8,
/// drop on overflow. Holding the to_vec'd frames briefly before they drain
/// puts the #65 serial `to_vec` allocations into the recorded trace.
const SERIAL_CAP: usize = 8;

struct Churn {
    transport: Transport<StepClock, MemoryStorage>,
    peers: Vec<Destination>,
    a: EndpointNode,
    b: EndpointNode,
    a_clock: StepClock,
    b_clock: StepClock,
    dest_hash: DestinationHash,
    key: [u8; 32],
    announce: Vec<u8>,
    link_id: LinkId,
    payload: Vec<u8>,
    // Management-announce node (the #65-pinned path): a probe responder on the
    // firmware storage, announcing through a bounded serial-style queue.
    mgmt: NodeCore<OsRng, StepClock, EmbeddedStorage>,
    mgmt_clock: StepClock,
    serial: std::collections::VecDeque<Vec<u8>>,
    rounds_run: u64,
    links_established: u64,
    packets_sent: u64,
    mgmt_announces: u64,
}

impl Churn {
    fn new() -> Self {
        // Announce-RX transport (LNodes are transport nodes; rebroadcast on).
        let clock = StepClock::new(START_MS);
        let identity = Identity::generate(&mut OsRng);
        let config = TransportConfig {
            enable_transport: true,
            path_expiry_secs: 365 * 24 * 3600,
            ..TransportConfig::default()
        };
        let transport = Transport::new(config, clock, MemoryStorage::with_defaults(), identity);
        let peers: Vec<Destination> = (0..ANNOUNCE_PEERS)
            .map(|i| {
                let id = Identity::generate(&mut OsRng);
                let aspect = ["peer0", "peer1", "peer2"][i % 3];
                Destination::new(
                    Some(id),
                    Direction::In,
                    DestinationType::Single,
                    "heapfrag",
                    &[aspect],
                )
                .unwrap()
            })
            .collect();

        // Long-lived link for the data-packet activity.
        let (mut b, b_clock, dest_hash, key, announce) = make_responder(START_MS);
        let (mut a, a_clock) = make_initiator(START_MS);
        let out = a.handle_packet(IFACE, &announce);
        settle(&mut a, &mut b, out);
        let link_id = establish(&mut a, &mut b, dest_hash, &key)
            .expect("data link must establish for the fragmentation churn");

        // Management-announce node on the firmware storage with the probe
        // responder armed (registers rnstransport.probe into mgmt_destinations).
        let mgmt_clock = StepClock::new(START_MS);
        let mgmt = NodeCoreBuilder::new()
            .enable_transport(true)
            .respond_to_probes(true)
            .build(OsRng, mgmt_clock.clone(), EmbeddedStorage::new());

        Self {
            transport,
            peers,
            a,
            b,
            a_clock,
            b_clock,
            dest_hash,
            key,
            announce,
            link_id,
            payload: b"heap-fragmentation-probe-payload-0123456789".to_vec(),
            mgmt,
            mgmt_clock,
            serial: std::collections::VecDeque::new(),
            rounds_run: 0,
            links_established: 0,
            packets_sent: 0,
            mgmt_announces: 0,
        }
    }

    /// One round: announce RX from the peer set, one ephemeral link
    /// establish+teardown, and a few data packets over the long-lived link.
    fn round(&mut self) {
        // Announce RX.
        let now = self.transport.clock().now_ms();
        self.transport.storage_mut().clear_packet_hashes();
        for dest in self.peers.iter_mut() {
            let announce = dest.announce(None, &mut OsRng, now, now / 1000).unwrap();
            let mut buf = [0u8; MTU];
            let len = announce.pack(&mut buf).unwrap();
            let _ = self.transport.process_incoming(IFACE_IDX, &buf[..len]);
        }
        self.transport.poll();
        let _ = self.transport.drain_actions();
        let _ = self.transport.drain_events().count();
        self.transport.clock().advance(ANNOUNCE_STEP_MS);

        // Ephemeral link establish + teardown (re-teach the path each round).
        let out = self.a.handle_packet(IFACE, &self.announce);
        settle(&mut self.a, &mut self.b, out);
        if let Some(eph) = establish(&mut self.a, &mut self.b, self.dest_hash, &self.key) {
            self.links_established += 1;
            let out = self.a.close_link(&eph);
            settle(&mut self.a, &mut self.b, out);
        }

        // Data packets over the long-lived link.
        for _ in 0..DATA_PACKETS_PER_ROUND {
            match self.a.send_on_link(&self.link_id, &self.payload) {
                Ok(out) => {
                    self.packets_sent += 1;
                    settle(&mut self.a, &mut self.b, out);
                }
                Err(SendError::Busy) | Err(SendError::PacingDelay { .. }) => {}
                Err(e) => panic!("unexpected send error: {e}"),
            }
            self.a_clock.advance(50);
            self.b_clock.advance(50);
            let out = self.a.handle_timeout();
            settle(&mut self.a, &mut self.b, out);
            let out = self.b.handle_timeout();
            settle(&mut self.b, &mut self.a, out);
        }

        // Management announce (#65-pinned path): step past the deadline so one
        // announce fires, then push every broadcast through the bounded serial
        // queue (to_vec, cap 8, drop on overflow) and drain it, exactly as the
        // firmware's EmbeddedInterface + retic_serial_task do.
        self.mgmt_clock.advance(MGMT_ANNOUNCE_INTERVAL_MS + 1);
        let out = self.mgmt.handle_timeout();
        for action in &out.actions {
            let (Action::Broadcast { data, .. } | Action::SendPacket { data, .. }) = action;
            if matches!(action, Action::Broadcast { .. }) {
                self.mgmt_announces += 1;
            }
            if self.serial.len() < SERIAL_CAP {
                self.serial.push_back(data.clone());
            }
        }
        self.serial.clear();

        self.rounds_run += 1;
    }
}

// ----------------------------------------------------------------------------
// Replay against a real 64 KiB LlffHeap.
// ----------------------------------------------------------------------------

const HEAP_SIZE: usize = 64 * 1024; // matches leviculum-nrf HEAP_SIZE
const SAMPLE_EVERY: usize = 5_000; // ops between fragmentation samples

/// Largest block still allocatable right now (align 8), by binary search. Each
/// probe allocs then immediately frees, so it does not perturb heap state.
fn largest_block(heap: &LlffHeap) -> usize {
    let (mut lo, mut hi) = (0usize, HEAP_SIZE);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let probe = match Layout::from_size_align(mid, 8) {
            Ok(l) => l,
            Err(_) => {
                hi = mid - 1;
                continue;
            }
        };
        // SAFETY: probe alloc on this heap; freed immediately with same layout.
        let p = unsafe { GlobalAlloc::alloc(heap, probe) };
        if p.is_null() {
            hi = mid - 1;
        } else {
            unsafe { GlobalAlloc::dealloc(heap, p, probe) };
            lo = mid;
        }
    }
    lo
}

struct ReplayResult {
    ops: usize,
    exhausted_at: Option<usize>,
    exhaust_fragmentation: bool,
    exhaust_req_size: usize,
    exhaust_free: usize,
    peak_used: usize,
    min_largest_block: usize,
    final_used: usize,
    final_free: usize,
    final_largest_block: usize,
}

fn replay(trace: &[Op]) -> ReplayResult {
    // 64 KiB region for the heap, page-aligned, on the System heap. RECORDING is
    // off here, so this allocation is not part of the trace.
    let region_layout = Layout::from_size_align(HEAP_SIZE, 4096).expect("valid region layout");
    // SAFETY: non-zero size; freed at the end with the same layout. The pointer
    // is handed to the heap as its backing store and never dereferenced here.
    let region = unsafe { System.alloc(region_layout) };
    assert!(!region.is_null(), "could not reserve 64 KiB replay region");

    let heap = LlffHeap::empty();
    // SAFETY: `region` is valid for HEAP_SIZE bytes; init is called exactly once.
    unsafe { heap.init(region as usize, HEAP_SIZE) };

    let mut live: HashMap<usize, (*mut u8, Layout)> = HashMap::new();
    let mut peak_used = 0usize;
    let mut min_largest_block = HEAP_SIZE;
    let mut exhausted_at = None;
    let mut exhaust_fragmentation = false;
    let mut exhaust_req_size = 0usize;
    let mut exhaust_free = 0usize;

    for (i, op) in trace.iter().enumerate() {
        match *op {
            Op::Alloc { ptr, size, align } => {
                let layout = match Layout::from_size_align(size, align) {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                // SAFETY: alloc on the replay heap; tracked for later dealloc.
                let got = unsafe { GlobalAlloc::alloc(&heap, layout) };
                if got.is_null() {
                    let free = heap.free();
                    exhausted_at = Some(i);
                    exhaust_fragmentation = free >= size;
                    exhaust_req_size = size;
                    exhaust_free = free;
                    break;
                }
                // Address reuse is impossible while the prior owner is still
                // live (System never hands a live address out twice), so any
                // existing entry here would be a bug; overwrite is harmless.
                live.insert(ptr, (got, layout));
                let used = heap.used();
                if used > peak_used {
                    peak_used = used;
                }
            }
            Op::Dealloc { ptr } => {
                if let Some((got, layout)) = live.remove(&ptr) {
                    // SAFETY: `got`/`layout` are exactly what this heap returned.
                    unsafe { GlobalAlloc::dealloc(&heap, got, layout) };
                }
            }
        }
        if i % SAMPLE_EVERY == 0 {
            let lb = largest_block(&heap);
            if lb < min_largest_block {
                min_largest_block = lb;
            }
        }
    }

    let final_used = heap.used();
    let final_free = heap.free();
    let final_largest_block = largest_block(&heap);
    if final_largest_block < min_largest_block {
        min_largest_block = final_largest_block;
    }

    // Free everything still mapped, then release the region. (Cleanliness only;
    // the result is already captured.)
    for (_, (got, layout)) in live.drain() {
        // SAFETY: matched ptr/layout from this heap.
        unsafe { GlobalAlloc::dealloc(&heap, got, layout) };
    }
    // SAFETY: same ptr/layout used to reserve the region.
    unsafe { System.dealloc(region, region_layout) };

    ReplayResult {
        ops: trace.len(),
        exhausted_at,
        exhaust_fragmentation,
        exhaust_req_size,
        exhaust_free,
        peak_used,
        min_largest_block,
        final_used,
        final_free,
        final_largest_block,
    }
}

/// Top allocation sizes in the trace (count and total bytes), the churn's
/// dominant fragmentation drivers.
fn size_histogram(trace: &[Op]) -> Vec<(usize, usize)> {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for op in trace {
        if let Op::Alloc { size, .. } = *op {
            *counts.entry(size).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(usize, usize)> = counts.into_iter().collect();
    v.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    v
}

// ----------------------------------------------------------------------------
// The test.
// ----------------------------------------------------------------------------

// Recording rounds kept small so the whole record+replay runs well under a
// minute under default parallel `cargo test`. Each round emits thousands of
// ops, so a few hundred rounds is a multi-100k-op trace: plenty to surface
// first-fit fragmentation if the core pattern drives it.
const WARMUP_ROUNDS: u64 = 40;
const RECORD_ROUNDS: u64 = 300;

#[test]
fn lnode_heap_fragmentation_replay() {
    let mut churn = Churn::new();

    // Warm up to steady state with recording OFF.
    for _ in 0..WARMUP_ROUNDS {
        churn.round();
    }

    // Record the churn.
    {
        let mut trace = TRACE.lock().unwrap_or_else(|e| e.into_inner());
        trace.clear();
    }
    RECORDING.store(true, Ordering::SeqCst);
    for _ in 0..RECORD_ROUNDS {
        churn.round();
    }
    RECORDING.store(false, Ordering::SeqCst);

    // Take the trace out so later allocations (replay, reporting) cannot touch
    // the global Vec while we read it.
    let trace: Vec<Op> = {
        let mut guard = TRACE.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };

    assert!(
        churn.links_established >= RECORD_ROUNDS,
        "churn must establish a link every recorded round ({} of {}), else the \
         trace is not exercising link churn",
        churn.links_established,
        RECORD_ROUNDS,
    );
    assert!(
        churn.packets_sent >= RECORD_ROUNDS,
        "churn must send data packets ({} sent over {} rounds), else the trace \
         is not exercising the data path",
        churn.packets_sent,
        RECORD_ROUNDS,
    );
    assert!(
        churn.mgmt_announces >= RECORD_ROUNDS,
        "churn must fire a management announce every round ({} over {} rounds), \
         else the trace is not exercising the #65-pinned mgmt-announce path",
        churn.mgmt_announces,
        RECORD_ROUNDS,
    );

    let allocs = trace
        .iter()
        .filter(|op| matches!(op, Op::Alloc { .. }))
        .count();
    let deallocs = trace.len() - allocs;
    println!(
        "[heap_frag] recorded {} ops ({} alloc / {} dealloc) over {} rounds \
         ({} links, {} packets)",
        trace.len(),
        allocs,
        deallocs,
        churn.rounds_run,
        churn.links_established,
        churn.packets_sent,
    );
    assert!(
        allocs > 10_000,
        "trace too small to be meaningful: {allocs} allocs"
    );

    let r = replay(&trace);

    println!(
        "[heap_frag] replay over {} ops on a {} KiB LlffHeap:",
        r.ops,
        HEAP_SIZE / 1024,
    );
    println!(
        "[heap_frag]   peak_used={} B ({:.1}% of pool)  final_used={} B  final_free={} B",
        r.peak_used,
        100.0 * r.peak_used as f64 / HEAP_SIZE as f64,
        r.final_used,
        r.final_free,
    );
    println!(
        "[heap_frag]   largest_block: min_seen={} B  final={} B",
        r.min_largest_block, r.final_largest_block,
    );

    println!("[heap_frag] dominant alloc sizes (size B : count):");
    for (size, count) in size_histogram(&trace).into_iter().take(12) {
        println!("[heap_frag]   {size:>6} B : {count}");
    }

    match r.exhausted_at {
        Some(i) if r.exhaust_fragmentation => {
            // #65 reproduced host-side: fragmentation-driven exhaustion.
            panic!(
                "FRAGMENTATION EXHAUSTION at op {i}/{}: a {}-byte request returned \
                 null while {} B free remained (peak_used {} B). The 64 KiB \
                 LlffHeap fragments under the core churn -> #65 reproduced. See \
                 the dominant-sizes histogram above for the fix target.",
                r.ops, r.exhaust_req_size, r.exhaust_free, r.peak_used,
            );
        }
        Some(i) => {
            // Null while free < size: the pool is genuinely full, not fragmented.
            panic!(
                "POOL FULL at op {i}/{}: a {}-byte request returned null with only \
                 {} B free (peak_used {} B). The core's steady-state working set \
                 does not fit in 64 KiB on a real LlffHeap (this is not \
                 fragmentation, it is capacity).",
                r.ops, r.exhaust_req_size, r.exhaust_free, r.peak_used,
            );
        }
        None => {
            // Did NOT exhaust. Report clearly: #65 is not driven by the shared
            // core's allocation pattern; it must be firmware-only allocations
            // (lora/usb/ble per-frame buffers) the reviewer takes on-device.
            println!(
                "[heap_frag] NO EXHAUSTION: the 64 KiB LlffHeap absorbed all {} ops \
                 of core churn. peak_used {} B ({:.1}%), smallest largest-block seen \
                 {} B. The shared-core pattern does NOT fragment the real allocator \
                 at the real size -> #65 is driven by firmware-only allocations \
                 (leviculum-nrf lora/usb/ble per-frame buffers), not the shared core.",
                r.ops,
                r.peak_used,
                100.0 * r.peak_used as f64 / HEAP_SIZE as f64,
                r.min_largest_block,
            );
        }
    }
}
