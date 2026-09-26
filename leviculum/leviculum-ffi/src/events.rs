//! The event bridge: a pollable eventfd over the engine's event stream.
//!
//! A bridge task drains the engine `EventReceiver`, projects each `NodeEvent`
//! to a self-owned [`lev_event_t`], and enqueues it. An eventfd in semaphore
//! mode mirrors the queue length so a C app can `poll`/`epoll` the fd and drain
//! with [`lev_next_event`]. The eventfd syscalls are done under the FIFO mutex
//! so the counter-equals-length invariant holds per instant. See
//! `docs/leviculum-api-design.md` §4.

use std::collections::VecDeque;
use std::os::raw::{c_int, c_void};
use std::os::unix::io::RawFd;
use std::sync::Mutex;

use leviculum_std::{EventClass, NodeEvent};

use crate::error::set_last_error;
use crate::{guard, write_out, LEV_ERR_INVALID_ARG, LEV_ERR_NULL_PTR, LEV_ERR_PANIC, LEV_OK};

/// Catch-all for events not yet projected with their own type and accessors.
pub const LEV_EVENT_OTHER: c_int = 0;
/// A validated announce was received from a peer.
pub const LEV_EVENT_ANNOUNCE_RECEIVED: c_int = 1;
/// A path to a requested destination was found.
pub const LEV_EVENT_PATH_FOUND: c_int = 2;
/// Deprecated, never fired. Under the auto-accept model (Python-RNS parity)
/// inbound links are accepted and proved by the core, so there is no separate
/// request event; an inbound link surfaces as `LEV_EVENT_LINK_ESTABLISHED`
/// with `lev_event_is_sender` returning 0. The numeric value is retained for
/// ABI stability.
pub const LEV_EVENT_LINK_REQUEST: c_int = 3;
/// A link handshake completed. Fires for both directions; `lev_event_is_sender`
/// is the inbound indicator: 0 means an inbound link (this node is the
/// responder; the link is already accepted and proved, mint a handle with
/// `lev_accept_link`), 1 means a link this node initiated.
pub const LEV_EVENT_LINK_ESTABLISHED: c_int = 4;
/// A link closed. `link_id` and `dest_hash` are set.
pub const LEV_EVENT_LINK_CLOSED: c_int = 5;
/// Data arrived on a link.
pub const LEV_EVENT_LINK_DATA: c_int = 6;
/// A single packet (datagram) arrived on a destination.
pub const LEV_EVENT_PACKET_RECEIVED: c_int = 7;
/// Control events were dropped; the count is available via
/// `lev_event_dropped_count`.
pub const LEV_EVENT_CONTROL_OVERFLOW: c_int = 8;
/// A request arrived on a link (respond with `lev_send_response`). `dest_hash`
/// is the destination the request was addressed to: several destinations may
/// register the same request path, so a responder hosting more than one needs
/// it to know which endpoint to serve.
pub const LEV_EVENT_REQUEST_RECEIVED: c_int = 9;
/// A response to a sent request arrived.
pub const LEV_EVENT_RESPONSE_RECEIVED: c_int = 10;
/// A sent request timed out without a response.
pub const LEV_EVENT_REQUEST_TIMEOUT: c_int = 11;
/// An incoming resource was advertised (accept or reject it).
pub const LEV_EVENT_RESOURCE_ADVERTISED: c_int = 12;
/// A resource transfer started.
pub const LEV_EVENT_RESOURCE_STARTED: c_int = 13;
/// Resource transfer progress (`lev_event_progress`).
pub const LEV_EVENT_RESOURCE_PROGRESS: c_int = 14;
/// A resource transfer completed (receiver gets data and metadata).
pub const LEV_EVENT_RESOURCE_COMPLETED: c_int = 15;
/// A resource transfer failed.
pub const LEV_EVENT_RESOURCE_FAILED: c_int = 16;
/// The peer proved an identity on a link; the 16-byte identity hash is the
/// event payload (`lev_event_data`), and `lev_link_remote_identity` returns it.
pub const LEV_EVENT_LINK_IDENTIFIED: c_int = 17;
/// A reliable, sequenced message arrived on a link's channel (the peer used
/// the channel, as `lev_link_send` does). Distinct from `LEV_EVENT_LINK_DATA`,
/// which is a raw unsequenced link packet. Carries a message type and a
/// sequence number via `lev_event_msgtype` and `lev_event_sequence`.
pub const LEV_EVENT_LINK_MESSAGE: c_int = 18;
/// A single packet arrived at a destination with the App proof strategy; the
/// app may call `lev_send_proof`. `dest_hash` is the destination, the data
/// payload is the 32-byte packet hash, and `lev_event_interface_id` reads the
/// interface the packet arrived on.
pub const LEV_EVENT_PACKET_PROOF_REQUESTED: c_int = 19;
/// Data arrived on a link whose destination has the App proof strategy. The
/// `link_id` is set and the data payload is the 32-byte packet hash.
pub const LEV_EVENT_LINK_PROOF_REQUESTED: c_int = 20;
/// A delivery proof confirmed a packet we sent on a link (PROVE_ALL). The
/// `link_id` is set and the data payload is the 32-byte packet hash.
pub const LEV_EVENT_LINK_DELIVERY_CONFIRMED: c_int = 21;
/// A link went inactive past its keepalive deadline; `link_id` is set. The link
/// is not closed yet (see `LEV_EVENT_LINK_RECOVERED` and `LEV_EVENT_LINK_CLOSED`).
pub const LEV_EVENT_LINK_STALE: c_int = 22;
/// A stale link resumed carrying traffic; `link_id` is set.
pub const LEV_EVENT_LINK_RECOVERED: c_int = 23;
/// A known path to a destination expired; `dest_hash` is set.
pub const LEV_EVENT_PATH_LOST: c_int = 24;
/// A delivery proof confirmed a single packet we sent; the data payload is the
/// 16-byte packet hash (as returned by `lev_send_datagram`).
pub const LEV_EVENT_PACKET_DELIVERY_CONFIRMED: c_int = 25;
/// Delivery of a single packet we sent failed; the data payload is the 16-byte
/// packet hash and `lev_event_delivery_error` says why.
pub const LEV_EVENT_DELIVERY_FAILED: c_int = 26;
/// No delivery proof arrived for a packet we sent on a link before its
/// RTT-derived receipt deadline expired. The `link_id` is set and the data
/// payload is the 32-byte packet hash (the failure half of
/// `LEV_EVENT_LINK_DELIVERY_CONFIRMED`).
pub const LEV_EVENT_LINK_DELIVERY_FAILED: c_int = 27;

// --- Link close reasons, read with `lev_event_close_reason` on a
// `LEV_EVENT_LINK_CLOSED` event. The values follow the engine's
// `LinkCloseReason` declaration order so the mapping is auditable.

