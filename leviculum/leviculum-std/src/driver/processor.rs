//! In-driver core processor seam (Codeberg #196, design B).
//!
//! A consumer registers a [`CoreProcessor`]; inside the driver's tick it
//! receives `&mut StdNodeCore` plus each [`NodeEvent`] and returns a
//! [`TickOutput`] the driver applies on its own send path. `leviculum-std`
//! stays protocol-agnostic: `leviculum-lxmf`'s `LxmfNode` is the first
//! consumer, `leviculum-lxst` the expected second.
//!
//! The production precedent is the in-loop `/status` responder
//! (`super::remote_mgmt`): take the lock, build a small bundle, hand back a
//! `TickOutput`, return. This is that shape, made generic and public.
//!
//! # The one rule: a hook must not re-enter the node
//!
//! Everything below is a consequence of one fact. `run_event_tap` and the
//! driver's timer branch call the hooks **with the core mutex already held**,
//! and that mutex is a non-reentrant `std::sync::Mutex`. Any path from a hook
//! body back to that mutex deadlocks the node on the spot — not later, not
//! under load, on the first call.
//!
//! The seam closes the two paths it can close and names the one it cannot:
//!
//! * [`CoreProcessor::on_event`] and [`CoreProcessor::on_tick`] are
//!   synchronous `fn`s. A `.await` in either body does not compile, which
//!   forecloses the driver's public async API: those methods end in
//!   `action_dispatch_tx.send(output).await` on a bounded channel the same
//!   loop drains, and only an `.await` can complete that send.
//! * The handle is `&mut StdNodeCore` — the sans-io core, which owns no
//!   channel to the loop and has no async surface at all. Every type that can
//!   reach `action_dispatch_tx` ([`super::PacketSender`],
//!   [`super::LinkHandle`], [`super::ReticulumNode`]) is constructed from
//!   `ReticulumNode`, never from the core.
//!
//! Both are pinned by `scripts/check-processor-compile-fail.sh`, which builds
//! the `leviculum-std/tests/cf_*.rs` fixtures and asserts each fails with one
//! specific error code. `just fast` runs it.
//!
//! # The residue: any smuggled handle that re-locks the core
//!
//! A processor's own struct may hold anything, and the hole is wider and much
//! duller than the async API. It is **not** primarily
//! `futures::executor::block_on` over a smuggled [`super::PacketSender`]: that
//! sender releases the core lock before its `.await`
//! (`leviculum-std/src/driver/sender.rs:76-86`), so such a call does not reach
//! the bounded channel at all. It deadlocks earlier, on the mutex.
//!
//! And no `block_on` is needed to get there. [`super::ReticulumNode`] carries
//! roughly forty plain synchronous `pub fn`s that open with
//! `self.inner.lock_recover()` — `has_path`, `hops_to`, `get_identity`,
//! `transport_stats`, `link_is_established` and so on. A processor holding an
//! `Arc<ReticulumNode>` deadlocks the entire node on its first `on_event`, in
//! safe synchronous code, with no `.await`, no channel and nothing for a
//! compile-fail fixture to catch. The async API is one instance of the rule,
//! and not the instance a consumer is likely to hit.
//!
//! That hole is not closed — it cannot be, see below — but since Codeberg #198
//! it is no longer silent. Every acquisition through
//! [`crate::sync_ext::MutexRecover`] checks a thread-local set of held mutex
//! addresses first, so a hook that re-locks the core panics with the mutex
//! named instead of parking the event loop. The panic lands in the
//! `catch_unwind` below: the processor is detached, `CoreProcessorPanicked` is
//! emitted, and the node keeps serving. See
//! `docs/src/concepts/self-deadlock-tripwire.md`.
//!
//! What the seam actually guarantees is narrower than "the mistake cannot be
//! written":
//!
//! 1. It never *hands out* a re-entrant handle. `&mut StdNodeCore` is the
//!    whole surface, and it locks nothing.
//! 2. Registration happens on [`super::ReticulumNodeBuilder`], **before the
//!    node exists**. A processor therefore cannot be constructed holding a
//!    handle to the node it will run inside; injecting one afterwards takes a
//!    deliberate `OnceLock`/`Weak` and a reference cycle the consumer has to
//!    build on purpose. That is a construction-order barrier, not a type-level
//!    one, but it is the reason this hazard stays theoretical in practice.
//! 3. The natural spellings of the async route do not compile.
//!
//! Beyond that it is prose, and [`CoreProcessor`] says so where a consumer
//! writing one will read it.
//!
//! # A hook owns the events of the core calls it makes
//!
//! The tap is fed from `output.events` inside the driver's `dispatch_output`,
//! i.e. from what the *driver's* tick produced. A core call made inside a hook
//! body is a different thing: `announce_destination`, `request_path`, and
//! everything else a consumer reaches on `&mut StdNodeCore` return their
//! events **synchronously**, on the `TickOutput` handed back to the caller.
//! Those events never pass the tap, and there is no later tick on which they
//! will — `run_event_tap` dispatches a hook's merged output with the
//! processor detached (see "The recursion bound is one" on that function), and
//! the driver has no other route back in.
//!
//! So the obligation is the consumer's, and it is the price of the seam rather
//! than a defect in it: **a hook owns the events returned by the core calls it
//! makes.** Putting them on the `TickOutput` the hook returns forwards them to
//! the application's event sink, which is usually what a consumer wants; it
//! does *not* route them back through the processor. A consumer whose own
//! state machine has to see them must feed them back itself, inside the same
//! hook call — and must bound that loop. A state machine that legitimately
//! emits events in response to events has no fixpoint the seam can promise,
//! and an unbounded loop under the core lock is a node hang rather than a bug
//! report.
//!
//! Both consumers in this tree already do exactly that, which is why the rule
//! is written here rather than rediscovered a third time:
//!
//! * `absorb_core` (leviculum-std/tests/rnsd_interop/lxmf_interop_tests.rs:177)
//!   walks a work queue of `TickOutput`s, routing every event through
//!   `LxmfRouter::handle_event` and pushing the output that produces back onto
//!   the queue.
//! * `absorb` (leviculum-lxmf-node/src/processor.rs:394) is the same loop
//!   inside a real hook: it re-feeds each event to the router *and* forwards
//!   it on the returned `TickOutput`, under a round cap —
//!   `MAX_ABSORB_ROUNDS` (leviculum-lxmf-node/src/processor.rs:84).
//!
//! # Registration happens before the channel exists
//!
//! A processor is installed on [`super::ReticulumNodeBuilder`], i.e. before
//! [`super::ReticulumNode::start`] constructs the real `action_dispatch_tx`.
//! Any handle cloned off the node at build time carries the placeholder channel
//! created in `ReticulumNode::new`, whose receiver is dropped immediately — a
//! send on it fails rather than blocks.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use leviculum_core::node::NodeEvent;
use leviculum_core::transport::TickOutput;

