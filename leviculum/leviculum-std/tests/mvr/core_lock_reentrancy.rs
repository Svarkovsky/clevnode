//! mvr for Codeberg #198 — a processor that re-locks the core is *named*,
//! not left to hang.
//!
//! `93ba351` shipped a seam that runs consumer code while the driver holds the
//! core `std::sync::Mutex`. That mutex is not reentrant, so any handle the
//! consumer smuggled in that re-locks the core parks the driver's event loop
//! forever: no stack, no log line, no exit code, and a listening socket still
//! open for every supervisor to see. `has_path`
//! (`leviculum-std/src/driver/mod.rs:2428-2430`) is one line, is safe, is
//! synchronous, and is one of roughly forty accessors shaped exactly like it.
//!
//! # What this reproduces
//!
//! A single node, no interfaces, no peer. Its registered [`CoreProcessor`]'s
//! `on_tick` — called from the driver's timer branch with the core guard live —
//! calls `has_path` on the node it runs inside. **Before the tripwire this test
//! hung forever**; the tripwire turns the hang into a panic inside the seam's
//! own `catch_unwind`, which detaches the processor, emits
//! `NodeEvent::CoreProcessorPanicked` and leaves the node serving.
//!
//! The smuggle is deliberate and awkward on purpose. A processor is registered
//! on the *builder*, before the node exists, so it cannot be constructed
//! holding a handle to the node it will run inside; getting one takes the
//! `OnceLock` + `Weak` cycle below. That construction-order barrier is why the
//! hazard is theoretical rather than routine (`driver::processor` documents it),
//! and it is exactly what a future `set_core_processor` on a live node would
//! give up.
//!
//! # How this is bounded, so a regression cannot wedge the suite
//!
//! Two independent bounds, because the failure under test *is* an unbounded
//! wait:
//!
//! 1. The wait for `CoreProcessorPanicked` runs under
//!    [`tokio::time::timeout`]. The node owns its **own** runtime and worker
//!    thread (`ReticulumNode::start` keeps it in `self.runtime`), so the thread
//!    that would be parked is never one of this test's, and the timeout still
//!    fires. A regression fails in `TRIPWIRE_TIMEOUT` with a message naming the
//!    deadlock instead of hanging.
//! 2. `Drop for ReticulumNode` is bounded by construction: it polls the runner
//!    handle for at most `DROP_FLUSH_BOUND` (400 ms) and then calls
//!    `Runtime::shutdown_background`, which does not join. So even a genuinely
//!    parked event loop cannot stall teardown.
//!
//! Verified rather than asserted: with the tripwire disabled by hand, this test
//! fails in ~15 s on bound 1 and the binary exits normally. See the batch
//! report.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use leviculum_core::node::NodeEvent;
use leviculum_core::transport::TickOutput;
use leviculum_core::DestinationHash;
use leviculum_std::driver::{CoreProcessor, ReticulumNode, ReticulumNodeBuilder, StdNodeCore};

/// How long the driver gets to report the re-entry.
///
/// `on_tick` fires from the timer branch at most ~1 s apart, so this is an
/// order of magnitude of slack on a loaded four-core host, not a tuned number.
const TRIPWIRE_TIMEOUT: Duration = Duration::from_secs(15);

/// Panic payloads seen by the capturing hook, so a test can assert on what the
/// tripwire *said* — the seam's `catch_unwind` swallows the payload into a
/// `tracing` line, and "it panicked" alone would be satisfied by any panic at
/// all, including one from an unrelated defect.
static PANIC_MESSAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Install a panic hook that records payloads and then chains to the previous
/// one, so ordinary panic output elsewhere in this binary is unchanged.
///
/// Installed once per process. The `mvr` gate runs `--test-threads=1`
/// (`Justfile`), so no other test is mid-panic while this reads the buffer.
fn install_capturing_panic_hook() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&'static str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned());
            if let Some(message) = message {
                PANIC_MESSAGES
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message);
            }
            previous(info);
        }));
    });
}