/// The local side or the peer closed the link deliberately. Reconnect freely.
pub const LEV_CLOSE_NORMAL: c_int = 0;
/// The link handshake did not complete in time: the peer may be unreachable or
/// the path stale. Re-resolve the path before retrying.
pub const LEV_CLOSE_TIMEOUT: c_int = 1;
/// A proof on the link did not verify. Retrying the same link gains nothing.
pub const LEV_CLOSE_INVALID_PROOF: c_int = 2;
/// The peer closed the link. Reconnect when there is something to send.
pub const LEV_CLOSE_PEER_CLOSED: c_int = 3;
/// The link went inactive past its keepalive deadline and was torn down.
/// Reconnect; a keepalive or traffic would have kept it alive.
pub const LEV_CLOSE_STALE: c_int = 4;
/// A channel message could not be delivered after the maximum retries. The link
/// was working, so a reconnect is reasonable, but the payload was lost.
pub const LEV_CLOSE_CHANNEL_EXHAUSTED: c_int = 5;
/// The peer identified as a blackholed identity and the link was torn down.
/// **Do not retry**: every attempt will be torn down the same way.
pub const LEV_CLOSE_BLACKHOLED: c_int = 6;
/// A close reason this ABI version has no constant for. The engine enum is
/// extensible; `event_projection_coverage` fails on a new variant that reaches
/// here, so this is a forward-compatibility floor and not a silent bucket.
pub const LEV_CLOSE_OTHER: c_int = 255;

// --- Single-packet delivery failures, read with `lev_event_delivery_error` on a
// `LEV_EVENT_DELIVERY_FAILED` event.

/// No proof arrived before the receipt expired. Re-send.
pub const LEV_DELIVERY_TIMEOUT: c_int = 0;
/// The link carrying the packet failed. Re-send (on a fresh link).
pub const LEV_DELIVERY_LINK_FAILED: c_int = 1;
/// A proof arrived and did not verify against the destination's identity. The
/// peer answered, so re-sending produces the same unverifiable proof: re-resolve
/// the destination's identity instead.
pub const LEV_DELIVERY_INVALID_PROOF: c_int = 2;
/// A delivery error this ABI version has no constant for; see
/// [`LEV_CLOSE_OTHER`].
pub const LEV_DELIVERY_OTHER: c_int = 255;

/// One projected event, fully self-owned (all payloads deep-copied out of the
/// `NodeEvent`), so it outlives the queue slot and is valid until
/// `lev_event_free`.
pub struct lev_event_t {
    ty: c_int,
    is_control: bool,
    link_id: Option<[u8; 16]>,
    dest_hash: Option<[u8; 16]>,
    request_id: Option<[u8; 16]>,
    resource_hash: Option<[u8; 32]>,
    path: Option<String>,
    data: Vec<u8>,
    metadata: Option<Vec<u8>>,
    progress: f64,
    dropped_count: u64,
    msgtype: u16,
    sequence: u16,
    is_sender: bool,
    /// The node-assigned interface id the event came in on, resolvable against
    /// `lev_interface_stats_id`.
    interface_id: Option<u64>,
    /// `LEV_CLOSE_*` for a link close.
    close_reason: Option<c_int>,
    /// `LEV_DELIVERY_*` for a failed single-packet delivery.
    delivery_error: Option<c_int>,
    /// Encrypted transfer size and uncompressed data size of a resource.
    transfer_size: Option<u64>,
    data_size: Option<u64>,
    /// Position of a completed resource segment within its transfer (1-based).
    segment_index: Option<u32>,
    total_segments: Option<u32>,
}

impl lev_event_t {
    fn bare(ty: c_int, is_control: bool) -> Self {
        Self {
            ty,
            is_control,
            link_id: None,
            dest_hash: None,
            request_id: None,
            resource_hash: None,
            path: None,
            data: Vec::new(),
            metadata: None,
            progress: 0.0,
            dropped_count: 0,
            msgtype: 0,
            sequence: 0,
            is_sender: false,
            interface_id: None,
            close_reason: None,
            delivery_error: None,
            transfer_size: None,
            data_size: None,
            segment_index: None,
            total_segments: None,
        }
    }
}

