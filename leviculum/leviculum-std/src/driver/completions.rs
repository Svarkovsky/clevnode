//! Completion futures resolved at the driver's dispatch layer (leviculum#42).
//!
//! A [`CompletionRegistry`] is the Arc-shared leaf the `ReticulumNode` handle
//! and the event loop both hold — the same ownership precedent as
//! `iface_stats_map`. `dispatch_output` calls [`CompletionRegistry::observe`]
//! on every event it is about to forward (ahead of the `EventSink`, so daemon
//! mode resolves futures too), and the registry resolves any waiter registered
//! for that outcome. Waiters are oneshot-backed [`Completion`] futures that
//! unregister themselves on drop, so `tokio::select!` cancellation leaves no
//! residue.
//!
//! # Locking
//!
//! The registry mutex is a LEAF: held only for map operations, never across an
//! `.await`, and never acquired while the node lock is held — `observe()`'s
//! call sites in `dispatch_output` run with the node lock released, and the
//! `sync_ext` reentrancy tripwire turns a future nesting violation into a test
//! failure instead of a silent deadlock. No wait or poll path in this module
//! ever takes the node lock (upstream Lew_Palm/leviculum#199 is building a
//! census of lock-taking pub fns; this module must shrink that pressure, not
//! grow it).
//!
//! # Race freedom
//!
//! The `*_awaited` send variants register interest BEFORE the `TickOutput` is
//! handed to the event loop, so the outcome cannot precede the registration.
//! The after-the-fact `await_*` paths close their register-late races with two
//! bounded mirrors checked under the SAME mutex observation takes — an
//! established-links set and a recent-terminal-outcomes ring — which
//! linearizes registration against event observation: an outcome is either
//! still ahead (waiter parked) or already mirrored (immediate resolve), never
//! lost between the two.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use leviculum_core::link::{LinkCloseReason, LinkId};
use leviculum_core::node::NodeEvent;
use leviculum_core::resource::ResourceError;
use tokio::sync::{broadcast, oneshot};

use crate::sync_ext::MutexRecover;

/// Established-links mirror ceiling. The mirror is self-cleaning (removed on
/// `LinkClosed`), so the cap is a defensive bound well above the realistic
/// concurrent-link envelope; hitting it evicts FIFO with a warn. A waiter for
/// an evicted-but-live link degrades to resolving at `LinkClosed` or node
/// stop, bounded by the caller's own timeout.
pub(crate) const ESTABLISHED_MIRROR_CAP: usize = 1024;

/// Recent-terminal-outcomes ring capacity. Entries are markers, never
/// payloads, so memory is bounded by entry count, not by response size.
pub(crate) const RECENT_OUTCOMES_CAP: usize = 256;

/// Ring slots of the [`EventTap`] broadcast channel (drop-oldest on overrun,
/// surfaced as [`TapEvent::Lagged`]). Mirrors the control-plane default scale
/// in `config.rs`; a driver invariant, not an operator tunable.
pub const DEFAULT_EVENT_TAP_CAPACITY: usize = 256;

/// Typed terminal error for a completion future. Every failure path the
/// driver can observe resolves waiters — a future never hangs on a dead
/// object; the caller owns any wall-clock timeout on top.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CompletionError {
    /// The link the awaited outcome depended on closed first.
    #[error("link closed ({reason:?}) before the awaited outcome")]
    LinkClosed {
        /// Why the link closed.
        reason: LinkCloseReason,
    },
    /// The awaited resource transfer failed.
    #[error("resource transfer failed: {error:?}")]
    Resource {
        /// The transfer's failure cause.
        error: ResourceError,
    },
    /// The awaited request timed out in-protocol.
    #[error("request timed out in-protocol")]
    RequestTimedOut,
    /// The outcome already happened; its payload is not mirrored
    /// (bounded-memory rule: the recent-outcomes ring stores markers, never
    /// response bytes).
    #[error("outcome already delivered on the event stream; payload not mirrored")]
    AlreadyCompleted,
    /// The node stopped before the awaited outcome.
    #[error("node stopped before the awaited outcome")]
    NodeStopped,
}

/// Sender-side resource completion (a sender's `ResourceCompleted` carries no
/// data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSentInfo {
    /// The link that carried the transfer.
    pub link_id: LinkId,
    /// Final segment index (1-based; equals `total_segments` on completion).
    pub segment_index: u32,
    /// Total number of segments the transfer was split into.
    pub total_segments: u32,
}

/// A received response, resolved from a `ResponseReceived` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseInfo {
    /// Raw msgpack-encoded response data.
    pub response_data: Vec<u8>,
    /// Resource metadata from a file response, `None` otherwise.
    pub metadata: Option<Vec<u8>>,
}

/// A pending completion. Dropping it unregisters the waiter, so it is safe in
/// `tokio::select!` (cancellation leaves no registry residue). Polling never
/// takes any node lock — only the oneshot the registry resolves.
pub struct Completion<T> {
    rx: oneshot::Receiver<Result<T, CompletionError>>,
    registry: Arc<CompletionRegistry>,
    key: CompletionKey,
    /// Distinguishes this waiter among same-key waiters on the drop path.
    token: u64,
    finished: bool,
}