use crate::sync_ext::MutexRecover;

use super::StdNodeCore;

/// The documented ceiling on how long one processor hook may hold the core
/// lock.
///
/// It is a reported ceiling, not an enforced one, and that is a property of
/// the language rather than a shortcut: a synchronous `fn` cannot be
/// preempted, so the only way to bound it by force would be to run it on
/// another thread — at which point it no longer holds `&mut StdNodeCore`
/// inside the tick, which is the entire seam. Exceeding it emits
/// `CORE_PROCESSOR_OVER_BUDGET`.
///
/// # What the report measures, and what it does not
///
/// One hook invocation: one `run_tick`, or the `on_event` calls of one
/// `dispatch_output`. It deliberately starts *after* the core lock is taken,
/// so time spent waiting for another thread's 141 ms `send_resource` is not
/// charged to a processor that did nothing.
///
/// It does **not** measure a driver tick. A timer tick runs `on_tick` and then
/// dispatches, and a busy second can carry many dispatches; each is timed
/// against the budget on its own. A processor spending 4.9 ms in every one of
/// them never trips the report while costing far more than 5 ms of lock. The
/// budget is a per-call tripwire, and the aggregate is what an operator has to
/// read off `CORE_LOCK_*` timings instead.
///
/// # Where the number comes from
///
/// `docs/src/concepts/core-lock-budget.md` rather than a feeling. The page's
/// own tolerance line is drawn at 3.2 ms: packing a 1 MiB LXMF message
/// (0.8 ms) and unpacking one with signature verification (3.2 ms) are listed
/// as costs that explicitly do *not* justify phasing. The failure mode it
/// names is `LxmfRouter::tick`
/// (`docs/src/concepts/core-lock-budget.md:147`) at 126.6 ms for 8 due 256 KiB
/// messages, against the 141 ms measured `send_resource` stall. 5 ms
/// therefore sits just above everything the page calls acceptable and ~25x
/// below what it calls the defect, which is the widest gap available that
/// still separates the two.
pub const PROCESSOR_TICK_BUDGET: Duration = Duration::from_millis(5);