fn captured_panics() -> Vec<String> {
    PANIC_MESSAGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// A processor holding a handle to the node it runs inside — the smuggle the
/// seam cannot forbid.
struct Reenterer {
    /// Filled after `start()`, because the node does not exist when the
    /// processor is registered.
    node: Arc<OnceLock<Weak<ReticulumNode>>>,
    ticks: Arc<AtomicUsize>,
    /// Whether this processor re-enters at all. The negative control builds the
    /// identical node with this `false`, so a green tripwire assertion cannot
    /// be satisfied by a node that panics for some other reason.
    reenter: bool,
}

impl CoreProcessor for Reenterer {
    fn on_event(&mut self, _core: &mut StdNodeCore, _event: &NodeEvent) -> TickOutput {
        TickOutput::empty()
    }

    fn on_tick(&mut self, _core: &mut StdNodeCore, _now_ms: u64) -> TickOutput {
        self.ticks.fetch_add(1, Ordering::Relaxed);
        if !self.reenter {
            return TickOutput::empty();
        }
        let Some(node) = self.node.get().and_then(Weak::upgrade) else {
            // The test has not injected the handle yet; nothing to re-enter.
            return TickOutput::empty();
        };
        // One line, safe, synchronous, no `.await`: and the driver is holding
        // the core mutex for the duration of this call.
        let _ = node.has_path(&DestinationHash::new([0u8; 16]));
        TickOutput::empty()
    }
}

struct Fixture {
    node: Arc<ReticulumNode>,
    events: leviculum_std::driver::EventReceiver,
    ticks: Arc<AtomicUsize>,
}

/// Build and start a node whose processor re-enters (or does not), and hand the
/// caller the node plus its event stream.
async fn start_node(reenter: bool) -> Fixture {
    let storage = tempfile::tempdir().expect("tempdir");
    let cell: Arc<OnceLock<Weak<ReticulumNode>>> = Arc::new(OnceLock::new());
    let ticks = Arc::new(AtomicUsize::new(0));

    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .storage_path(storage.path().to_path_buf())
        .core_processor(Reenterer {
            node: Arc::clone(&cell),
            ticks: Arc::clone(&ticks),
            reenter,
        })
        .build()
        .await
        .expect("build node");

    let events = node.take_event_receiver().expect("event receiver");
    node.start().await.expect("start node");

    // The reference cycle, completed after the fact: this is the `OnceLock` +
    // `Weak` the seam's documentation says a consumer would have to build on
    // purpose. Until it is set, `on_tick` is a no-op, so no tick before this
    // point can trip anything.
    let node = Arc::new(node);
    cell.set(Arc::downgrade(&node)).ok();

    // Keep the storage alive for the node's lifetime.
    std::mem::forget(storage);

    Fixture {
        node,
        events,
        ticks,
    }
}

/// Wait up to `TRIPWIRE_TIMEOUT` for `CoreProcessorPanicked` on `hook`.
///
/// `Ok(true)` = seen. `Err(())` = the timeout fired, which without the tripwire
/// means the driver's event loop is parked on a lock only it can release.
async fn wait_for_processor_panic(
    events: &mut leviculum_std::driver::EventReceiver,
    hook: &str,
) -> Result<(), ()> {
    let seen = tokio::time::timeout(TRIPWIRE_TIMEOUT, async {
        while let Some(event) = events.recv().await {
            if let NodeEvent::CoreProcessorPanicked { hook: h } = event {
                if h == hook {
                    return true;
                }
            }
        }
        false
    })
    .await;
    match seen {
        Ok(true) => Ok(()),
        _ => Err(()),
    }
}

/// Acceptance for #198: the re-entry is reported by name, and the node survives.
///
/// Without the tripwire the driver parks inside `has_path` and this test never
/// completes — that hang is the demonstration, and `TRIPWIRE_TIMEOUT` is what
/// keeps a regression from wedging the suite forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_processor_relocking_the_core_is_named_not_hung() {
    install_capturing_panic_hook();
    let before = captured_panics().len();

    let Fixture {
        node,
        mut events,
        ticks,
    } = start_node(true).await;

    let outcome = wait_for_processor_panic(&mut events, "on_tick").await;

    // Read the tripwire's own words before tearing anything down.
    let messages: Vec<String> = captured_panics().split_off(before);
    let reentrant: Vec<&String> = messages
        .iter()
        .filter(|m| m.contains("re-entrant lock"))
        .collect();

    // Teardown is bounded by DROP_FLUSH_BOUND + shutdown_background even when
    // the loop is parked, so this is safe on the failure path too.
    drop(events);
    drop(node);

    assert!(
        outcome.is_ok(),
        "the driver never reported the re-entry within {TRIPWIRE_TIMEOUT:?}: its event \
         loop is parked inside has_path on a mutex only it can release, which is \
         exactly the failure #198 exists to make visible. on_tick ran {} time(s).",
        ticks.load(Ordering::Relaxed)
    );
    assert!(
        !reentrant.is_empty(),
        "the processor was detached, but nothing named a re-entrant lock — the \
         tripwire is not what stopped it. Captured: {messages:?}"
    );
    assert!(
        reentrant[0].contains("CoreProcessor"),
        "the report must point at the seam a consumer can actually fix: {}",
        reentrant[0]
    );
}

/// Negative control on the identical node: a processor that does *not* re-enter
/// must run its ticks and never be detached.
///
/// Without this, the assertion above is satisfied by a tripwire that fires on
/// every acquisition — which would take the daemon down on its first tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_processor_that_does_not_re_enter_is_left_alone() {
    install_capturing_panic_hook();

    let Fixture {
        node,
        mut events,
        ticks,
    } = start_node(false).await;

    // Long enough for several timer ticks, each of which takes the core lock
    // and runs the hook under it.
    let detached = tokio::time::timeout(
        Duration::from_secs(4),
        wait_for_processor_panic(&mut events, "on_tick"),
    )
    .await;

    let ran = ticks.load(Ordering::Relaxed);
    drop(events);
    drop(node);

    assert!(
        detached.is_err() || detached.unwrap().is_err(),
        "a processor that never touches the node must not be detached"
    );
    assert!(
        ran > 0,
        "the control proves nothing if the hook never ran: on_tick ran {ran} times"
    );
}