impl<T> Future for Completion<T> {
    type Output = Result<T, CompletionError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `Completion` is Unpin (the oneshot receiver is), so no projection.
        let this = self.get_mut();
        match Pin::new(&mut this.rx).poll(cx) {
            Poll::Ready(result) => {
                this.finished = true;
                Poll::Ready(match result {
                    Ok(outcome) => outcome,
                    // The registry dropped the sender without resolving — only
                    // possible if the waiter was lost to a registry bug; the
                    // typed stop error keeps even that from hanging a caller.
                    Err(_) => Err(CompletionError::NodeStopped),
                })
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for Completion<T> {
    fn drop(&mut self) {
        if !self.finished {
            self.registry.unregister(self.key, self.token);
        }
    }
}

/// Resolves `Ok(())` when the link's establishment proof is observed.
pub type LinkEstablishedFuture = Completion<()>;
/// Resolves with the sender-side transfer outcome.
pub type ResourceSentFuture = Completion<ResourceSentInfo>;
/// Resolves with the response to a sent request.
pub type RequestResponseFuture = Completion<ResponseInfo>;

/// What a waiter is waiting for. Private: callers hold typed futures, never
/// keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CompletionKey {
    LinkEstablished { link_id: LinkId },
    ResourceSent { resource_hash: [u8; 32] },
    RequestResponse { request_id: [u8; 16] },
}

/// The three oneshot sender shapes, one per key shape. Registration is the
/// only insertion point and always pairs key and sender shape, so the
/// `resolve_*` mismatch arms are unreachable by construction; if a bug ever
/// reached one, the sender is dropped and the waiter resolves
/// `Err(NodeStopped)` via `RecvError` — degraded to a typed error, never a
/// hang.
enum WaiterTx {
    Link(oneshot::Sender<Result<(), CompletionError>>),
    Resource(oneshot::Sender<Result<ResourceSentInfo, CompletionError>>),
    Request(oneshot::Sender<Result<ResponseInfo, CompletionError>>),
}

impl WaiterTx {
    fn fail(self, err: CompletionError) {
        match self {
            WaiterTx::Link(tx) => {
                let _ = tx.send(Err(err));
            }
            WaiterTx::Resource(tx) => {
                let _ = tx.send(Err(err));
            }
            WaiterTx::Request(tx) => {
                let _ = tx.send(Err(err));
            }
        }
    }

    fn resolve_link(self) {
        if let WaiterTx::Link(tx) = self {
            let _ = tx.send(Ok(()));
        }
    }

    fn resolve_resource(self, info: ResourceSentInfo) {
        if let WaiterTx::Resource(tx) = self {
            let _ = tx.send(Ok(info));
        }
    }

    fn resolve_request(self, response: ResponseInfo) {
        if let WaiterTx::Request(tx) = self {
            let _ = tx.send(Ok(response));
        }
    }
}

struct Waiter {
    token: u64,
    /// Link association for the `LinkClosed` sweep — every waiter kind
    /// carries one, so no waiter can outlive the link its outcome needs.
    link_id: LinkId,
    tx: WaiterTx,
}

/// Terminal-outcome markers for the recent ring. Markers only, no payloads:
/// a late response waiter gets [`CompletionError::AlreadyCompleted`] rather
/// than the driver holding unbounded response bytes.
enum RecentOutcome {
    LinkClosed { reason: LinkCloseReason },
    ResourceSent(ResourceSentInfo),
    ResourceFailed { error: ResourceError },
    ResponseDelivered,
    RequestTimedOut,
}

struct RegistryInner {
    /// Bounded by live futures: `Completion::drop` unregisters.
    waiters: HashMap<CompletionKey, Vec<Waiter>>,
    /// Established-links mirror; capped at [`ESTABLISHED_MIRROR_CAP`].
    established: HashSet<LinkId>,
    /// FIFO eviction order for the mirror.
    established_order: VecDeque<LinkId>,
    /// Recent terminal outcomes; capped at [`RECENT_OUTCOMES_CAP`].
    recent: VecDeque<(CompletionKey, RecentOutcome)>,
    /// Lazily-created event-tap fan-out. Kept across a zero-subscriber lull
    /// (only `tap_active` is cleared) so a later subscribe reuses it; dropped
    /// by `close()` so every tap sees end-of-stream.
    tap: Option<broadcast::Sender<NodeEvent>>,
    /// Per-link count of committed-but-not-yet-terminal outgoing resource
    /// sends. The link-scoped sweep in `on_resource_sent`/`on_resource_failed`
    /// is only sound when the arriving terminal event belongs to the SOLE
    /// outstanding send on that link: a previous transfer can complete in
    /// core (freeing the `TransferInProgress` slot for a new commit) before
    /// its event dispatches, and in that window a stale event must not
    /// resolve a waiter registered for the newer send. Terminal events
    /// dispatch in creation order, so "count > 1 at dispatch" identifies a
    /// stale event exactly; the sweep defers to the true final event.
    /// Entries are removed at zero, on `LinkClosed`, and by `close()`.
    pending_sends: HashMap<LinkId, u32>,
    /// Starts at 1 so the token 0 of immediately-resolved futures never
    /// matches a parked waiter on the drop path.
    next_token: u64,
    /// Set by [`CompletionRegistry::close`]; registrations after it resolve
    /// `Err(NodeStopped)` immediately.
    closed: bool,
}

impl RegistryInner {
    fn insert_waiter(&mut self, key: CompletionKey, link_id: LinkId, tx: WaiterTx) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        self.waiters
            .entry(key)
            .or_default()
            .push(Waiter { token, link_id, tx });
        token
    }

    fn push_recent(&mut self, key: CompletionKey, outcome: RecentOutcome) {
        if self.recent.len() >= RECENT_OUTCOMES_CAP {
            self.recent.pop_front();
        }
        self.recent.push_back((key, outcome));
    }

    /// Newest ring entry for `key`, if any. Newest wins: a link id can in
    /// principle recur (re-established then re-closed), and only the latest
    /// outcome describes the object the caller can still observe.
    fn newest_recent(&self, key: CompletionKey) -> Option<&RecentOutcome> {
        self.recent
            .iter()
            .rev()
            .find_map(|(k, o)| (*k == key).then_some(o))
    }

    fn recent_link_closed(&self, link_id: LinkId) -> Option<LinkCloseReason> {
        match self.newest_recent(CompletionKey::LinkEstablished { link_id }) {
            Some(RecentOutcome::LinkClosed { reason }) => Some(*reason),
            _ => None,
        }
    }

    /// Remove and return every waiter parked under `key`.
    fn take_waiters(&mut self, key: CompletionKey) -> Vec<Waiter> {
        self.waiters.remove(&key).unwrap_or_default()
    }

    /// Remove and return every waiter associated with `link_id`, across all
    /// keys (optionally restricted to resource-sent keys).
    fn sweep_link(&mut self, link_id: LinkId, resource_keys_only: bool) -> Vec<Waiter> {
        let mut swept = Vec::new();
        for (key, ws) in self.waiters.iter_mut() {
            if resource_keys_only && !matches!(key, CompletionKey::ResourceSent { .. }) {
                continue;
            }
            let mut i = 0;
            while i < ws.len() {
                if ws[i].link_id == link_id {
                    swept.push(ws.swap_remove(i));
                } else {
                    i += 1;
                }
            }
        }
        self.waiters.retain(|_, ws| !ws.is_empty());
        swept
    }
}

/// Leaf-level completion state shared between the node handle and the event
/// loop. See the module docs for the locking and race-freedom contracts.
pub(crate) struct CompletionRegistry {
    inner: Mutex<RegistryInner>,
    /// Fast path for [`observe`](Self::observe): `false` means no live tap
    /// subscriber, so the tap branch is one relaxed atomic load — the
    /// zero-subscriber cost the issue requires.
    tap_active: AtomicBool,
}

impl CompletionRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(RegistryInner {
                waiters: HashMap::new(),
                established: HashSet::new(),
                established_order: VecDeque::new(),
                recent: VecDeque::new(),
                pending_sends: HashMap::new(),
                tap: None,
                next_token: 1,
                closed: false,
            }),
            tap_active: AtomicBool::new(false),
        })
    }

    /// The single dispatch-layer hook. Hot path: one enum-discriminant match;
    /// irrelevant variants return without locking or allocating. Relevant
    /// variants: `LinkEstablished`, `LinkClosed`, sender-side
    /// `ResourceCompleted`/`ResourceFailed`, `ResponseReceived`,
    /// `RequestTimedOut` — all `EventClass::Control`, so the primary stream
    /// cannot drop what this hook resolves on.
    ///
    /// Waiters are resolved AFTER the registry lock is released: a oneshot
    /// send wakes the waiter's task, and that wake must not run under a lock
    /// this module promises to hold for map ops only.
    ///
    /// When a tap subscriber exists, EVERY event is additionally broadcast to
    /// it — the tap sits at this layer precisely so it also sees events the
    /// data plane is entitled to drop.
    pub(crate) fn observe(&self, event: &NodeEvent) {
        if self.tap_active.load(Ordering::Relaxed) {
            self.broadcast_event(event);
        }
        match event {
            NodeEvent::LinkEstablished { link_id, .. } => self.on_link_established(*link_id),
            NodeEvent::LinkClosed {
                link_id, reason, ..
            } => self.on_link_closed(*link_id, *reason),
            NodeEvent::ResourceCompleted {
                link_id,
                resource_hash,
                is_sender: true,
                segment_index,
                total_segments,
                ..
            } if segment_index == total_segments => {
                self.on_resource_sent(
                    *link_id,
                    *resource_hash,
                    ResourceSentInfo {
                        link_id: *link_id,
                        segment_index: *segment_index,
                        total_segments: *total_segments,
                    },
                );
            }
            NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error,
                is_sender: true,
                ..
            } => self.on_resource_failed(*link_id, *resource_hash, *error),
            NodeEvent::ResponseReceived {
                request_id,
                response_data,
                metadata,
                ..
            } => self.on_response_received(*request_id, response_data, metadata),
            NodeEvent::RequestTimedOut { request_id, .. } => self.on_request_timed_out(*request_id),
            _ => {}
        }
    }

    fn on_link_established(&self, link_id: LinkId) {
        let resolved = {
            let mut inner = self.inner.lock_recover();
            let ws = inner.take_waiters(CompletionKey::LinkEstablished { link_id });
            if inner.established.insert(link_id) {
                inner.established_order.push_back(link_id);
                if inner.established.len() > ESTABLISHED_MIRROR_CAP {
                    if let Some(evicted) = inner.established_order.pop_front() {
                        inner.established.remove(&evicted);
                        tracing::warn!(
                            "completion mirror evicted live link {:?} at cap {} — an \
                             await_link_established for it now resolves only at LinkClosed \
                             or node stop",
                            evicted,
                            ESTABLISHED_MIRROR_CAP,
                        );
                    }
                }
            }
            ws
        };
        for w in resolved {
            w.tx.resolve_link();
        }
    }

    fn on_link_closed(&self, link_id: LinkId, reason: LinkCloseReason) {
        let swept = {
            let mut inner = self.inner.lock_recover();
            if inner.established.remove(&link_id) {
                inner.established_order.retain(|l| *l != link_id);
            }
            inner.push_recent(
                CompletionKey::LinkEstablished { link_id },
                RecentOutcome::LinkClosed { reason },
            );
            // A dead link settles its send accounting with it — outstanding
            // sends will never produce terminal events now.
            inner.pending_sends.remove(&link_id);
            // One sweep covers the establishment waiters too: every waiter
            // carries the link its outcome depends on, and a dead link is
            // terminal for all of them (the never-hang rule).
            inner.sweep_link(link_id, false)
        };
        for w in swept {
            w.tx.fail(CompletionError::LinkClosed { reason });
        }
    }

    /// Record that an outgoing resource send committed on `link_id`. Every
    /// resource-send path in the driver calls this after a successful core
    /// commit and BEFORE dispatching the commit's output, so the count is
    /// never behind the events. See `pending_sends` for why the sweep
    /// depends on it.
    pub(crate) fn note_send_began(&self, link_id: LinkId) {
        let mut inner = self.inner.lock_recover();
        if inner.closed {
            return;
        }
        *inner.pending_sends.entry(link_id).or_insert(0) += 1;
    }

    /// Saturating-decrement of `pending_sends` for one terminal event;
    /// returns whether the event was the sole outstanding send (sweep OK).
    fn settle_pending_send(inner: &mut RegistryInner, link_id: LinkId) -> bool {
        match inner.pending_sends.get_mut(&link_id) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                inner.pending_sends.remove(&link_id);
                true
            }
            // No note (a send path that predates the counter, or a
            // receiver-side event routed here by mistake): sweeping is the
            // pre-counter behavior — keep it rather than strand waiters.
            None => true,
        }
    }

    fn on_resource_sent(&self, link_id: LinkId, resource_hash: [u8; 32], info: ResourceSentInfo) {
        let resolved = {
            let mut inner = self.inner.lock_recover();
            let sweep_ok = Self::settle_pending_send(&mut inner, link_id);
            let mut ws = inner.take_waiters(CompletionKey::ResourceSent { resource_hash });
            // Split transfers: each segment carries its OWN hash, and the
            // sender's single ResourceCompleted carries the FINAL segment's —
            // not the segment-1 hash `send_resource` returned to the caller.
            // The link-scoped fallback is sound only when this event is the
            // link's SOLE outstanding send: a stale event (transfer finished
            // in core, event not yet dispatched, next send already
            // committed) must defer to the newer send's own terminal event —
            // that is what `sweep_ok` guards (see `pending_sends`).
            if sweep_ok {
                ws.extend(inner.sweep_link(link_id, true));
            }
            inner.push_recent(
                CompletionKey::ResourceSent { resource_hash },
                RecentOutcome::ResourceSent(info),
            );
            ws
        };
        for w in resolved {
            w.tx.resolve_resource(info);
        }
    }

    fn on_resource_failed(&self, link_id: LinkId, resource_hash: [u8; 32], error: ResourceError) {
        let resolved = {
            let mut inner = self.inner.lock_recover();
            let sweep_ok = Self::settle_pending_send(&mut inner, link_id);
            let mut ws = inner.take_waiters(CompletionKey::ResourceSent { resource_hash });
            // Same link-scoped fallback and same staleness guard as
            // completion: a mid-split failure carries the in-flight
            // segment's hash, not the caller's.
            if sweep_ok {
                ws.extend(inner.sweep_link(link_id, true));
            }
            inner.push_recent(
                CompletionKey::ResourceSent { resource_hash },
                RecentOutcome::ResourceFailed { error },
            );
            ws
        };
        for w in resolved {
            w.tx.fail(CompletionError::Resource { error });
        }
    }

    fn on_response_received(
        &self,
        request_id: [u8; 16],
        response_data: &[u8],
        metadata: &Option<Vec<u8>>,
    ) {
        let resolved = {
            let mut inner = self.inner.lock_recover();
            let ws = inner.take_waiters(CompletionKey::RequestResponse { request_id });
            inner.push_recent(
                CompletionKey::RequestResponse { request_id },
                RecentOutcome::ResponseDelivered,
            );
            ws
        };
        // The payload is cloned ONLY when a waiter exists; the ring above
        // stores a marker either way.
        for w in resolved {
            w.tx.resolve_request(ResponseInfo {
                response_data: response_data.to_vec(),
                metadata: metadata.clone(),
            });
        }
    }

    fn on_request_timed_out(&self, request_id: [u8; 16]) {
        let resolved = {
            let mut inner = self.inner.lock_recover();
            let ws = inner.take_waiters(CompletionKey::RequestResponse { request_id });
            inner.push_recent(
                CompletionKey::RequestResponse { request_id },
                RecentOutcome::RequestTimedOut,
            );
            ws
        };
        for w in resolved {
            w.tx.fail(CompletionError::RequestTimedOut);
        }
    }

    /// Register a link-establishment waiter. Checks the established mirror
    /// and the recent ring FIRST (under the one mutex), so registering after
    /// the fact resolves immediately instead of racing the event.
    pub(crate) fn register_link_established(
        self: &Arc<Self>,
        link_id: LinkId,
    ) -> LinkEstablishedFuture {
        let key = CompletionKey::LinkEstablished { link_id };
        let (tx, rx) = oneshot::channel();
        let token = {
            let mut inner = self.inner.lock_recover();
            if inner.closed {
                let _ = tx.send(Err(CompletionError::NodeStopped));
                0
            } else if inner.established.contains(&link_id) {
                let _ = tx.send(Ok(()));
                0
            } else if let Some(reason) = inner.recent_link_closed(link_id) {
                let _ = tx.send(Err(CompletionError::LinkClosed { reason }));
                0
            } else {
                inner.insert_waiter(key, link_id, WaiterTx::Link(tx))
            }
        };
        self.completion(key, token, rx)
    }

    /// Register a sender-side resource-completion waiter. Ring first (the
    /// transfer may already have terminated), then the link's own terminal
    /// state — this is what closes the commit→register microgap of
    /// `send_resource_awaited`.
    pub(crate) fn register_resource_sent(
        self: &Arc<Self>,
        resource_hash: [u8; 32],
        link_id: LinkId,
    ) -> ResourceSentFuture {
        let key = CompletionKey::ResourceSent { resource_hash };
        let (tx, rx) = oneshot::channel();
        let token = {
            let mut inner = self.inner.lock_recover();
            if inner.closed {
                let _ = tx.send(Err(CompletionError::NodeStopped));
                0
            } else {
                match inner.newest_recent(key) {
                    Some(RecentOutcome::ResourceSent(info)) => {
                        let _ = tx.send(Ok(*info));
                        0
                    }
                    Some(RecentOutcome::ResourceFailed { error }) => {
                        let _ = tx.send(Err(CompletionError::Resource { error: *error }));
                        0
                    }
                    _ => {
                        if let Some(reason) = inner.recent_link_closed(link_id) {
                            let _ = tx.send(Err(CompletionError::LinkClosed { reason }));
                            0
                        } else {
                            inner.insert_waiter(key, link_id, WaiterTx::Resource(tx))
                        }
                    }
                }
            }
        };
        self.completion(key, token, rx)
    }

    /// Register a request-response waiter. A response that already arrived
    /// resolves `Err(AlreadyCompleted)` — the ring keeps markers, never
    /// payloads.
    pub(crate) fn register_request_response(
        self: &Arc<Self>,
        request_id: [u8; 16],
        link_id: LinkId,
    ) -> RequestResponseFuture {
        let key = CompletionKey::RequestResponse { request_id };
        let (tx, rx) = oneshot::channel();
        let token = {
            let mut inner = self.inner.lock_recover();
            if inner.closed {
                let _ = tx.send(Err(CompletionError::NodeStopped));
                0
            } else {
                match inner.newest_recent(key) {
                    Some(RecentOutcome::ResponseDelivered) => {
                        let _ = tx.send(Err(CompletionError::AlreadyCompleted));
                        0
                    }
                    Some(RecentOutcome::RequestTimedOut) => {
                        let _ = tx.send(Err(CompletionError::RequestTimedOut));
                        0
                    }
                    _ => {
                        if let Some(reason) = inner.recent_link_closed(link_id) {
                            let _ = tx.send(Err(CompletionError::LinkClosed { reason }));
                            0
                        } else {
                            inner.insert_waiter(key, link_id, WaiterTx::Request(tx))
                        }
                    }
                }
            }
        };
        self.completion(key, token, rx)
    }

    fn completion<T>(
        self: &Arc<Self>,
        key: CompletionKey,
        token: u64,
        rx: oneshot::Receiver<Result<T, CompletionError>>,
    ) -> Completion<T> {
        Completion {
            rx,
            registry: Arc::clone(self),
            key,
            token,
            finished: false,
        }
    }

    /// Drop path of a [`Completion`]. Unknown `(key, token)` — already
    /// resolved, or resolved at registration — is a no-op.
    fn unregister(&self, key: CompletionKey, token: u64) {
        let mut inner = self.inner.lock_recover();
        if let Some(ws) = inner.waiters.get_mut(&key) {
            ws.retain(|w| w.token != token);
            if ws.is_empty() {
                inner.waiters.remove(&key);
            }
        }
    }

    /// Subscribe a secondary bounded event observer.
    ///
    /// Lazily creates the broadcast fan-out (the first subscriber pays it;
    /// nodes that never subscribe never allocate it). On a closed registry
    /// the returned tap yields `None` immediately.
    pub(crate) fn subscribe(&self) -> EventTap {
        let mut inner = self.inner.lock_recover();
        if inner.closed {
            let (tx, rx) = broadcast::channel(DEFAULT_EVENT_TAP_CAPACITY);
            drop(tx);
            return EventTap { rx, lost: 0 };
        }
        let sender = inner
            .tap
            .get_or_insert_with(|| broadcast::channel(DEFAULT_EVENT_TAP_CAPACITY).0);
        let rx = sender.subscribe();
        // Armed under the same lock the broadcast path takes, so an event
        // observed after this subscribe cannot miss the new receiver.
        self.tap_active.store(true, Ordering::Relaxed);
        EventTap { rx, lost: 0 }
    }

    /// Slow half of the tap: only entered while `tap_active` reads true.
    fn broadcast_event(&self, event: &NodeEvent) {
        let inner = self.inner.lock_recover();
        match &inner.tap {
            Some(tx) if tx.receiver_count() > 0 => {
                // Drop-oldest on overrun is the broadcast channel's own
                // semantics; the receiver surfaces it as `Lagged(n)`.
                let _ = tx.send(event.clone());
            }
            // Every tap was dropped: disarm the fast path. The sender is
            // kept so a later subscribe reuses it.
            _ => self.tap_active.store(false, Ordering::Relaxed),
        }
    }

    /// Event-loop exit: resolve every pending waiter with `NodeStopped` and
    /// mark the registry closed, so registrations against a stopped node
    /// resolve immediately instead of parking forever. The tap sender drops
    /// with the guard, so every `EventTap::recv` returns `None` once drained.
    pub(crate) fn close(&self) {
        let pending: Vec<Waiter> = {
            let mut inner = self.inner.lock_recover();
            inner.closed = true;
            inner.tap = None;
            inner.pending_sends.clear();
            self.tap_active.store(false, Ordering::Relaxed);
            inner.waiters.drain().flat_map(|(_, ws)| ws).collect()
        };
        for w in pending {
            w.tx.fail(CompletionError::NodeStopped);
        }
    }

    /// Restart support: `stop()` closes the registry when the event loop
    /// exits; a subsequent `start()` reopens it so new registrations park
    /// again. The mirrors are kept — the core (and its link state) survives
    /// a stop/start cycle, so they still describe it.
    pub(crate) fn reopen(&self) {
        self.inner.lock_recover().closed = false;
    }

    #[cfg(test)]
    fn tap_armed(&self) -> bool {
        self.tap_active.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.inner
            .lock_recover()
            .waiters
            .values()
            .map(Vec::len)
            .sum()
    }

    #[cfg(test)]
    fn recent_len(&self) -> usize {
        self.inner.lock_recover().recent.len()
    }

    #[cfg(test)]
    fn established_len(&self) -> usize {
        self.inner.lock_recover().established.len()
    }
}