/// Project a `NodeEvent` to a self-owned [`lev_event_t`].
///
/// The class (control or data) is taken from `event_class` for every variant so
/// the queue's per-plane policy is correct even for variants still mapped to
/// [`LEV_EVENT_OTHER`]. Richer per-type projection lands with later phases.
fn project(ev: NodeEvent) -> lev_event_t {
    let is_control = matches!(ev.event_class(), EventClass::Control);
    match ev {
        NodeEvent::AnnounceReceived {
            announce,
            interface_index,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_ANNOUNCE_RECEIVED, is_control);
            e.dest_hash = Some(*announce.destination_hash().as_bytes());
            e.data = announce.app_data().to_vec();
            e.interface_id = Some(interface_index as u64);
            e
        }
        NodeEvent::PathFound {
            destination_hash,
            interface_index,
            ..
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_PATH_FOUND, is_control);
            e.dest_hash = Some(*destination_hash.as_bytes());
            e.interface_id = Some(interface_index as u64);
            e
        }
        NodeEvent::ControlPlaneOverflow { dropped_count } => {
            let mut e = lev_event_t::bare(LEV_EVENT_CONTROL_OVERFLOW, is_control);
            e.dropped_count = dropped_count;
            e
        }
        NodeEvent::LinkEstablished {
            link_id,
            is_initiator,
            destination_hash,
        } => {
            // Auto-accept model (Python-RNS parity): inbound links are accepted
            // and proved by the core, so there is no separate LinkRequest event.
            // This single event fires for both directions; `is_sender` is the
            // inbound indicator. `is_sender == 0` is an inbound link (this node
            // is the responder, the former LEV_EVENT_LINK_REQUEST case): mint a
            // handle with lev_accept_link and use it. `is_sender == 1` is a link
            // this node initiated.
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_ESTABLISHED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.is_sender = is_initiator;
            e.dest_hash = Some(*destination_hash.as_bytes());
            e
        }
        NodeEvent::LinkClosed {
            link_id,
            destination_hash,
            reason,
            ..
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_CLOSED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.dest_hash = Some(*destination_hash.as_bytes());
            e.close_reason = Some(close_reason_code(reason));
            e
        }
        NodeEvent::LinkIdentified {
            link_id,
            identity_hash,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_IDENTIFIED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.data = identity_hash.to_vec();
            e
        }
        NodeEvent::LinkDataReceived { link_id, data } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_DATA, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.data = data;
            e
        }
        NodeEvent::MessageReceived {
            link_id,
            msgtype,
            sequence,
            data,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_MESSAGE, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.data = data;
            e.msgtype = msgtype;
            e.sequence = sequence;
            e
        }
        NodeEvent::PacketProofRequested {
            packet_hash,
            destination_hash,
            interface_index,
            // The #376 delivery hint stops at the FFI boundary: `lev_event_t`
            // has no field for a peer, and the C API's reply
            // (`lev_send_proof`) routes over the path table anyway, which
            // carries its own `via_peer`. Adding it here is an ABI change,
            // registered as such in `PROJECTION_GAPS`
            // (tests/event_projection_coverage.rs).
            peer: _,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_PACKET_PROOF_REQUESTED, is_control);
            e.dest_hash = Some(*destination_hash.as_bytes());
            e.data = packet_hash.to_vec();
            e.interface_id = Some(interface_index as u64);
            e
        }
        NodeEvent::LinkProofRequested {
            link_id,
            packet_hash,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_PROOF_REQUESTED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.data = packet_hash.to_vec();
            e
        }
        NodeEvent::LinkDeliveryConfirmed {
            link_id,
            packet_hash,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_DELIVERY_CONFIRMED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.data = packet_hash.to_vec();
            e
        }
        NodeEvent::LinkDeliveryFailed {
            link_id,
            packet_hash,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_DELIVERY_FAILED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.data = packet_hash.to_vec();
            e
        }
        NodeEvent::LinkStale { link_id } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_STALE, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e
        }
        NodeEvent::LinkRecovered { link_id } => {
            let mut e = lev_event_t::bare(LEV_EVENT_LINK_RECOVERED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e
        }
        NodeEvent::PathLost { destination_hash } => {
            let mut e = lev_event_t::bare(LEV_EVENT_PATH_LOST, is_control);
            e.dest_hash = Some(*destination_hash.as_bytes());
            e
        }
        NodeEvent::PacketDeliveryConfirmed { packet_hash } => {
            let mut e = lev_event_t::bare(LEV_EVENT_PACKET_DELIVERY_CONFIRMED, is_control);
            e.data = packet_hash.to_vec();
            e
        }
        NodeEvent::DeliveryFailed { packet_hash, error } => {
            let mut e = lev_event_t::bare(LEV_EVENT_DELIVERY_FAILED, is_control);
            e.data = packet_hash.to_vec();
            e.delivery_error = Some(delivery_error_code(error));
            e
        }
        NodeEvent::PacketReceived {
            destination,
            data,
            interface_index,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_PACKET_RECEIVED, is_control);
            e.dest_hash = Some(*destination.as_bytes());
            e.data = data;
            e.interface_id = Some(interface_index as u64);
            e
        }
        NodeEvent::RequestReceived {
            link_id,
            destination_hash,
            request_id,
            path,
            data,
            ..
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_REQUEST_RECEIVED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.dest_hash = Some(*destination_hash.as_bytes());
            e.request_id = Some(request_id);
            e.path = Some(path);
            e.data = data;
            e
        }
        NodeEvent::ResponseReceived {
            link_id,
            request_id,
            response_data,
            metadata,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_RESPONSE_RECEIVED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.request_id = Some(request_id);
            e.data = response_data;
            // A response that arrived as a resource carries the resource's
            // metadata blob (the `{"name": ...}` a file response rides with).
            // Dropping it here left `lev_event_metadata` returning
            // LEV_ERR_INVALID_ARG for every response, though the accessor is
            // generic and `ResourceCompleted` has always projected it.
            e.metadata = metadata;
            e
        }
        NodeEvent::RequestTimedOut {
            link_id,
            request_id,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_REQUEST_TIMEOUT, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.request_id = Some(request_id);
            e
        }
        NodeEvent::ResourceAdvertised {
            link_id,
            resource_hash,
            transfer_size,
            data_size,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_RESOURCE_ADVERTISED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.resource_hash = Some(resource_hash);
            // The event exists so the app can accept or reject, and size is what
            // that decision turns on.
            e.transfer_size = Some(transfer_size);
            e.data_size = Some(data_size);
            e
        }
        NodeEvent::ResourceTransferStarted {
            link_id,
            resource_hash,
            ..
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_RESOURCE_STARTED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.resource_hash = Some(resource_hash);
            e
        }
        NodeEvent::ResourceProgress {
            link_id,
            resource_hash,
            progress,
            is_sender,
            transfer_size,
            data_size,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_RESOURCE_PROGRESS, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.resource_hash = Some(resource_hash);
            e.progress = progress as f64;
            e.is_sender = is_sender;
            // Not redundant with the advertise event: `ResourceAdvertised` is
            // emitted only under the AcceptApp strategy, so an auto-accepting
            // receiver (AcceptAll, and every response resource) never sees one
            // and this is its only route to the sizes.
            e.transfer_size = Some(transfer_size);
            e.data_size = Some(data_size);
            e
        }
        NodeEvent::ResourceCompleted {
            link_id,
            resource_hash,
            data,
            metadata,
            is_sender,
            segment_index,
            total_segments,
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_RESOURCE_COMPLETED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.resource_hash = Some(resource_hash);
            e.data = data;
            e.metadata = metadata;
            e.is_sender = is_sender;
            // A multi-segment resource fires this event once per segment with the
            // same resource_hash; without these a C receiver can neither order
            // the chunks nor tell which one is the last.
            e.segment_index = Some(segment_index);
            e.total_segments = Some(total_segments);
            e
        }
        NodeEvent::ResourceFailed {
            link_id,
            resource_hash,
            is_sender,
            ..
        } => {
            let mut e = lev_event_t::bare(LEV_EVENT_RESOURCE_FAILED, is_control);
            e.link_id = Some(*link_id.as_bytes());
            e.resource_hash = Some(resource_hash);
            e.is_sender = is_sender;
            e
        }
        // Other variants keep their class so the cap policy is right, but carry
        // no typed fields yet.
        _ => lev_event_t::bare(LEV_EVENT_OTHER, is_control),
    }
}

/// Map the engine's link close reason to its `LEV_CLOSE_*` constant.
///
/// Both this and [`delivery_error_code`] live *below* `project()` on purpose:
/// `event_projection_coverage` clamps its arm parsing at `project()`'s `_ =>`
/// catch-all, so a wildcard arm above it would truncate what the guard sees.
/// The clamp is anchored on `fn project(` there, and this ordering keeps the two
/// independent.
fn close_reason_code(reason: leviculum_std::LinkCloseReason) -> c_int {
    use leviculum_std::LinkCloseReason as R;
    match reason {
        R::Normal => LEV_CLOSE_NORMAL,
        R::Timeout => LEV_CLOSE_TIMEOUT,
        R::InvalidProof => LEV_CLOSE_INVALID_PROOF,
        R::PeerClosed => LEV_CLOSE_PEER_CLOSED,
        R::Stale => LEV_CLOSE_STALE,
        R::ChannelExhausted => LEV_CLOSE_CHANNEL_EXHAUSTED,
        R::Blackholed => LEV_CLOSE_BLACKHOLED,
        // `LinkCloseReason` is `#[non_exhaustive]`, so this arm is required to
        // compile. It is not a silent bucket: `every_close_reason_has_a_c_constant`
        // fails on a variant that has no arm above, which is what keeps a new
        // engine reason from landing here unnoticed.
        _ => LEV_CLOSE_OTHER,
    }
}