/// A synchronous state machine the driver runs inside its own tick.
///
/// The consumer plugs in `LxmfNode` (or any other core-driving state machine);
/// the driver hands it `&mut StdNodeCore` and each [`NodeEvent`] the core
/// produced, and dispatches the returned [`TickOutput`] on the same path as
/// its own. See the `driver::processor` module documentation for what the
/// shape makes impossible and why.
///
/// # Where the events come from
///
/// The tap sits on `output.events` inside the driver's `dispatch_output`,
/// *before* the event sink classifies. Seven of the event types LXMF needs,
/// including `PacketReceived` and `LinkDataReceived` — how a message
/// *arrives* — are [`EventClass::Data`] and therefore droppable under load. A
/// processor fed from the public
/// [`take_event_receiver`](super::ReticulumNode::take_event_receiver) would
/// silently lose inbound messages with nothing underneath to retransmit them.
///
/// [`EventClass::Data`]: leviculum_core::node::EventClass::Data
///
/// # Answering in the same tick
///
/// Three events cannot be deferred: `PacketProofRequested` →
/// `send_proof_on_interface`, `LinkProofRequested` → `send_data_proof`, and
/// `ResourceAdvertised` → `accept_resource`/`reject_resource`. Call them on
/// the `core` handle and return the resulting `TickOutput`; the driver
/// dispatches it before it returns from the same `dispatch_output`.
///
/// # Recursion, and the events you have to re-feed yourself
///
/// A processor does **not** see the events of its own `TickOutput`, from
/// either method. The bound is one, and `run_event_tap` in this module
/// carries the reasoning.
///
/// That bound has a consequence a consumer has to plan for. A core call made
/// from inside a hook — `announce_destination`, `request_path`, anything else
/// on the `core` handle — returns its events *synchronously*, on the
/// `TickOutput` it hands back to you. Those events are yours: the driver
/// dispatches your merged output with this processor detached, so nothing
/// routes them back through `on_event`. Returning them forwards them to the
/// application's event sink; if your own state machine has to see them, feed
/// them back to it inside the same hook call, under a round cap — a machine
/// that emits events in response to events has no fixpoint this seam can
/// promise, and an unbounded loop under the core lock is a node hang. The
/// module documentation names the two places in this tree that already do it.
///
/// # Talking to the rest of the application
///
/// A processor owns whatever channels it likes, but may only use non-blocking
/// sends on them (`try_send`, `std::sync::mpsc::Sender::send`). Anything that
/// can block belongs on the other side of the queue.
///
/// **It may not own a handle to the node it runs inside.** Both methods are
/// called with the core mutex held, and that mutex is not reentrant, so any
/// call that re-locks it hangs the node. That is not confined to the async
/// API: [`super::ReticulumNode`] exposes roughly forty synchronous `pub fn`s
/// that lock the core — `has_path`, `hops_to`, `get_identity`,
/// `transport_stats` — and one of them in an `on_event` body is a deadlock in
/// ordinary safe code. Everything a hook needs is on the `core` argument;
/// anything else belongs on the far side of a channel.
///
/// Since Codeberg #198 that mistake reports itself: the re-entrant acquisition
/// panics naming the mutex, this hook is detached, and the node continues. It
/// is still a bug in the processor and it still costs you the processor — the
/// tripwire makes it visible, it does not make it survivable.
///
/// # The one call on `core` that must not be made
///
/// `NodeCore::send_resource` (`leviculum-core/src/node/mod.rs:1308`) is `pub`
/// and reachable on the handle. It is the composed single call that
/// `docs/src/concepts/core-lock-budget.md` was written about: 141 ms under the
/// lock for a 1 MiB payload, a 32 % inbound-throughput stall (Codeberg #152).
/// It exists for the no_std and FFI callers that have no lock to hold. From a
/// hook, use the three-phase form the driver itself uses —
/// `resource_send_params`, `resource::prepare_resource_send` off the lock,
/// `commit_resource_send` — or hand the send to the application side of a
/// channel. [`PROCESSOR_TICK_BUDGET`] reports the composed call 141 ms after
/// the fact; it does not prevent it.
///
/// # Panics
///
/// A panic in either method is caught. The driver logs
/// `CORE_PROCESSOR_PANICKED`, detaches the processor permanently, emits
/// [`NodeEvent::CoreProcessorPanicked`] on the application's control plane and
/// carries on serving the mesh without it. Re-registration is builder-only, so
/// recovery means building a new node. The core may be left mid-mutation by
/// the panicking call, the same trade-off `crate::sync_ext::MutexRecover`
/// makes for poison.
pub trait CoreProcessor: Send + 'static {
    /// Consume one event the core produced this tick.
    ///
    /// Runs with the core lock held. Return `TickOutput::empty()` for events
    /// this processor does not care about — the driver skips dispatch when the
    /// merged output is empty.
    ///
    /// The `next_deadline_ms` of the returned output is honoured: the driver
    /// brings its next poll forward to meet it.
    fn on_event(&mut self, core: &mut StdNodeCore, event: &NodeEvent) -> TickOutput;

    /// Periodic slot, called from the driver's timer branch (at most ~1 s
    /// apart, sooner when the core has an earlier deadline).
    ///
    /// This is where a processor drains its own outbound command queue and
    /// runs its own timers — an event tap alone can never *initiate* anything,
    /// because it only fires when the core has something to say. Runs with the
    /// core lock held, under the same [`PROCESSOR_TICK_BUDGET`].
    ///
    /// The returned `next_deadline_ms` is folded into the driver's next poll
    /// with `min()` against the core's own. A deadline already in the past
    /// pins the driver to its 1 ms floor, i.e. a thousand lock acquisitions a
    /// second: return `None` when there is nothing to wake for rather than a
    /// stale timestamp.
    ///
    /// The default implementation does nothing.
    fn on_tick(&mut self, core: &mut StdNodeCore, now_ms: u64) -> TickOutput {
        let _ = (core, now_ms);
        TickOutput::empty()
    }
}