/// One item from an [`EventTap`].
//
// `large_enum_variant`: the event IS the payload, and consumers match it
// apart immediately; boxing would add a per-event heap allocation only to
// appease the size lint on the rare `Lagged` arm.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum TapEvent {
    /// An observed node event (a clone; the primary stream got the original).
    Event(NodeEvent),
    /// The tap fell behind; the ring overwrote `n` oldest events
    /// (drop-oldest).
    Lagged(u64),
}

/// Secondary bounded event observer (leviculum#42 surface b).
///
/// Never consumes from or races the primary [`EventReceiver`]: it is fed
/// clones at the dispatch layer, BEFORE the two-plane sink, so it also sees
/// events the data plane is entitled to drop, and it works on a daemon-mode
/// node built `without_events()`. Backpressure is drop-oldest: a slow tap
/// loses the oldest events and is told so via [`TapEvent::Lagged`], while the
/// node never blocks on it.
///
/// [`EventReceiver`]: super::EventReceiver
pub struct EventTap {
    rx: broadcast::Receiver<NodeEvent>,
    lost: u64,
}

impl EventTap {
    /// Next observed event, or `None` once the node has stopped (channel
    /// closed and drained).
    pub async fn recv(&mut self) -> Option<TapEvent> {
        match self.rx.recv().await {
            Ok(event) => Some(TapEvent::Event(event)),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                self.lost += n;
                Some(TapEvent::Lagged(n))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    /// Non-blocking [`recv`](Self::recv): `None` when nothing is buffered
    /// (or the node has stopped).
    pub fn try_recv(&mut self) -> Option<TapEvent> {
        match self.rx.try_recv() {
            Ok(event) => Some(TapEvent::Event(event)),
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                self.lost += n;
                Some(TapEvent::Lagged(n))
            }
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => None,
        }
    }

    /// Cumulative events lost to lag on this tap.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Consumer-side filter sugar: `recv()` skips events failing `f`.
    ///
    /// The filter cannot un-occupy ring slots — a filtered-out event still
    /// counted against the tap's capacity, and lag is reported for all
    /// events, not only matching ones.
    pub fn filtered(self, f: impl Fn(&NodeEvent) -> bool + Send + 'static) -> FilteredEventTap {
        FilteredEventTap {
            tap: self,
            filter: Box::new(f),
        }
    }
}

/// An [`EventTap`] with a consumer-side event filter. See
/// [`EventTap::filtered`].
pub struct FilteredEventTap {
    tap: EventTap,
    filter: Box<dyn Fn(&NodeEvent) -> bool + Send>,
}

impl FilteredEventTap {
    /// Next event passing the filter, a [`TapEvent::Lagged`] notice, or
    /// `None` once the node has stopped.
    pub async fn recv(&mut self) -> Option<TapEvent> {
        loop {
            match self.tap.recv().await? {
                TapEvent::Event(event) => {
                    if (self.filter)(&event) {
                        return Some(TapEvent::Event(event));
                    }
                }
                lagged @ TapEvent::Lagged(_) => return Some(lagged),
            }
        }
    }