/// Map the engine's single-packet delivery error to its `LEV_DELIVERY_*`
/// constant. See [`close_reason_code`] for why this sits here.
fn delivery_error_code(error: leviculum_std::DeliveryError) -> c_int {
    use leviculum_std::DeliveryError as E;
    match error {
        E::Timeout => LEV_DELIVERY_TIMEOUT,
        E::LinkFailed => LEV_DELIVERY_LINK_FAILED,
        E::InvalidProof => LEV_DELIVERY_INVALID_PROOF,
        _ => LEV_DELIVERY_OTHER,
    }
}

/// Mutable, lock-guarded bridge state.
struct BridgeState {
    queue: VecDeque<Box<lev_event_t>>,
    control_len: usize,
    data_len: usize,
    /// Control events dropped since the last overflow marker was enqueued.
    control_dropped: u64,
}

/// The event bridge shared between the drain task (producer) and
/// `lev_next_event` (consumer).
pub(crate) struct EventBridge {
    fd: RawFd,
    state: Mutex<BridgeState>,
    control_cap: usize,
    data_cap: usize,
}

// SAFETY: `fd` is a plain integer used only via kernel-atomic eventfd syscalls,
// always under `state`'s lock; the rest is `Send`/`Sync` by composition.
unsafe impl Send for EventBridge {}
unsafe impl Sync for EventBridge {}

/// Increment the eventfd counter by 1. Called under the state lock.
///
/// Counter discipline: the eventfd is in semaphore mode and its counter mirrors
/// the number of queued events, which the per-plane caps bound to at most
/// `control_cap + data_cap + 1`. That ceiling is far below the eventfd's
/// `u64::MAX - 1` saturation point, so a write here can never see `EAGAIN`
/// (which an eventfd returns only when the add would overflow). An eventfd write
/// is all-or-nothing for the 8-byte value, so there is no short write either.
/// The one transient failure we can see is `EINTR`, which we retry so the
/// increment is never lost and `counter == queue length` holds on return.
fn fd_write(fd: RawFd) {
    let v: u64 = 1;
    loop {
        // SAFETY: writing 8 bytes of a u64 to an eventfd is the documented contract.
        let n = unsafe { libc::write(fd, &v as *const u64 as *const c_void, 8) };
        if n == 8 {
            return;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        // Unreachable under the cap discipline above. Losing the increment would
        // desync the counter from the queue length and could wedge a poller, so
        // surface it loudly in debug builds rather than swallowing it silently.
        debug_assert!(false, "eventfd write failed: {err}");
        return;
    }
}

/// Decrement the eventfd counter by 1 (semaphore mode). Called under the state
/// lock only after a successful pop, so the counter is `>= 1` and `read` returns
/// immediately. See [`fd_write`] for the counter discipline. `EINTR` is retried;
/// a spurious `EAGAIN` (counter already 0) is unreachable here but tolerated.
fn fd_read(fd: RawFd) {
    let mut v: u64 = 0;
    loop {
        // SAFETY: reading 8 bytes from an eventfd into a u64 is the documented
        // contract; the fd is non-blocking so this never blocks.
        let n = unsafe { libc::read(fd, &mut v as *mut u64 as *mut c_void, 8) };
        if n == 8 {
            return;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        // EAGAIN means the counter was already 0; we read exactly once per
        // successful pop under the lock, so the counter is >= 1 here. Tolerate
        // it without spinning rather than risk a busy loop.
        debug_assert!(
            err.raw_os_error() == Some(libc::EAGAIN),
            "eventfd read failed: {err}"
        );
        return;
    }
}

impl EventBridge {
    pub(crate) fn new(control_cap: usize, data_cap: usize) -> std::io::Result<Self> {
        // SAFETY: eventfd with these flags returns a new fd or -1.
        let fd = unsafe {
            libc::eventfd(
                0,
                libc::EFD_SEMAPHORE | libc::EFD_NONBLOCK | libc::EFD_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            state: Mutex::new(BridgeState {
                queue: VecDeque::new(),
                control_len: 0,
                data_len: 0,
                control_dropped: 0,
            }),
            control_cap,
            data_cap,
        })
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.fd
    }

    /// If control events were dropped and there is now room, enqueue one
    /// coalesced overflow marker reporting the count. Called under the lock.
    fn flush_overflow(&self, state: &mut BridgeState) {
        if state.control_dropped == 0 || state.control_len >= self.control_cap {
            return;
        }
        let mut marker = lev_event_t::bare(LEV_EVENT_CONTROL_OVERFLOW, true);
        marker.dropped_count = state.control_dropped;
        state.queue.push_back(Box::new(marker));
        state.control_len += 1;
        state.control_dropped = 0;
        fd_write(self.fd);
    }

    /// Enqueue one projected event, applying the per-plane cap at enqueue so a
    /// dropped event is never counted and never writes the fd.
    pub(crate) fn enqueue(&self, ev: Box<lev_event_t>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if ev.is_control {
            if state.control_len >= self.control_cap {
                // Lossless-by-default: record the loss, surfaced via a marker.
                state.control_dropped += 1;
            } else {
                state.queue.push_back(ev);
                state.control_len += 1;
                fd_write(self.fd);
            }
        } else if state.data_len < self.data_cap {
            state.queue.push_back(ev);
            state.data_len += 1;
            fd_write(self.fd);
        }
        // else: data region full, drop the incoming event (backpressure).
        self.flush_overflow(&mut state);
    }

    /// Pop one event, decrementing the eventfd counter under the lock. Returns
    /// `None` when the queue is empty.
    pub(crate) fn next(&self) -> Option<Box<lev_event_t>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ev = state.queue.pop_front();
        if let Some(ref e) = ev {
            // The plane class is read from `e.is_control`, the same field
            // `enqueue` incremented from, so the two counters cannot diverge.
            // saturating_sub plus a debug_assert guards against an unreachable
            // underflow wedging the count in release instead of panicking.
            if e.is_control {
                debug_assert!(state.control_len > 0, "control_len underflow");
                state.control_len = state.control_len.saturating_sub(1);
            } else {
                debug_assert!(state.data_len > 0, "data_len underflow");
                state.data_len = state.data_len.saturating_sub(1);
            }
            fd_read(self.fd);
        }
        // Room may have appeared for a pending overflow marker.
        self.flush_overflow(&mut state);
        ev
    }
}

impl Drop for EventBridge {
    fn drop(&mut self) {
        // SAFETY: `fd` is owned by this bridge and closed exactly once.
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// Drain task: project and enqueue every event until the channels close.
pub(crate) async fn run_bridge(
    mut rx: leviculum_std::EventReceiver,
    bridge: std::sync::Arc<EventBridge>,
) {
    while let Some(ev) = rx.recv().await {
        bridge.enqueue(Box::new(project(ev)));
    }
}

// --- C accessors on a drained event handle ---

/// The event's type, one of the `LEV_EVENT_*` constants. Returns
/// `LEV_EVENT_OTHER` (0) on a NULL pointer, which is indistinguishable from a
/// real `LEV_EVENT_OTHER` event; callers that may pass NULL should null-check
/// the handle first (`lev_next_event`/`lev_wait_event` already yield non-NULL
/// handles, so this only matters for hand-constructed pointers).
#[no_mangle]
pub unsafe extern "C" fn lev_event_type(ev: *const lev_event_t) -> c_int {
    guard(LEV_EVENT_OTHER, || match ev.as_ref() {
        Some(e) => e.ty,
        None => LEV_EVENT_OTHER,
    })
}

/// Write the event's link id (16 bytes) into `buf`, read(2) style.
/// `LEV_ERR_INVALID_ARG` if the event has no link id.
#[no_mangle]
pub unsafe extern "C" fn lev_event_link_id(
    ev: *const lev_event_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        match &e.link_id {
            Some(id) => write_out(id, buf, cap, out_len),
            None => LEV_ERR_INVALID_ARG,
        }
    })
}

/// Write the event's destination hash (16 bytes) into `buf`, read(2) style.
/// `LEV_ERR_INVALID_ARG` if the event has no destination hash.
#[no_mangle]
pub unsafe extern "C" fn lev_event_dest_hash(
    ev: *const lev_event_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        match &e.dest_hash {
            Some(h) => write_out(h, buf, cap, out_len),
            None => LEV_ERR_INVALID_ARG,
        }
    })
}