/// The registered processor, plus the driver's ability to take it away.
///
/// A slot rather than a bare `Box<dyn CoreProcessor>` because detaching has to
/// be possible from inside the hooks' own call frames: a panic in a hook is
/// discovered several `dispatch_output` frames below the event loop that owns
/// the processor, and the decision to never call it again has to survive the
/// return.
pub(crate) struct ProcessorSlot {
    processor: Option<Box<dyn CoreProcessor>>,
}

impl ProcessorSlot {
    pub(crate) fn new(processor: Box<dyn CoreProcessor>) -> Self {
        Self {
            processor: Some(processor),
        }
    }
}

/// Feed one tick's raw events to the processor and return its merged
/// `TickOutput`, or `None` when there is nothing to dispatch.
///
/// Called from `dispatch_output` with the raw `output.events` plus the
/// driver's own `FramesDropped` notices, before the event sink classifies
/// them.
///
/// # The recursion bound is one
///
/// The returned `TickOutput` is dispatched with the processor detached, so a
/// processor never observes the events it itself emitted. This is a bound, not
/// an oversight, and it costs nothing:
///
/// * Applying a `TickOutput` does not run the core. `dispatch_output` routes
///   actions to interfaces and forwards events; it cannot *produce* a new
///   `NodeEvent` from the protocol. Everything a processor's actions cause
///   comes back through `handle_packet`/`handle_timeout` on a later tick,
///   where the tap is live again — with one exception the driver closes by
///   hand: a `SendPacket` onto a dying interface produces a `FramesDropped`
///   inside this very `dispatch_output`, which is why those are tapped too.
///   Note the scope: this is about *applying* a `TickOutput`. A core call the
///   hook made while building one did run the core, and its events came back
///   on that output — never through here. See "A hook owns the events of the
///   core calls it makes" in the module documentation.
/// * The only events in a processor's `TickOutput` are ones it synthesised
///   itself. Feeding those back is a self-cycle with no fixpoint anyone can
///   guarantee — `LxmfNode::handle_event` legitimately emits events in
///   response to events, so the loop would be unbounded. An unbounded loop
///   here is a node hang, not a bug report.
///
/// This mirrors the `/status` responder, which passes `remote_mgmt: None` on
/// its own recursive dispatch for the same reason.
pub(crate) fn run_event_tap(
    slot: &mut ProcessorSlot,
    inner: &Arc<Mutex<StdNodeCore>>,
    events: &[&NodeEvent],
) -> Option<TickOutput> {
    if events.is_empty() {
        return None;
    }
    let processor = slot.processor.as_deref_mut()?;

    let mut merged = TickOutput::empty();
    let mut panicked = false;
    {
        // One acquisition for the whole tick, like the `/status` responder:
        // re-taking it per event would multiply the contention this seam is
        // supposed to be careful with.
        let mut core = inner.lock_recover();
        // The clock starts INSIDE the lock. Started outside, a 141 ms
        // `send_resource` on another thread would be charged to a processor
        // that had not run yet.
        let started = Instant::now();
        for event in events {
            match catch_unwind(AssertUnwindSafe(|| processor.on_event(&mut core, event))) {
                Ok(output) => merged.merge(output),
                Err(payload) => {
                    report_panic("on_event", payload.as_ref());
                    panicked = true;
                    break;
                }
            }
        }
        report_budget(started, "on_event", events.len());
    }

    if panicked {
        slot.processor = None;
        merged
            .events
            .push(NodeEvent::CoreProcessorPanicked { hook: "on_event" });
    }

    (!merged.is_empty()).then_some(merged)
}