    /// Cumulative events lost to lag on the underlying tap.
    pub fn lost(&self) -> u64 {
        self.tap.lost()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_core::DestinationHash;

    fn link(id: u8) -> LinkId {
        LinkId::new([id; 16])
    }

    fn established(link_id: LinkId) -> NodeEvent {
        NodeEvent::LinkEstablished {
            link_id,
            is_initiator: true,
            destination_hash: DestinationHash::new([0xD5; 16]),
        }
    }

    fn closed(link_id: LinkId, reason: LinkCloseReason) -> NodeEvent {
        NodeEvent::LinkClosed {
            link_id,
            reason,
            is_initiator: true,
            destination_hash: DestinationHash::new([0xD5; 16]),
        }
    }

    fn resource_completed(link_id: LinkId, hash: [u8; 32], seg: u32, total: u32) -> NodeEvent {
        NodeEvent::ResourceCompleted {
            link_id,
            resource_hash: hash,
            data: Vec::new(),
            metadata: None,
            is_sender: true,
            segment_index: seg,
            total_segments: total,
        }
    }

    /// Poll a completion exactly once with a no-op waker.
    fn poll_now<T>(fut: &mut Completion<T>) -> Poll<Result<T, CompletionError>> {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        Pin::new(fut).poll(&mut cx)
    }

    #[test]
    fn register_then_observe_established_resolves_ok() {
        let reg = CompletionRegistry::new();
        let mut fut = reg.register_link_established(link(1));
        assert!(poll_now(&mut fut).is_pending());
        reg.observe(&established(link(1)));
        assert_eq!(poll_now(&mut fut), Poll::Ready(Ok(())));
    }

    #[test]
    fn observe_established_then_register_resolves_immediately_via_mirror() {
        let reg = CompletionRegistry::new();
        reg.observe(&established(link(2)));
        let mut fut = reg.register_link_established(link(2));
        assert_eq!(poll_now(&mut fut), Poll::Ready(Ok(())));
        assert_eq!(reg.waiter_count(), 0, "immediate resolve must not park");
    }

    #[test]
    fn observe_closed_then_register_resolves_link_closed_error() {
        let reg = CompletionRegistry::new();
        reg.observe(&established(link(3)));
        reg.observe(&closed(link(3), LinkCloseReason::Timeout));
        let mut fut = reg.register_link_established(link(3));
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Err(CompletionError::LinkClosed {
                reason: LinkCloseReason::Timeout
            }))
        );
    }