/// Write the event's request id (16 bytes) into `buf`, read(2) style.
/// `LEV_ERR_INVALID_ARG` if the event has no request id.
#[no_mangle]
pub unsafe extern "C" fn lev_event_request_id(
    ev: *const lev_event_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        match &e.request_id {
            Some(id) => write_out(id, buf, cap, out_len),
            None => LEV_ERR_INVALID_ARG,
        }
    })
}

/// Write the event's request path into `buf` as UTF-8 bytes (not
/// NUL-terminated), read(2) style. `LEV_ERR_INVALID_ARG` if the event has no
/// path.
#[no_mangle]
pub unsafe extern "C" fn lev_event_path(
    ev: *const lev_event_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        match &e.path {
            Some(p) => write_out(p.as_bytes(), buf, cap, out_len),
            None => LEV_ERR_INVALID_ARG,
        }
    })
}

/// Write the event's primary payload into `buf`, read(2) style. The payload may
/// be empty (sets `*out_len` to 0).
#[no_mangle]
pub unsafe extern "C" fn lev_event_data(
    ev: *const lev_event_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        write_out(&e.data, buf, cap, out_len)
    })
}

/// Write the event's resource hash (32 bytes) into `buf`, read(2) style.
/// `LEV_ERR_INVALID_ARG` if the event has no resource hash.
#[no_mangle]
pub unsafe extern "C" fn lev_event_resource_hash(
    ev: *const lev_event_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        match &e.resource_hash {
            Some(h) => write_out(h, buf, cap, out_len),
            None => LEV_ERR_INVALID_ARG,
        }
    })
}

/// Write the event's metadata (msgpack bytes) into `buf`, read(2) style.
/// `LEV_ERR_INVALID_ARG` if the event has no metadata.
#[no_mangle]
pub unsafe extern "C" fn lev_event_metadata(
    ev: *const lev_event_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        match &e.metadata {
            Some(m) => write_out(m, buf, cap, out_len),
            None => LEV_ERR_INVALID_ARG,
        }
    })
}

/// Write the transfer progress (0.0..1.0) of a `LEV_EVENT_RESOURCE_PROGRESS`
/// event into `*out`. `LEV_ERR_INVALID_ARG` for any other event type.
#[no_mangle]
pub unsafe extern "C" fn lev_event_progress(ev: *const lev_event_t, out: *mut f64) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if e.ty != LEV_EVENT_RESOURCE_PROGRESS {
            set_last_error("event has no progress");
            return LEV_ERR_INVALID_ARG;
        }
        *out = e.progress;
        LEV_OK
    })
}

/// Read the dropped-event count of a `LEV_EVENT_CONTROL_OVERFLOW` event.
/// `LEV_ERR_INVALID_ARG` for any other event type.
#[no_mangle]
pub unsafe extern "C" fn lev_event_dropped_count(ev: *const lev_event_t, out: *mut u64) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if e.ty != LEV_EVENT_CONTROL_OVERFLOW {
            set_last_error("event has no dropped count");
            return LEV_ERR_INVALID_ARG;
        }
        *out = e.dropped_count;
        LEV_OK
    })
}