/// Run the processor's periodic slot. The caller already holds the core lock.
///
/// The output is returned rather than merged into the core's: it is dispatched
/// on its own, detached, exactly like the tap's. See `run_event_loop`'s timer
/// branch for why.
pub(crate) fn run_tick(
    slot: &mut ProcessorSlot,
    core: &mut StdNodeCore,
    now_ms: u64,
) -> TickOutput {
    let Some(processor) = slot.processor.as_deref_mut() else {
        return TickOutput::empty();
    };
    let started = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| processor.on_tick(core, now_ms)));
    report_budget(started, "on_tick", 0);
    match result {
        Ok(output) => output,
        Err(payload) => {
            report_panic("on_tick", payload.as_ref());
            slot.processor = None;
            let mut output = TickOutput::empty();
            output
                .events
                .push(NodeEvent::CoreProcessorPanicked { hook: "on_tick" });
            output
        }
    }
}

/// Emit `CORE_PROCESSOR_OVER_BUDGET` when a hook ran longer than
/// [`PROCESSOR_TICK_BUDGET`].
///
/// Structured fields only, no trailing prose: the canonical event-log line is
/// whitespace-tokenised (see `EventSink::emit_control`).
fn report_budget(started: Instant, hook: &'static str, events: usize) {
    let elapsed = started.elapsed();
    if elapsed > PROCESSOR_TICK_BUDGET {
        tracing::warn!(
            event = "CORE_PROCESSOR_OVER_BUDGET",
            hook,
            elapsed_us = elapsed.as_micros() as u64,
            budget_us = PROCESSOR_TICK_BUDGET.as_micros() as u64,
            events,
        );
    }
}

/// Emit `CORE_PROCESSOR_PANICKED` for a hook that unwound.
///
/// The panic payload's message is the only thing worth keeping — the location
/// and backtrace already went to stderr through the default panic hook before
/// `catch_unwind` handed it back.
///
/// Two lines on purpose. The canonical event-log line is whitespace-tokenised
/// (see `EventSink::emit_control`), and a panic message is arbitrary prose, so
/// it cannot ride as a field on the `event=` line. It goes out separately,
/// with no `event=` field of its own.
fn report_panic(hook: &'static str, payload: &(dyn Any + Send)) {
    let message = payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>");
    tracing::error!(event = "CORE_PROCESSOR_PANICKED", hook);
    tracing::error!(
        "core processor {hook} panicked and has been detached; the node continues \
         without it: {message}"
    );
}