    #[test]
    fn dropped_future_unregisters_waiter() {
        let reg = CompletionRegistry::new();
        let mut doomed = reg.register_link_established(link(4));
        let mut sibling = reg.register_link_established(link(4));
        assert!(poll_now(&mut doomed).is_pending());
        assert_eq!(reg.waiter_count(), 2);

        drop(doomed);
        assert_eq!(reg.waiter_count(), 1, "Drop must unregister mid-wait");

        // The event still resolves the sibling on the same key; nothing
        // panics over the vanished waiter.
        reg.observe(&established(link(4)));
        assert_eq!(poll_now(&mut sibling), Poll::Ready(Ok(())));
        assert_eq!(reg.waiter_count(), 0);
    }

    #[test]
    fn resource_completed_final_segment_resolves_info() {
        let reg = CompletionRegistry::new();
        let mut fut = reg.register_resource_sent([0xAB; 32], link(5));
        reg.observe(&resource_completed(link(5), [0xAB; 32], 3, 3));
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Ok(ResourceSentInfo {
                link_id: link(5),
                segment_index: 3,
                total_segments: 3,
            }))
        );
    }

    #[test]
    fn intermediate_segment_does_not_resolve() {
        let reg = CompletionRegistry::new();
        let mut fut = reg.register_resource_sent([0xAB; 32], link(5));
        reg.observe(&resource_completed(link(5), [0xAB; 32], 1, 3));
        assert!(
            poll_now(&mut fut).is_pending(),
            "a non-final segment is not the transfer's completion"
        );
    }

    /// Split transfers advertise each segment under its own hash; the final
    /// sender-side ResourceCompleted carries the LAST segment's, while the
    /// caller holds segment 1's. The link-scoped fallback (one outgoing
    /// transfer per link) is what resolves the caller's waiter.
    #[test]
    fn split_transfer_resolves_waiter_registered_under_segment_one_hash() {
        let reg = CompletionRegistry::new();
        let mut fut = reg.register_resource_sent([0x01; 32], link(6));
        reg.observe(&resource_completed(link(6), [0x99; 32], 4, 4));
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Ok(ResourceSentInfo {
                link_id: link(6),
                segment_index: 4,
                total_segments: 4,
            }))
        );
    }

    /// The reviewer-found race (leviculum#42 integration): a transfer can
    /// complete in core — freeing the link's `TransferInProgress` slot for a
    /// new commit — before its terminal event dispatches. The stale event
    /// must not sweep-resolve a waiter registered for the newer send; the
    /// pending-sends count identifies it and defers to the newer send's own
    /// terminal event.
    #[test]
    fn stale_terminal_event_defers_sweep_to_the_newer_sends_event() {
        let reg = CompletionRegistry::new();
        // Transfer A: plain send, no waiter, committed and completed in core.
        reg.note_send_began(link(9));
        // Transfer B commits in the window before A's event dispatches.
        reg.note_send_began(link(9));
        let mut fut = reg.register_resource_sent([0xB1; 32], link(9));
        // A's event arrives late, carrying A's final-segment hash.
        reg.observe(&resource_completed(link(9), [0xAA; 32], 2, 2));
        assert_eq!(
            poll_now(&mut fut),
            Poll::Pending,
            "a stale event must not resolve the newer send's waiter"
        );
        // B's own final event resolves it.
        reg.observe(&resource_completed(link(9), [0xBF; 32], 3, 3));
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Ok(ResourceSentInfo {
                link_id: link(9),
                segment_index: 3,
                total_segments: 3,
            }))
        );
    }

    /// An exact hash match is the transfer's identity — it resolves even
    /// while older sends are outstanding on the link.
    #[test]
    fn stale_event_does_not_suppress_exact_hash_resolution() {
        let reg = CompletionRegistry::new();
        reg.note_send_began(link(11));
        reg.note_send_began(link(11));
        let mut fut = reg.register_resource_sent([0xE1; 32], link(11));
        reg.observe(&resource_completed(link(11), [0xE1; 32], 1, 1));
        assert!(matches!(poll_now(&mut fut), Poll::Ready(Ok(_))));
    }

    /// LinkClosed settles the link's send accounting — a LinkId never
    /// returns (link ids are per-instance), so a lingering entry would be a
    /// pure leak.
    #[test]
    fn link_close_drops_send_accounting() {
        let reg = CompletionRegistry::new();
        reg.note_send_began(link(12));
        reg.note_send_began(link(12));
        assert!(reg
            .inner
            .lock_recover()
            .pending_sends
            .contains_key(&link(12)));
        reg.observe(&closed(link(12), LinkCloseReason::Timeout));
        assert!(
            !reg.inner
                .lock_recover()
                .pending_sends
                .contains_key(&link(12)),
            "a dead link's send accounting must not outlive it"
        );
    }

    #[test]
    fn resource_failed_resolves_typed_error() {
        let reg = CompletionRegistry::new();
        let mut fut = reg.register_resource_sent([0xCC; 32], link(7));
        reg.observe(&NodeEvent::ResourceFailed {
            link_id: link(7),
            resource_hash: [0xCC; 32],
            error: ResourceError::MaxRetriesExceeded,
            is_sender: true,
        });
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Err(CompletionError::Resource {
                error: ResourceError::MaxRetriesExceeded
            }))
        );
    }

    #[test]
    fn receiver_side_resource_events_do_not_resolve_sender_waiters() {
        let reg = CompletionRegistry::new();
        let mut fut = reg.register_resource_sent([0xCC; 32], link(7));
        reg.observe(&NodeEvent::ResourceCompleted {
            link_id: link(7),
            resource_hash: [0xCC; 32],
            data: vec![1, 2, 3],
            metadata: None,
            is_sender: false,
            segment_index: 1,
            total_segments: 1,
        });
        assert!(
            poll_now(&mut fut).is_pending(),
            "an inbound transfer completing is not this node's send"
        );
    }

    #[test]
    fn response_received_resolves_cloned_payload() {
        let reg = CompletionRegistry::new();
        let mut fut = reg.register_request_response([0x11; 16], link(8));
        reg.observe(&NodeEvent::ResponseReceived {
            link_id: link(8),
            request_id: [0x11; 16],
            response_data: vec![0xDE, 0xAD],
            metadata: Some(vec![0xBE]),
        });
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Ok(ResponseInfo {
                response_data: vec![0xDE, 0xAD],
                metadata: Some(vec![0xBE]),
            }))
        );
    }

    #[test]
    fn request_timed_out_resolves_error() {
        let reg = CompletionRegistry::new();
        let mut fut = reg.register_request_response([0x22; 16], link(9));
        reg.observe(&NodeEvent::RequestTimedOut {
            link_id: link(9),
            request_id: [0x22; 16],
        });
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Err(CompletionError::RequestTimedOut))
        );
    }

    /// C5: a dead link resolves EVERY waiter kind keyed to it — none may
    /// hang on an object that no longer exists.
    #[test]
    fn link_closed_sweeps_pending_resource_and_request_waiters() {
        let reg = CompletionRegistry::new();
        let mut est = reg.register_link_established(link(10));
        let mut res = reg.register_resource_sent([0x33; 32], link(10));
        let mut req = reg.register_request_response([0x44; 16], link(10));
        let mut other = reg.register_link_established(link(11));

        reg.observe(&closed(link(10), LinkCloseReason::PeerClosed));

        let expected = CompletionError::LinkClosed {
            reason: LinkCloseReason::PeerClosed,
        };
        assert_eq!(poll_now(&mut est), Poll::Ready(Err(expected.clone())));
        assert_eq!(poll_now(&mut res), Poll::Ready(Err(expected.clone())));
        assert_eq!(poll_now(&mut req), Poll::Ready(Err(expected)));
        assert!(
            poll_now(&mut other).is_pending(),
            "a different link's waiters must survive the sweep"
        );
    }

    #[test]
    fn late_response_waiter_gets_already_completed() {
        let reg = CompletionRegistry::new();
        reg.observe(&NodeEvent::ResponseReceived {
            link_id: link(12),
            request_id: [0x55; 16],
            response_data: vec![0x01],
            metadata: None,
        });
        let mut fut = reg.register_request_response([0x55; 16], link(12));
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Err(CompletionError::AlreadyCompleted))
        );
    }

    #[test]
    fn recent_ring_evicts_oldest_at_cap() {
        let reg = CompletionRegistry::new();
        // Overfill the ring with distinct link-closed markers.
        for i in 0..(RECENT_OUTCOMES_CAP + 8) {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(i as u64).to_le_bytes());
            reg.observe(&closed(LinkId::new(id), LinkCloseReason::Timeout));
        }
        assert_eq!(reg.recent_len(), RECENT_OUTCOMES_CAP);

        // The oldest marker is gone: a late registration for it parks
        // instead of resolving (the documented degraded mode).
        let mut oldest = [0u8; 16];
        oldest[..8].copy_from_slice(&0u64.to_le_bytes());
        let mut fut = reg.register_link_established(LinkId::new(oldest));
        assert!(poll_now(&mut fut).is_pending());

        // The newest survives and still resolves immediately.
        let mut newest = [0u8; 16];
        newest[..8].copy_from_slice(&((RECENT_OUTCOMES_CAP + 7) as u64).to_le_bytes());
        let mut fut = reg.register_link_established(LinkId::new(newest));
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Err(CompletionError::LinkClosed {
                reason: LinkCloseReason::Timeout
            }))
        );
    }

    /// Mirror overflow evicts FIFO; a waiter for the evicted (still-live)
    /// link degrades to resolving at close() / LinkClosed — never to a hang.
    #[test]
    fn established_mirror_evicts_at_cap_and_waiter_still_resolves_on_close() {
        let reg = CompletionRegistry::new();
        for i in 0..(ESTABLISHED_MIRROR_CAP + 1) {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(i as u64).to_le_bytes());
            reg.observe(&established(LinkId::new(id)));
        }
        assert_eq!(reg.established_len(), ESTABLISHED_MIRROR_CAP);

        // Link 0 was evicted: a late await parks (immediate-resolve lost)...
        let mut evicted = [0u8; 16];
        evicted[..8].copy_from_slice(&0u64.to_le_bytes());
        let mut fut = reg.register_link_established(LinkId::new(evicted));
        assert!(poll_now(&mut fut).is_pending());

        // ...but close() still resolves it with the typed stop error.
        reg.close();
        assert_eq!(
            poll_now(&mut fut),
            Poll::Ready(Err(CompletionError::NodeStopped))
        );
    }

    #[test]
    fn close_resolves_all_pending_with_node_stopped_and_rejects_new_registrations() {
        let reg = CompletionRegistry::new();
        let mut est = reg.register_link_established(link(13));
        let mut res = reg.register_resource_sent([0x66; 32], link(13));
        reg.close();

        assert_eq!(
            poll_now(&mut est),
            Poll::Ready(Err(CompletionError::NodeStopped))
        );
        assert_eq!(
            poll_now(&mut res),
            Poll::Ready(Err(CompletionError::NodeStopped))
        );

        let mut late = reg.register_request_response([0x77; 16], link(13));
        assert_eq!(
            poll_now(&mut late),
            Poll::Ready(Err(CompletionError::NodeStopped))
        );
        assert_eq!(reg.waiter_count(), 0);
    }

    #[test]
    fn reopen_after_close_parks_new_waiters_again() {
        let reg = CompletionRegistry::new();
        reg.close();
        reg.reopen();
        let mut fut = reg.register_link_established(link(14));
        assert!(poll_now(&mut fut).is_pending());
        reg.observe(&established(link(14)));
        assert_eq!(poll_now(&mut fut), Poll::Ready(Ok(())));
    }

    #[test]
    fn two_waiters_same_key_both_resolve() {
        let reg = CompletionRegistry::new();
        let mut a = reg.register_link_established(link(15));
        let mut b = reg.register_link_established(link(15));
        reg.observe(&established(link(15)));
        assert_eq!(poll_now(&mut a), Poll::Ready(Ok(())));
        assert_eq!(poll_now(&mut b), Poll::Ready(Ok(())));
    }

    /// Cancel-safety under a real select!: the losing branch's completion is
    /// dropped mid-wait, and the event arriving afterwards must neither panic
    /// nor disturb a sibling waiter.
    #[tokio::test]
    async fn select_dropped_completion_leaves_registry_clean() {
        let reg = CompletionRegistry::new();
        let mut sibling = reg.register_link_established(link(16));
        {
            let fut = reg.register_link_established(link(16));
            tokio::select! {
                biased;
                _ = std::future::ready(()) => {}
                _ = fut => panic!("nothing resolved this yet"),
            }
            // `fut` was polled once, lost the race, and is dropped here.
        }
        assert_eq!(reg.waiter_count(), 1, "the select! loser must unregister");

        reg.observe(&established(link(16)));
        assert_eq!(poll_now(&mut sibling), Poll::Ready(Ok(())));
    }

    // ---- surface (b): the event tap -----------------------------------

    /// Overrun drops OLDEST and says so: with `capacity + K` undrained
    /// events, the first recv is `Lagged(K)`, then the survivors follow in
    /// order, and `lost()` totals `K`.
    #[tokio::test]
    async fn tap_lag_reports_dropped_oldest_count() {
        let reg = CompletionRegistry::new();
        let mut tap = reg.subscribe();

        const K: usize = 16;
        for i in 0..(DEFAULT_EVENT_TAP_CAPACITY + K) {
            // A variant the completion match ignores: the tap must carry
            // every event, not only completion-relevant ones.
            reg.observe(&NodeEvent::InterfaceDown(i));
        }

        match tap.recv().await {
            Some(TapEvent::Lagged(n)) => assert_eq!(n, K as u64),
            other => panic!("expected Lagged({K}), got {other:?}"),
        }
        assert_eq!(tap.lost(), K as u64);

        // The oldest survivor is exactly the K-th event.
        match tap.recv().await {
            Some(TapEvent::Event(NodeEvent::InterfaceDown(i))) => assert_eq!(i, K),
            other => panic!("expected InterfaceDown({K}), got {other:?}"),
        }
    }

    /// Dropping the last tap disarms the fast path: the next observe clears
    /// `tap_active`, and later observes skip the broadcast branch entirely
    /// (the zero-subscriber cost is one atomic load).
    #[test]
    fn zero_subscriber_observe_takes_no_lock_path() {
        let reg = CompletionRegistry::new();
        assert!(!reg.tap_armed(), "no subscriber yet: fast path disarmed");

        let tap = reg.subscribe();
        assert!(reg.tap_armed());

        drop(tap);
        // Still armed until an observe notices the receiver count hit zero...
        reg.observe(&NodeEvent::InterfaceDown(1));
        assert!(
            !reg.tap_armed(),
            "first observe after the last tap drops must disarm the fast path"
        );

        // ...and a re-subscribe re-arms it (the sender was kept).
        let _tap = reg.subscribe();
        assert!(reg.tap_armed());
    }

    /// close() ends the tap stream: recv drains anything buffered, then
    /// returns None instead of parking forever on a stopped node.
    #[tokio::test]
    async fn tap_closes_on_registry_close() {
        let reg = CompletionRegistry::new();
        let mut tap = reg.subscribe();
        reg.observe(&NodeEvent::InterfaceDown(7));
        reg.close();

        match tap.recv().await {
            Some(TapEvent::Event(NodeEvent::InterfaceDown(7))) => {}
            other => panic!("buffered event must still drain, got {other:?}"),
        }
        assert!(tap.recv().await.is_none(), "closed tap must end the stream");

        // A subscribe on the closed registry ends immediately too.
        let mut late = reg.subscribe();
        assert!(late.recv().await.is_none());
    }

    /// The filter is consumer-side sugar: non-matching events are skipped
    /// inside recv(), lag notices pass through.
    #[tokio::test]
    async fn filtered_tap_skips_non_matching_events() {
        let reg = CompletionRegistry::new();
        let mut tap = reg
            .subscribe()
            .filtered(|ev| matches!(ev, NodeEvent::InterfaceDown(9)));

        reg.observe(&NodeEvent::InterfaceDown(1));
        reg.observe(&NodeEvent::InterfaceDown(9));
        reg.close();

        match tap.recv().await {
            Some(TapEvent::Event(NodeEvent::InterfaceDown(9))) => {}
            other => panic!("filter must skip to the matching event, got {other:?}"),
        }
        assert!(tap.recv().await.is_none());
    }
}