/// Read the node-assigned id of the interface an event arrived on into `*out`.
///
/// Set on `LEV_EVENT_ANNOUNCE_RECEIVED`, `LEV_EVENT_PATH_FOUND` and
/// `LEV_EVENT_PACKET_RECEIVED`; `LEV_ERR_INVALID_ARG` on any other event type.
///
/// This is an *id*, not a position in the interface snapshot: resolve it to an
/// interface by walking `lev_interface_stats_snapshot` and comparing
/// `lev_interface_stats_id`, then read the name with
/// `lev_interface_stats_name`. It is the same numbering `lev_path_table_entry`
/// reports as `interface_index`, so a path and the announce that created it can
/// be attributed to one interface.
#[no_mangle]
pub unsafe extern "C" fn lev_event_interface_id(ev: *const lev_event_t, out: *mut u64) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        match e.interface_id {
            Some(id) => {
                *out = id;
                LEV_OK
            }
            None => {
                set_last_error("event has no interface id");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Read why a `LEV_EVENT_LINK_CLOSED` event's link closed into `*out`, as one of
/// the `LEV_CLOSE_*` constants. `LEV_ERR_INVALID_ARG` for any other event type.
///
/// The reason is behavioural, not cosmetic: `LEV_CLOSE_BLACKHOLED` must not be
/// retried at all, `LEV_CLOSE_TIMEOUT` wants the path re-resolved first, and
/// `LEV_CLOSE_NORMAL`/`_PEER_CLOSED` may be reconnected immediately.
#[no_mangle]
pub unsafe extern "C" fn lev_event_close_reason(ev: *const lev_event_t, out: *mut c_int) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        match e.close_reason {
            Some(r) => {
                *out = r;
                LEV_OK
            }
            None => {
                set_last_error("event has no close reason");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Read why a `LEV_EVENT_DELIVERY_FAILED` event's packet was not delivered into
/// `*out`, as one of the `LEV_DELIVERY_*` constants. `LEV_ERR_INVALID_ARG` for
/// any other event type.
///
/// `LEV_DELIVERY_TIMEOUT` and `_LINK_FAILED` mean re-send;
/// `LEV_DELIVERY_INVALID_PROOF` means the peer answered with a proof that did
/// not verify, so re-sending produces the same result.
#[no_mangle]
pub unsafe extern "C" fn lev_event_delivery_error(
    ev: *const lev_event_t,
    out: *mut c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        match e.delivery_error {
            Some(err) => {
                *out = err;
                LEV_OK
            }
            None => {
                set_last_error("event has no delivery error");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Read the total encrypted transfer size of a resource, in bytes, into `*out`.
///
/// Set on `LEV_EVENT_RESOURCE_ADVERTISED` (so an app under the AcceptApp
/// strategy can decide whether to accept it) and on
/// `LEV_EVENT_RESOURCE_PROGRESS` (the only place an auto-accepting receiver sees
/// it, since no advertisement is surfaced there). `LEV_ERR_INVALID_ARG` for any
/// other event type.
#[no_mangle]
pub unsafe extern "C" fn lev_event_transfer_size(ev: *const lev_event_t, out: *mut u64) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        match e.transfer_size {
            Some(n) => {
                *out = n;
                LEV_OK
            }
            None => {
                set_last_error("event has no transfer size");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Read the original uncompressed data size of a resource, in bytes, into
/// `*out`. Set on the same events as `lev_event_transfer_size`; this is the size
/// the assembled payload will have, which is what an accept/reject decision and
/// a receive buffer are sized against. `LEV_ERR_INVALID_ARG` for any other event
/// type.
#[no_mangle]
pub unsafe extern "C" fn lev_event_data_size(ev: *const lev_event_t, out: *mut u64) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        match e.data_size {
            Some(n) => {
                *out = n;
                LEV_OK
            }
            None => {
                set_last_error("event has no data size");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Read the 1-based segment index of a `LEV_EVENT_RESOURCE_COMPLETED` event into
/// `*out`. `LEV_ERR_INVALID_ARG` for any other event type.
///
/// A multi-segment resource fires one completion per segment, all carrying the
/// same `resource_hash`, so this and `lev_event_total_segments` are how a C
/// receiver orders the chunks and recognises the last one
/// (`segment_index == total_segments`). Metadata is present on segment 1 only.
#[no_mangle]
pub unsafe extern "C" fn lev_event_segment_index(ev: *const lev_event_t, out: *mut u32) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        match e.segment_index {
            Some(n) => {
                *out = n;
                LEV_OK
            }
            None => {
                set_last_error("event has no segment index");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Read the total number of segments of a `LEV_EVENT_RESOURCE_COMPLETED` event's
/// transfer into `*out`. 1 for a single-segment resource.
/// `LEV_ERR_INVALID_ARG` for any other event type. See
/// `lev_event_segment_index`.
#[no_mangle]
pub unsafe extern "C" fn lev_event_total_segments(ev: *const lev_event_t, out: *mut u32) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        match e.total_segments {
            Some(n) => {
                *out = n;
                LEV_OK
            }
            None => {
                set_last_error("event has no segment count");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Whether a resource event is for a transfer this node is *sending*. Returns 1
/// on the sender side of a `LEV_EVENT_RESOURCE_PROGRESS`/`_COMPLETED`/`_FAILED`
/// event, 0 on the receiver side.
///
/// `LEV_EVENT_RESOURCE_STARTED` is absent from that list because the engine
/// emits it on the receiver only (`is_sender: false` at both push sites), so 0
/// there is the truth and not a gap in the projection.
///
/// One non-resource event also sets this flag: `LEV_EVENT_LINK_ESTABLISHED`
/// returns 1 for a link this node initiated and 0 for an inbound one, which is
/// that event's documented inbound indicator (see the constant). Everything
/// else, and a NULL pointer, returns 0.
///
/// A sender's `LEV_EVENT_RESOURCE_COMPLETED` is the signal that an outgoing
/// transfer finished (its data payload is empty); a receiver's carries the
/// assembled data. Use this to tell the two apart on a node that both sends and
/// receives resources.
#[no_mangle]
pub unsafe extern "C" fn lev_event_is_sender(ev: *const lev_event_t) -> c_int {
    guard(0, || match ev.as_ref() {
        Some(e) if e.is_sender => 1,
        _ => 0,
    })
}

/// Read the message type of a `LEV_EVENT_LINK_MESSAGE` event into `*out`. The
/// type identifies the channel message kind on the wire (0 is the raw bytes
/// message that `lev_link_send` uses and that Python's `RawBytesMessage`
/// carries). `LEV_ERR_INVALID_ARG` for any other event type.
#[no_mangle]
pub unsafe extern "C" fn lev_event_msgtype(ev: *const lev_event_t, out: *mut u16) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if e.ty != LEV_EVENT_LINK_MESSAGE {
            set_last_error("event has no message type");
            return LEV_ERR_INVALID_ARG;
        }
        *out = e.msgtype;
        LEV_OK
    })
}

/// Read the sequence number of a `LEV_EVENT_LINK_MESSAGE` event into `*out`.
/// The channel assigns sequence numbers in send order for reliable, ordered
/// delivery. `LEV_ERR_INVALID_ARG` for any other event type.
#[no_mangle]
pub unsafe extern "C" fn lev_event_sequence(ev: *const lev_event_t, out: *mut u16) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let e = match ev.as_ref() {
            Some(e) => e,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if e.ty != LEV_EVENT_LINK_MESSAGE {
            set_last_error("event has no sequence number");
            return LEV_ERR_INVALID_ARG;
        }
        *out = e.sequence;
        LEV_OK
    })
}

/// Free an event handle returned by `lev_next_event`/`lev_wait_event`.
/// `lev_event_free(NULL)` is a no-op.
#[no_mangle]
pub unsafe extern "C" fn lev_event_free(ev: *mut lev_event_t) {
    guard((), || {
        if !ev.is_null() {
            drop(Box::from_raw(ev));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_int as poll_c_int;

    /// Is the eventfd readable right now (poll with zero timeout)?
    fn readable(fd: RawFd) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, zero timeout.
        let n = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, 0 as poll_c_int) };
        n > 0 && (pfd.revents & libc::POLLIN) != 0
    }

    fn ev(ty: c_int, control: bool) -> Box<lev_event_t> {
        Box::new(lev_event_t::bare(ty, control))
    }

    #[test]
    fn fd_tracks_queue_length() {
        let b = EventBridge::new(8, 8).unwrap();
        assert!(!readable(b.fd()));
        b.enqueue(ev(LEV_EVENT_OTHER, true));
        assert!(readable(b.fd()));
        b.enqueue(ev(LEV_EVENT_OTHER, false));
        assert!(readable(b.fd()));
        assert!(b.next().is_some());
        assert!(readable(b.fd())); // one still queued
        assert!(b.next().is_some());
        assert!(!readable(b.fd())); // drained
        assert!(b.next().is_none());
        assert!(!readable(b.fd()));
    }

    #[test]
    fn data_region_drops_incoming_when_full() {
        let b = EventBridge::new(8, 2).unwrap();
        b.enqueue(ev(LEV_EVENT_OTHER, false));
        b.enqueue(ev(LEV_EVENT_OTHER, false));
        b.enqueue(ev(LEV_EVENT_OTHER, false)); // dropped, never counted
        assert!(b.next().is_some());
        assert!(b.next().is_some());
        assert!(b.next().is_none()); // only two were ever queued
        assert!(!readable(b.fd()));
    }

    #[test]
    fn control_overflow_surfaces_a_marker() {
        let b = EventBridge::new(2, 8).unwrap();
        b.enqueue(ev(LEV_EVENT_OTHER, true));
        b.enqueue(ev(LEV_EVENT_OTHER, true));
        b.enqueue(ev(LEV_EVENT_OTHER, true)); // over cap, recorded as dropped
        b.enqueue(ev(LEV_EVENT_OTHER, true)); // still over cap, dropped count 2

        // Drain the two queued control events; room appears and the marker is
        // flushed with the dropped count.
        let a = b.next().unwrap();
        assert_eq!(a.ty, LEV_EVENT_OTHER);
        let c = b.next().unwrap();
        assert_eq!(c.ty, LEV_EVENT_OTHER);
        let marker = b.next().unwrap();
        assert_eq!(marker.ty, LEV_EVENT_CONTROL_OVERFLOW);
        assert_eq!(marker.dropped_count, 2);
        assert!(b.next().is_none());
        assert!(!readable(b.fd()));
    }

    /// A response that arrived as a resource carries the resource's metadata
    /// blob, and `lev_event_metadata` is the generic accessor for it. The arm
    /// bound the variant with `..` and dropped the field, so every C caller saw
    /// `LEV_ERR_INVALID_ARG` on a response that did have metadata — found by
    /// `event_projection_coverage`'s field guard, which is why the projection
    /// is asserted here rather than only through a C round trip (the C API
    /// sends msgpack-wrapped responses, which never carry metadata).
    #[test]
    fn response_metadata_reaches_the_projection() {
        let ev = leviculum_std::NodeEvent::ResponseReceived {
            link_id: leviculum_std::LinkId::new([1u8; 16]),
            request_id: [2u8; 16],
            response_data: b"page body".to_vec(),
            metadata: Some(b"\x81\xa4name\xa5a.txt".to_vec()),
        };
        let p = project(ev);
        assert_eq!(p.ty, LEV_EVENT_RESPONSE_RECEIVED);
        assert_eq!(
            p.metadata.as_deref(),
            Some(&b"\x81\xa4name\xa5a.txt"[..]),
            "a response's resource metadata must reach lev_event_metadata"
        );
        assert_eq!(p.data, b"page body");
    }

    /// Codeberg #137 projected the destination of a link close, but the only
    /// check on it was `event_projection_coverage`'s source-level guard, which
    /// sees that `e.dest_hash =` appears in the arm and cannot see *what* it is
    /// set to — the projection would still pass with the link id copied in by
    /// mistake. The C-level suite only asserts that a LINK_CLOSED event arrives.
    /// This pins the value.
    #[test]
    fn link_closed_projects_its_own_destination() {
        let dest = leviculum_std::DestinationHash::new([0xABu8; 16]);
        let ev = leviculum_std::NodeEvent::LinkClosed {
            link_id: leviculum_std::LinkId::new([0x11u8; 16]),
            reason: leviculum_std::LinkCloseReason::PeerClosed,
            is_initiator: true,
            destination_hash: dest,
        };
        let p = project(ev);
        assert_eq!(p.ty, LEV_EVENT_LINK_CLOSED);
        assert_eq!(p.link_id, Some([0x11u8; 16]));
        assert_eq!(
            p.dest_hash,
            Some([0xABu8; 16]),
            "a link close must name the destination whose link closed, so a \
             responder hosting several destinations knows which one lost a peer"
        );
    }

    /// Read an event through the C accessor, as a C app would.
    fn as_ptr(e: &lev_event_t) -> *const lev_event_t {
        e as *const lev_event_t
    }

    /// The interface an event arrived on is an id, and it is the id the engine
    /// used — not the position of anything.
    ///
    /// `AnnounceReceived` carries the identical one-line projection but cannot
    /// be built here: `ReceivedAnnounce`'s only constructor is
    /// `pub(crate) from_packet` inside `leviculum-core`. The source-level field
    /// guard covers that arm, and `announce_interface_id_resolves_to_an_interface`
    /// in `ffi_integration` drives it end to end over a real interface.
    #[test]
    fn path_and_packet_events_carry_the_interface_they_arrived_on() {
        let path = project(leviculum_std::NodeEvent::PathFound {
            destination_hash: leviculum_std::DestinationHash::new([1u8; 16]),
            hops: 3,
            interface_index: 7,
        });
        let packet = project(leviculum_std::NodeEvent::PacketReceived {
            destination: leviculum_std::DestinationHash::new([2u8; 16]),
            data: b"payload".to_vec(),
            interface_index: 4,
        });
        unsafe {
            let mut id = u64::MAX;
            assert_eq!(lev_event_interface_id(as_ptr(&path), &mut id), LEV_OK);
            assert_eq!(id, 7, "a path event must name the interface that found it");
            assert_eq!(lev_event_interface_id(as_ptr(&packet), &mut id), LEV_OK);
            assert_eq!(
                id, 4,
                "a packet event must name the interface it came in on"
            );

            // An event with no interface says so rather than reporting 0, which
            // is a real interface id.
            let stale = project(leviculum_std::NodeEvent::LinkStale {
                link_id: leviculum_std::LinkId::new([3u8; 16]),
            });
            assert_eq!(
                lev_event_interface_id(as_ptr(&stale), &mut id),
                LEV_ERR_INVALID_ARG
            );
            assert_eq!(
                lev_event_interface_id(as_ptr(&path), std::ptr::null_mut()),
                LEV_ERR_NULL_PTR
            );
            assert_eq!(
                lev_event_interface_id(std::ptr::null(), &mut id),
                LEV_ERR_NULL_PTR
            );
        }
    }

    /// Every close reason reaches C as its own constant. A C app reconnects
    /// differently per reason — `LEV_CLOSE_BLACKHOLED` must not be retried at
    /// all — so collapsing two of them, or letting one fall through to
    /// `LEV_CLOSE_OTHER`, is a behavioural defect and not a cosmetic one.
    #[test]
    fn every_close_reason_reaches_c_as_its_own_constant() {
        use leviculum_std::LinkCloseReason as R;
        let expected = [
            (R::Normal, LEV_CLOSE_NORMAL),
            (R::Timeout, LEV_CLOSE_TIMEOUT),
            (R::InvalidProof, LEV_CLOSE_INVALID_PROOF),
            (R::PeerClosed, LEV_CLOSE_PEER_CLOSED),
            (R::Stale, LEV_CLOSE_STALE),
            (R::ChannelExhausted, LEV_CLOSE_CHANNEL_EXHAUSTED),
            (R::Blackholed, LEV_CLOSE_BLACKHOLED),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (reason, want) in expected {
            let p = project(leviculum_std::NodeEvent::LinkClosed {
                link_id: leviculum_std::LinkId::new([9u8; 16]),
                reason,
                is_initiator: false,
                destination_hash: leviculum_std::DestinationHash::new([8u8; 16]),
            });
            let mut got = -1;
            unsafe {
                assert_eq!(lev_event_close_reason(as_ptr(&p), &mut got), LEV_OK);
            }
            assert_eq!(got, want, "{reason:?} must project to {want}");
            assert_ne!(
                got, LEV_CLOSE_OTHER,
                "{reason:?} fell into the wildcard arm"
            );
            assert!(
                seen.insert(got),
                "{reason:?} shares a constant with another"
            );
        }
        assert_eq!(seen.len(), expected.len());

        // Not a link close: no reason, and it says so.
        let other = project(leviculum_std::NodeEvent::LinkStale {
            link_id: leviculum_std::LinkId::new([9u8; 16]),
        });
        let mut got = 0;
        unsafe {
            assert_eq!(
                lev_event_close_reason(as_ptr(&other), &mut got),
                LEV_ERR_INVALID_ARG
            );
            assert_eq!(
                lev_event_close_reason(as_ptr(&other), std::ptr::null_mut()),
                LEV_ERR_NULL_PTR
            );
        }
    }

    /// Each delivery failure reaches C as its own constant. `_INVALID_PROOF` is
    /// the one that changes behaviour: the peer answered, so the re-send the
    /// other two ask for cannot succeed.
    #[test]
    fn every_delivery_error_reaches_c_as_its_own_constant() {
        use leviculum_std::DeliveryError as E;
        for (error, want) in [
            (E::Timeout, LEV_DELIVERY_TIMEOUT),
            (E::LinkFailed, LEV_DELIVERY_LINK_FAILED),
            (E::InvalidProof, LEV_DELIVERY_INVALID_PROOF),
        ] {
            let p = project(leviculum_std::NodeEvent::DeliveryFailed {
                packet_hash: [0xEEu8; 16],
                error,
            });
            let mut got = -1;
            unsafe {
                assert_eq!(lev_event_delivery_error(as_ptr(&p), &mut got), LEV_OK);
            }
            assert_eq!(got, want, "{error:?} must project to {want}");
            assert_ne!(got, LEV_DELIVERY_OTHER, "{error:?} fell into the wildcard");
        }
    }

    /// A resource's sizes are what an accept-or-reject decision turns on, and
    /// they must be the resource's own numbers.
    ///
    /// The advertise event is only emitted under the AcceptApp strategy, so an
    /// auto-accepting receiver sees the sizes on the progress event or nowhere;
    /// both are pinned. The two sizes are deliberately different here — the
    /// encrypted transfer is larger than the payload — so swapping them fails.
    #[test]
    fn resource_events_carry_both_sizes() {
        let adv = project(leviculum_std::NodeEvent::ResourceAdvertised {
            link_id: leviculum_std::LinkId::new([1u8; 16]),
            resource_hash: [2u8; 32],
            transfer_size: 9_000,
            data_size: 8_192,
        });
        let progress = project(leviculum_std::NodeEvent::ResourceProgress {
            link_id: leviculum_std::LinkId::new([1u8; 16]),
            resource_hash: [2u8; 32],
            progress: 0.5,
            transfer_size: 9_000,
            data_size: 8_192,
            is_sender: false,
        });
        unsafe {
            for e in [&adv, &progress] {
                let mut transfer = 0u64;
                let mut data = 0u64;
                assert_eq!(lev_event_transfer_size(as_ptr(e), &mut transfer), LEV_OK);
                assert_eq!(lev_event_data_size(as_ptr(e), &mut data), LEV_OK);
                assert_eq!(transfer, 9_000, "encrypted transfer size");
                assert_eq!(data, 8_192, "uncompressed payload size");
            }
            // Neither size belongs to a link close.
            let closed = project(leviculum_std::NodeEvent::LinkClosed {
                link_id: leviculum_std::LinkId::new([1u8; 16]),
                reason: leviculum_std::LinkCloseReason::Normal,
                is_initiator: false,
                destination_hash: leviculum_std::DestinationHash::new([3u8; 16]),
            });
            let mut v = 0u64;
            assert_eq!(
                lev_event_transfer_size(as_ptr(&closed), &mut v),
                LEV_ERR_INVALID_ARG
            );
            assert_eq!(
                lev_event_data_size(as_ptr(&closed), &mut v),
                LEV_ERR_INVALID_ARG
            );
        }
    }

    /// A multi-segment resource fires one completion per segment under the same
    /// resource hash, so the segment position is the only thing that tells the
    /// chunks apart. Index and total are distinct values here so projecting one
    /// into the other's slot fails.
    #[test]
    fn resource_completion_carries_its_segment_position() {
        let p = project(leviculum_std::NodeEvent::ResourceCompleted {
            link_id: leviculum_std::LinkId::new([1u8; 16]),
            resource_hash: [2u8; 32],
            data: b"chunk".to_vec(),
            metadata: None,
            is_sender: false,
            segment_index: 2,
            total_segments: 5,
        });
        unsafe {
            let mut index = 0u32;
            let mut total = 0u32;
            assert_eq!(lev_event_segment_index(as_ptr(&p), &mut index), LEV_OK);
            assert_eq!(lev_event_total_segments(as_ptr(&p), &mut total), LEV_OK);
            assert_eq!(index, 2, "the 1-based position of this segment");
            assert_eq!(total, 5, "how many segments the transfer has");
            assert!(index < total, "segment 2 of 5 is not the last one");

            let progress = project(leviculum_std::NodeEvent::ResourceProgress {
                link_id: leviculum_std::LinkId::new([1u8; 16]),
                resource_hash: [2u8; 32],
                progress: 0.5,
                transfer_size: 1,
                data_size: 1,
                is_sender: false,
            });
            assert_eq!(
                lev_event_segment_index(as_ptr(&progress), &mut index),
                LEV_ERR_INVALID_ARG
            );
            assert_eq!(
                lev_event_total_segments(as_ptr(&progress), &mut total),
                LEV_ERR_INVALID_ARG
            );
        }
    }

    #[test]
    fn flood_preserves_invariant() {
        let b = EventBridge::new(64, 64).unwrap();
        for i in 0..1000 {
            b.enqueue(ev(LEV_EVENT_OTHER, i % 2 == 0));
        }
        let mut drained = 0;
        while b.next().is_some() {
            drained += 1;
        }
        // Caps bound the queue; nothing readable after a full drain.
        assert!(drained <= 64 + 64 + 1); // + possible overflow marker
        assert!(!readable(b.fd()));
    }
}
