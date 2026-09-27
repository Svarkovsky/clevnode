//! Sans-I/O driver for Reticulum
//!
//! This module provides `ReticulumNode`, the async I/O driver that bridges the
//! pure state machine (`NodeCore` from leviculum-core) with actual network
//! interfaces. It owns the interfaces and dispatches `Action` values.
//!
//! # Architecture (Sans-I/O)
//!
//! `NodeCore` from leviculum-core is a pure state machine that never performs I/O
//! directly. Instead, it returns `Action` values (SendPacket, Broadcast) that this
//! driver dispatches to the actual network interfaces.
//!
//! The event loop awaits interface readability via `select!`:
//! 1. Wakes immediately when any interface has data (no polling delay)
//! 2. Feeds packets to `NodeCore::handle_packet()` → gets `TickOutput`
//! 3. Dispatches `TickOutput` from external callers (connect, send, close)
//! 4. Wakes on timer deadline for periodic maintenance
//! 5. Dispatches `Action`s from `TickOutput` to interfaces
//! 6. Forwards `NodeEvent`s to the application
//!
//! # Example
//!
//! ```no_run
//! use leviculum_std::driver::ReticulumNodeBuilder;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create a node with a TCP interface
//!     let mut node = ReticulumNodeBuilder::new()
//!         .add_tcp_client("127.0.0.1:4242".parse()?)
//!         .build()
//!         .await?;
//!
//!     // Start the node
//!     node.start().await?;
//!
//!     // Take event receiver to handle events
//!     let mut events = node.take_event_receiver().unwrap();
//!
//!     // Process events
//!     while let Some(event) = events.recv().await {
//!         println!("Event: {:?}", event);
//!     }
//!
//!     Ok(())
//! }
//! ```

mod builder;
mod completions;
mod interface_build;
mod processor;
#[cfg(feature = "rpc")]
mod remote_mgmt;
#[cfg(not(feature = "rpc"))]
mod remote_mgmt_dummy;
mod sender;
mod stream;

use completions::CompletionRegistry;
#[cfg(feature = "rpc")]
use remote_mgmt::RemoteMgmtResponder;
#[cfg(not(feature = "rpc"))]
use remote_mgmt_dummy::RemoteMgmtResponder;

pub use builder::ReticulumNodeBuilder;
pub use completions::{
    Completion, CompletionError, EventTap, FilteredEventTap, LinkEstablishedFuture,
    RequestResponseFuture, ResourceSentFuture, ResourceSentInfo, ResponseInfo, TapEvent,
    DEFAULT_EVENT_TAP_CAPACITY,
};
pub use processor::{CoreProcessor, PROCESSOR_TICK_BUDGET};
pub use sender::PacketSender;
pub use stream::LinkHandle;

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;

use crate::sync_ext::MutexRecover;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use tokio::sync::mpsc::{
    self,
    error::{TryRecvError, TrySendError},
};
use tokio::sync::watch;

use crate::interfaces::IncomingPacket;
use leviculum_announce_policy::{Decision, PeerAnnounceLimiter};
use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::link::LinkId;
use leviculum_core::node::{EventClass, FrameDropReason, NodeCore, NodeEvent};
use leviculum_core::traits::{InterfaceError, Storage as StorageTrait};
use leviculum_core::transport::{InterfaceId, TickOutput};
use leviculum_core::{AnnounceControl, Destination, DestinationHash};

use crate::clock::SystemClock;
use crate::config::InterfaceConfig;
use crate::error::Error;
use crate::interfaces::tcp::{
    spawn_tcp_client_with_reconnect, TcpClientConfig, TcpClientHandle,
    DEFAULT_RECONNECT_MAX_INTERVAL, DEFAULT_TCP_CONNECT_TIMEOUT, TCP_DEFAULT_BUFFER_SIZE,
};
use crate::interfaces::{InterfaceHandle, InterfaceRegistry, InterfaceStatsMap};
use crate::storage::Storage;

/// Type alias for the concrete NodeCore used by std platforms.
///
/// Public because it is the handle a [`CoreProcessor`] is given (Codeberg
/// #196). It is the sans-io core: no async surface, and no channel back into
/// the driver's event loop.
///
/// Its two crate-local type parameters are re-exported below because they have
/// to be: naming a private type in the signature of a trait a downstream crate
/// implements is a hard error there, not merely a lint here. This is the public
/// surface design B costs, and the issue's own summary of B ("requires
/// publishing `&mut StdNodeCore`") is what it means concretely.
pub type StdNodeCore = NodeCore<rand_core::OsRng, SystemClock, Storage>;

// Re-exported solely so `StdNodeCore` is nameable and implementable downstream
// (Codeberg #196). A processor has no reason to touch either directly.
pub use crate::clock::SystemClock as StdClock;
pub use crate::storage::Storage as StdStorage;

/// Capacity of the internal action-dispatch channel that carries
/// `TickOutput`s produced outside the event loop (connect, send_on_link,
/// close_link, announce) into it. Each such call produces exactly one
/// `TickOutput` and the loop drains them every iteration, so this only
/// backs up if the loop is already blocked.
const ACTION_DISPATCH_CAPACITY: usize = 256;

/// Maximum packets per interface in the retry queue.
/// Sized to absorb announce-burst fan-out from transit peers; observed
/// peak >500 packets in a single event-loop tick on transit-active lnsd.
/// When full, oldest is dropped.
const RETRY_QUEUE_CAP: usize = 1024;

/// Depth at which `push_retry_with_warn` emits a one-shot tracing::warn
/// to flag that first-order backpressure may be mis-tuned. Held at
/// 12.5 % of `RETRY_QUEUE_CAP` so the warn fires well before drops do.
const RETRY_QUEUE_DEPTH_WARN: usize = 128;

/// Total wall-clock budget for the event loop's graceful drain on shutdown
/// (Codeberg #77). After draining `action_dispatch_rx` and dispatching the
/// queued outputs (e.g. a responder `close_link`), the loop waits up to this
/// long for the interface tasks to flush their outgoing queues to the socket
/// before the runtime aborts them. Caps teardown so a wedged or back-pressured
/// interface cannot hang shutdown; the common case exits in a couple of polls.
const SHUTDOWN_FLUSH_BOUND: Duration = Duration::from_millis(250);

/// Poll interval while waiting for the interface outgoing queues to drain
/// during the shutdown flush. Tight so teardown stays prompt; each poll yields
/// to the (co-scheduled) interface tasks so they can pop and write.
const SHUTDOWN_FLUSH_POLL: Duration = Duration::from_millis(1);

/// Write margin applied once the outgoing queues report empty: the interface
/// task has popped the last packet but may still be inside `write_all`. Yield
/// this long so the final frame reaches the socket before the task is aborted.
/// Generous slack over a sub-millisecond loopback write to absorb scheduler
/// latency on a loaded CI worker.
const SHUTDOWN_FLUSH_MARGIN: Duration = Duration::from_millis(25);

/// How often the event loop reconciles auto-connected interfaces against the
/// live discovered-interface registry (Codeberg #32, sub-task b). Python's
/// monitor job polls every 5 s; we poll faster so a discovered peer is
/// auto-connected promptly after its announce lands (local timing only, no
/// wire or semantic change).
const AUTOCONNECT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Total wall-clock budget the `Drop` path waits for the event loop to finish
/// its graceful drain+flush before aborting the runtime. Slightly larger than
/// the loop's own `SHUTDOWN_FLUSH_BOUND` + `SHUTDOWN_FLUSH_MARGIN` so the
/// bounded flush can complete; the wait early-exits the instant the runner
/// finishes, so a clean teardown costs only a few milliseconds.
const DROP_FLUSH_BOUND: Duration = Duration::from_millis(400);

/// Sender half of the split control/data node-event channels (Codeberg #71).
///
/// Lives in the event loop only (single owner, so `&mut self` is enough for
/// the dropped-counter — no atomics needed). [`emit`](EventSink::emit)
/// classifies each [`NodeEvent`] with [`NodeEvent::event_class`] and routes
/// it:
///
/// * **Control** plane — lossless by default. When the bounded control
///   channel is full the event is dropped but counted, and the loss is made
///   visible by delivering one [`NodeEvent::ControlPlaneOverflow`] as soon as
///   the channel has room (see [`flush_overflow`](EventSink::flush_overflow)).
///   The marker itself is only enqueued when there is room, so it is never
///   lost.
/// * **Data** plane — droppable. A full data channel drops silently; that is
///   the intended backpressure.
struct EventSink {
    /// Lossless-by-default control plane.
    control_tx: mpsc::Sender<NodeEvent>,
    /// Droppable data plane (backpressure).
    data_tx: mpsc::Sender<NodeEvent>,
    /// Configured control-channel capacity, for the overflow warn log.
    control_capacity: usize,
    /// Control events dropped since the last `ControlPlaneOverflow` marker
    /// was delivered. Surfaced (and reset) by `flush_overflow` once the
    /// control channel has room.
    control_dropped: u64,
}

impl EventSink {
    /// Route one event to the control or data plane by its class.
    fn emit(&mut self, event: NodeEvent) {
        match event.event_class() {
            EventClass::Control => self.emit_control(event),
            EventClass::Data => self.emit_data(event),
        }
    }

    /// Deliver a control-plane event losslessly, or count it as dropped and
    /// surface the loss via `ControlPlaneOverflow`.
    ///
    /// The real event is tried first so a freed slot is never starved by the
    /// overflow marker; only when the event lands (proving the channel has
    /// room) do we try to flush any pending overflow marker behind it.
    fn emit_control(&mut self, event: NodeEvent) {
        match self.control_tx.try_send(event) {
            Ok(()) => self.flush_overflow(),
            Err(TrySendError::Full(ev)) => {
                self.control_dropped += 1;
                // BUG-1 sibling: structured fields only, no trailing prose
                // (the spaces would corrupt the canonical event-log line).
                tracing::warn!(
                    event = "EVENT_CHANNEL_FULL",
                    queue_capacity = self.control_capacity,
                    dropped_event_type = ev.variant_name(),
                    pending_dropped = self.control_dropped,
                );
            }
            Err(TrySendError::Closed(ev)) => {
                tracing::warn!(
                    event = "EVENT_CHANNEL_CLOSED",
                    dropped_event_type = ev.variant_name(),
                );
            }
        }
    }

    /// If control events were previously dropped, try to deliver one
    /// `ControlPlaneOverflow` marker reporting the count. It is only enqueued
    /// when the channel has room, so the marker is never itself dropped; the
    /// counter is reset only on a successful send.
    fn flush_overflow(&mut self) {
        if self.control_dropped == 0 {
            return;
        }
        let dropped_count = self.control_dropped;
        match self
            .control_tx
            .try_send(NodeEvent::ControlPlaneOverflow { dropped_count })
        {
            Ok(()) => {
                tracing::warn!(event = "CONTROL_PLANE_OVERFLOW", dropped_count);
                self.control_dropped = 0;
            }
            // Still full: keep the count and try again on the next emit.
            Err(TrySendError::Full(_)) => {}
            // Receiver gone: nothing can observe the marker anyway.
            Err(TrySendError::Closed(_)) => self.control_dropped = 0,
        }
    }

    /// Deliver a data-plane event, dropping silently when full (backpressure).
    fn emit_data(&mut self, event: NodeEvent) {
        match self.data_tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(ev)) => {
                // Silent by design: data-plane drops are normal backpressure.
                tracing::trace!(
                    dropped_event_type = ev.variant_name(),
                    "data event dropped (backpressure)"
                );
            }
            Err(TrySendError::Closed(ev)) => {
                tracing::warn!(
                    event = "EVENT_CHANNEL_CLOSED",
                    dropped_event_type = ev.variant_name(),
                );
            }
        }
    }
}

/// Receiver half handed to the application by
/// [`ReticulumNode::take_event_receiver`] (Codeberg #71).
///
/// Merges the split control/data channels into a single stream, draining the
/// control plane with strict priority over the data plane so a flood of data
/// events can never starve discovery- or lifecycle-critical control events.
pub struct EventReceiver {
    /// Lossless-by-default control plane (drained first).
    control: mpsc::Receiver<NodeEvent>,
    /// Droppable data plane.
    data: mpsc::Receiver<NodeEvent>,
}

impl EventReceiver {
    /// Receive the next event, control plane first.
    ///
    /// Returns `None` only once both planes are closed and drained. Drop-safe
    /// for use in `tokio::select!`: a buffered control event is returned
    /// synchronously, otherwise both channels are awaited with the control
    /// plane biased, and `tokio::sync::mpsc::Receiver::recv` is cancel-safe.
    pub async fn recv(&mut self) -> Option<NodeEvent> {
        // Strict priority: return any already-buffered control event first.
        match self.control.try_recv() {
            Ok(ev) => return Some(ev),
            Err(TryRecvError::Empty) => {}
            // Control closed and drained: serve the data plane to completion.
            Err(TryRecvError::Disconnected) => return self.data.recv().await,
        }
        // Nothing buffered on control; wait on both, control biased so a
        // control event that races in still wins.
        tokio::select! {
            biased;
            ev = self.control.recv() => match ev {
                Some(e) => Some(e),
                None => self.data.recv().await, // control closed
            },
            ev = self.data.recv() => match ev {
                Some(e) => Some(e),
                None => self.control.recv().await, // data closed
            },
        }
    }

    /// Non-blocking receive, control plane first. Mirrors
    /// [`tokio::sync::mpsc::Receiver::try_recv`].
    pub fn try_recv(&mut self) -> Result<NodeEvent, TryRecvError> {
        match self.control.try_recv() {
            Ok(ev) => Ok(ev),
            Err(TryRecvError::Empty) => self.data.try_recv(),
            // Control closed: fall back to whatever the data plane reports.
            Err(TryRecvError::Disconnected) => self.data.try_recv(),
        }
    }
}

/// Resolve the `lt_alock` u16 (long-term airtime lock) sent to the RNode
/// firmware from the configured `airtime_limit_long` percentage and the TX
/// `frequency` (Hz).
///
/// An explicit `airtime_limit_long` always wins, including `0` (which the
/// firmware reads as "unlimited"): `Some(p) -> p * 100`. When it is absent and
/// the frequency falls in an EU 863-870 MHz sub-band, the ETSI duty-cycle cap
/// becomes the default (lawful-by-default, Codeberg #55): the fraction from
/// `etsi_eu868_duty_cycle` maps to `fraction * 10000`. Non-EU / out-of-band
/// frequencies with no explicit limit stay off (`None`).
///
/// An explicit value *more permissive* than the lawful default for the band —
/// including `0`, which is the airtime lock's "unlimited" and therefore the
/// most permissive value there is, not the least — is warned about and then
/// honoured, the same way [`leviculum_core::rnode::resolve_tx_power`] treats a
/// `txpower` over the derived e.r.p. cap. No radio configuration is refused
/// for a radio-regulatory reason; a silent override is the other half of that
/// policy failing, so the warning is at WARN and needs no debug filter.
fn resolve_lt_alock(airtime_limit_long: Option<f64>, frequency: u32) -> Option<u16> {
    match airtime_limit_long {
        Some(p) => {
            let alock = (p * 100.0) as u16;
            if let Some(fraction) = leviculum_core::rnode::etsi_eu868_duty_cycle(frequency as u64) {
                let lawful = (fraction * 10000.0) as u16;
                if alock == 0 || alock > lawful {
                    tracing::warn!(
                        "airtime_limit_long {} exceeds the ETSI EU868 lawful default {:.1}% \
                         for {} Hz; honouring the explicit value",
                        if alock == 0 {
                            "0% (unlimited)".to_string()
                        } else {
                            format!("{:.2}%", p)
                        },
                        fraction * 100.0,
                        frequency,
                    );
                }
            }
            Some(alock)
        }
        None => leviculum_core::rnode::etsi_eu868_duty_cycle(frequency as u64).map(|fraction| {
            let alock = (fraction * 10000.0) as u16;
            tracing::info!(
                "RNode: no airtime_limit_long set; applying ETSI EU868 lawful default \
                 for {} Hz -> {:.1}% duty cycle (lt_alock={})",
                frequency,
                fraction * 100.0,
                alock,
            );
            alock
        }),
    }
}

/// The IFAC size an interface type falls back to when the config names no
/// `ifac_size`, mirroring each Python class's `DEFAULT_IFAC_SIZE`.
///
/// This is not a stylistic preference: the size fixes the length of the access
/// code on every packet, so two peers that disagree reject each other's frames
/// outright — a silent, un-diagnosable dead link rather than a degraded one
/// (Codeberg #293). The grouping is therefore pinned type-by-type against the
/// reference in `test_default_ifac_size_matches_python_reference`, not inferred
/// from the transport medium.
///
/// Byte-serial family (8): `AX25KISSInterface.py:70`, `KISSInterface.py:63`,
/// `PipeInterface.py:57`, `RNodeInterface.py:110`,
/// `RNodeMultiInterface.py:137`, `SerialInterface.py:53`.
/// Network family (16): `AutoInterface.py:50`, `I2PInterface.py:839`,
/// `TCPInterface.py:77` (client) and `:454` (server), `UDPInterface.py:42`.
/// `BackboneInterface` / `BackboneClientInterface` need no arm: `ini_config`
/// rewrites them to `TCPServerInterface` / `TCPClientInterface` before the
/// driver sees them, and upstream all four are 16 (`BackboneInterface.py:54`,
/// `:508`), so the rewrite cannot change the answer.
fn default_ifac_size(interface_type: &str) -> usize {
    match interface_type {
        "AX25KISSInterface"
        | "KISSInterface"
        | "PipeInterface"
        | "RNodeInterface"
        | "RNodeMultiInterface"
        | "SerialInterface" => leviculum_core::constants::IFAC_DEFAULT_SIZE_SERIAL,
        // BLEInterface: not an upstream RNS class; the reference plugin
        // pins the network-family value itself (`ble-reticulum@07d94130`
        // `BLEInterface.py`, `DEFAULT_IFAC_SIZE = 16`).
        "AutoInterface" | "BLEInterface" | "I2PInterface" | "TCPClientInterface"
        | "TCPServerInterface" | "UDPInterface" => {
            leviculum_core::constants::IFAC_DEFAULT_SIZE_NETWORK
        }
        // Deliberate default, not a fall-through: an unrecognised type is one
        // we do not build (`interface_build::build` warns and self-manages), or
        // a future addition. 16 is the safer of the two — it matches every
        // network-family class upstream, and the byte-serial exception is a
        // closed, hand-maintained list. A new type landing here silently takes
        // 16; the pinning test is what forces it to be classified on purpose.
        _ => leviculum_core::constants::IFAC_DEFAULT_SIZE_NETWORK,
    }
}

/// Build an IfacConfig from interface configuration, if IFAC params are present.
fn build_ifac_config(config: &InterfaceConfig) -> Option<leviculum_core::ifac::IfacConfig> {
    if config.networkname.is_none() && config.passphrase.is_none() {
        return None;
    }
    let size = config
        .ifac_size
        .unwrap_or_else(|| default_ifac_size(&config.interface_type));
    match leviculum_core::ifac::IfacConfig::new(
        config.networkname.as_deref(),
        config.passphrase.as_deref(),
        size,
    ) {
        Ok(ifac) => Some(ifac),
        Err(e) => {
            tracing::warn!("Failed to create IFAC config: {:?}", e);
            None
        }
    }
}

/// At startup, IFAC for leaf interfaces is registered by index in the config
/// loop, which never runs for a runtime attach — without this, a runtime
/// interface configured with IFAC keys would silently come up unauthenticated.
/// Carry the IFAC on the handle instead: the dynamic-registration branch
/// applies `info.ifac` exactly like a server-accepted child. Handles that
/// already carry one (I2P children) keep theirs.
fn apply_runtime_ifac(config: &InterfaceConfig, handles: &mut [InterfaceHandle]) {
    let Some(ifac) = build_ifac_config(config) else {
        return;
    };
    for handle in handles.iter_mut() {
        if handle.info.ifac.is_none() {
            handle.info.ifac = Some(ifac.clone());
        }
    }
}

/// Hearing-interface IFAC per discovered endpoint (Codeberg #151):
/// `discovery_hash` -> the [`IfacConfig`](leviculum_core::ifac::IfacConfig) of
/// the interface the endpoint's discovery announce was last heard on under
/// IFAC protection.
type HeardIfacMap =
    BTreeMap<[u8; leviculum_core::discovery::STAMP_SIZE], leviculum_core::ifac::IfacConfig>;

/// IFAC resolution for an auto-connected discovered endpoint (Codeberg #151).
enum AutoConnectIfac {
    /// No IFAC anywhere: connect unauthenticated (open-network behaviour).
    Open,
    /// Spawn the client under this IFAC (boxed: the variant is much larger
    /// than the others, clippy `large_enum_variant`).
    Protected(Box<leviculum_core::ifac::IfacConfig>),
    /// Do not connect at all (fail closed), for the named reason.
    Refused { reason: &'static str },
}

/// Resolve the IFAC an auto-connected discovered TCP client must carry
/// (Codeberg #151, fail closed):
///
/// 1. The discovery record advertises `ifac_netname`/`ifac_netkey` -> use them
///    (Python `InterfaceDiscovery.autoconnect` passes both to `_add_interface`,
///    which derives the key with the interface-default IFAC size — 16 for
///    Backbone/TCP).
/// 2. Otherwise inherit the IFAC of the interface the announce was heard on
///    (the parent-child rule of `AutoInterface.py:559-561`: what is found
///    through a protected interface is spoken to under the same protection).
/// 3. Neither resolves but this node has operator-configured IFAC -> refuse.
///    An operator who closed their network must not get an open link opened
///    beside it by an automatism.
/// 4. No IFAC anywhere -> connect open, like today.
fn resolve_autoconnect_ifac(
    rec_netname: Option<&str>,
    rec_netkey: Option<&str>,
    heard_on: Option<&leviculum_core::ifac::IfacConfig>,
    operator_ifac_present: bool,
) -> AutoConnectIfac {
    if rec_netname.is_some() || rec_netkey.is_some() {
        // Python publishes the pair together (discovery_publish_ifac), but a
        // single present field still identifies a protected endpoint, and
        // `_add_interface` derives from whichever is set.
        return match leviculum_core::ifac::IfacConfig::new(
            rec_netname,
            rec_netkey,
            leviculum_core::constants::IFAC_DEFAULT_SIZE_NETWORK,
        ) {
            Ok(cfg) => AutoConnectIfac::Protected(Box::new(cfg)),
            // The record claims protection we cannot derive: never fall back
            // to an open connection to a peer that declared itself closed.
            Err(_) => AutoConnectIfac::Refused {
                reason: "the record advertises IFAC material we cannot derive a key from",
            },
        };
    }
    if let Some(parent) = heard_on {
        return AutoConnectIfac::Protected(Box::new(parent.clone()));
    }
    if operator_ifac_present {
        return AutoConnectIfac::Refused {
            reason: "this node runs IFAC, but the record advertises none and the announce \
                     was not heard on an IFAC-protected interface",
        };
    }
    AutoConnectIfac::Open
}

/// Build an [`AnnounceRateConfig`] from interface configuration, applying
/// Python's validation and coupling (Reticulum.py:798-821). Returns `None`
/// when no `announce_rate_*` key was set (an absent entry resolves identically
/// to an all-`None` config). Codeberg #67 Stage 2a: read + report only.
///
/// - `announce_rate_target` is kept only when > 0 (Python `> 0`).
/// - `announce_rate_penalty` / `announce_rate_grace` are kept when >= 0 (always
///   true for the `u32` parse, which already rejects negatives).
/// - When a target is set but penalty/grace are unset, they default to 0.
fn build_announce_rate_config(
    config: &InterfaceConfig,
) -> Option<leviculum_core::transport::AnnounceRateConfig> {
    let target = config.announce_rate_target.filter(|&t| t > 0);
    let mut penalty = config.announce_rate_penalty;
    let mut grace = config.announce_rate_grace;

    if config.announce_rate_target.is_none()
        && config.announce_rate_penalty.is_none()
        && config.announce_rate_grace.is_none()
    {
        return None;
    }

    // Coupling: a configured target defaults an unset penalty/grace to 0.
    if target.is_some() {
        penalty.get_or_insert(0);
        grace.get_or_insert(0);
    }

    Some(leviculum_core::transport::AnnounceRateConfig {
        target,
        penalty,
        grace,
    })
}

/// Resolve the configured `announce_cap` percentage to the whole per cent the
/// transport's announce throttler keeps (Codeberg #92, Reticulum.py:713-716).
/// Returns `None` when the key was absent.
///
/// The ini layer already dropped values outside Python's `0 < v <= 100` window,
/// so this only bridges the representation: Python holds the share as a float,
/// the core as whole per cent. A fractional value is rounded to nearest, and a
/// value that would round down to zero resolves to 1% — the smallest share the
/// cap can express — because 0 would mean "no announces at all", which is not
/// what a sub-1% cap asks for.
fn announce_cap_percent_from_config(config: &InterfaceConfig) -> Option<u32> {
    config
        .announce_cap
        .map(|cap| (cap.round().max(1.0) as u32).min(100))
}

/// The `peer=` field of an `[ANNOUNCE]` event: the first four bytes of
/// the identity, the same eight hex digits the BLE interface's own lines
/// and the firmware's `[ANNOUNCE]` line carry, so one capture can be
/// grepped for a peer across both.
fn peer_hex8(identity: &[u8; 16]) -> String {
    identity
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// How many peer identities `lnsd`'s peer-up announce gate remembers.
///
/// Thirty-two, against the firmware's eight: a daemon's BLE adapter is
/// not bounded by four SoftDevice links, and on a PC the table costs 768
/// bytes. Overflowing it costs one extra announce for the peer whose own
/// announce is furthest in the past, never a wrongly withheld one — see
/// [`PeerAnnounceLimiter`].
const PEER_ANNOUNCE_SLOTS: usize = 32;

/// Announce this daemon's destinations to a peer whose first link on a
/// multi-peer interface just came up (Codeberg #376).
///
/// The daemon owns no `lxmf.delivery` destination the way a board does —
/// its destinations are whatever was registered on it (the probe and
/// management destinations) plus the cached announces of its IPC
/// clients — so "announce ourselves" means exactly the set interface
/// recovery re-announces, addressed at the one peer instead of put on the
/// whole interface. The delivery hint is what keeps a neighbour on the
/// same adapter from receiving a copy it would forward, which is the
/// relayed-announce race #376 opened with.
///
/// `clock_ok` and `now_ms` are read from the core by the caller and
/// passed in rather than taken here, so the two gates this function obeys
/// — the clock rule and the fifteen-minute per-identity limit — are
/// exercisable without a fake clock. On a PC `has_plausible_wall_clock`
/// is true from the first instruction; the gate still exists because the
/// same policy runs on a board that boots without one.
fn peer_up_announce(
    core: &mut StdNodeCore,
    gate: &mut PeerAnnounceLimiter<PEER_ANNOUNCE_SLOTS>,
    iface: InterfaceId,
    peer: [u8; 16],
    now_ms: u64,
    clock_ok: bool,
) -> leviculum_core::transport::TickOutput {
    match gate.peer_up(peer, now_ms, clock_ok) {
        Decision::Announce => {
            let (sent, output) = core.announce_local_destinations_to_peer(iface, peer);
            if sent > 0 {
                tracing::info!(
                    event = "ANNOUNCE_TX",
                    reason = "peer-up",
                    peer = %peer_hex8(&peer),
                    iface = iface.0,
                    count = sent,
                );
            }
            output
        }
        Decision::Withheld(reason) => {
            tracing::debug!(
                event = "ANNOUNCE_WITHHELD",
                reason = reason.as_str(),
                peer = %peer_hex8(&peer),
                iface = iface.0,
            );
            leviculum_core::transport::TickOutput::default()
        }
    }
}

/// Hand an interface's configured bitrate and announce cap to the core.
///
/// Split out of the interface-registration loop because the ordering is a
/// contract, not a formatting choice, and a contract with no test is a comment:
/// `register_interface_bitrate` (re)creates the cap entry at the registration
/// default, so a cap applied before it is silently discarded — the shape of the
/// Codeberg #92 bug this fixed.
///
/// - `bitrate` (Codeberg #93): a key that cleared `MINIMUM_BITRATE` overrides
///   the medium default and feeds announce bandwidth capping / timing, where
///   Python applies `configured_bitrate` (Reticulum.py:887, Transport.py:1257).
///   Media-agnostic: transport only sees bits per second.
/// - `announce_cap` (Codeberg #92, Reticulum.py:713-716, 774): the setter
///   reports `false` when the interface has no cap entry at all — with no
///   configured bitrate there is nothing to take a share of, so say that rather
///   than dropping the key silently.
fn apply_bitrate_and_announce_cap(core: &mut StdNodeCore, idx: usize, config: &InterfaceConfig) {
    if let Some(bitrate) = config.bitrate {
        let bps = bitrate.min(u32::MAX as u64) as u32;
        core.register_interface_bitrate(idx, bps);
        tracing::info!("Interface {} configured bitrate: {} bps", idx, bps);
    }
    if let Some(cap_percent) = announce_cap_percent_from_config(config) {
        if core.set_interface_announce_cap(idx, cap_percent) {
            tracing::info!("Interface {} announce cap: {}%", idx, cap_percent);
        } else {
            tracing::warn!(
                "Interface {} announce_cap ignored: the key needs a \
                 configured bitrate to take a share of",
                idx
            );
        }
    }
}

/// Channels consumed by the event loop.
struct EventLoopChannels {
    /// Split control/data application event sink. `None` when the node was
    /// built with `without_events()`; in that case `dispatch_output` skips
    /// event-forwarding and `output.events` falls out of scope, exactly
    /// like the `leviculum-nrf` daemon binaries.
    event_sink: Option<EventSink>,
    action_dispatch_rx: mpsc::Receiver<TickOutput>,
    new_interface_rx: mpsc::Receiver<InterfaceHandle>,
    reconnect_rx: mpsc::Receiver<InterfaceId>,
    /// Tunnel-synthesize initiation signal (Codeberg #64). A tunnel-capable TCP
    /// client fires its id here on every connect; the loop initiates the
    /// synthesize handshake toward the peer.
    tunnel_notify_rx: mpsc::Receiver<InterfaceId>,
    /// Runtime interface-removal requests. An id fired here is torn down through
    /// the same path as a channel-close disconnect (see [`recv_any`]).
    remove_iface_rx: mpsc::Receiver<InterfaceId>,
    /// Per-peer transition reports from multi-peer interfaces (Codeberg
    /// #365): `Lost` when a peer's last link on that broadcast domain died
    /// (the loop culls the paths via that peer — the per-peer analog of
    /// the `Disconnected` cleanup), `Up` when a peer's first link came up
    /// (the loop counts it into the core's peer mirror and runs the
    /// opt-in peer-up pull). Currently fed by the BLE
    /// interface only; see [`crate::interfaces::PeerEvent`] for why both
    /// share one ordered channel.
    peer_event_rx: mpsc::Receiver<(InterfaceId, crate::interfaces::PeerEvent)>,
    shutdown: watch::Receiver<bool>,
}

/// Runtime auto-connect wiring handed to the event loop (Codeberg #32).
///
/// Bundles the interface-id allocator and registration channels the loop's
/// auto-connect poll uses to spawn discovered TCP endpoints at runtime, so a
/// discovered [`AutoConnectManager`](crate::autoconnect::AutoConnectManager)
/// registers interfaces through the exact same path as the static and
/// hot-plug interfaces.
struct AutoConnectWiring {
    /// Auto-connect cap; `0` leaves the feature disabled.
    max: usize,
    new_iface_tx: mpsc::Sender<InterfaceHandle>,
    reconnect_tx: mpsc::Sender<InterfaceId>,
    next_id: Arc<AtomicUsize>,
    corrupt_every: Option<u64>,
    outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
}

/// One periodic self-advertise job (Codeberg #107): a discoverable interface's
/// pre-stamped discovery announce `app_data` and its cadence. The `app_data` is
/// built once at start (PoW stamp + optional network-identity encryption via
/// [`build_announce_app_data`](leviculum_core::discovery::build_announce_app_data))
/// so the announcer arm never runs a proof-of-work stamp on the event loop.
struct DiscoveryAnnounceJob {
    /// Ready-to-announce `flags + msgpack(info) + stamp` payload.
    app_data: Vec<u8>,
    /// Minimum spacing between this interface's announces.
    interval: Duration,
    /// When this interface last self-advertised; `None` until the first emit.
    last_announce: Option<tokio::time::Instant>,
    /// Interface name, for the announce log line.
    label: String,
}

/// Producer-side discovery wiring (Codeberg #107): the registered discovery
/// destination and one announce job per discoverable interface, driven on the
/// `job_interval` cadence. `None` when no interface is `discoverable`.
struct DiscoveryAnnounceWiring {
    /// The `rnstransport.discovery.interface` destination, keyed by the network
    /// identity (encrypted network) or the node identity (plaintext).
    dest_hash: leviculum_core::DestinationHash,
    /// Announcer job interval (Python `InterfaceAnnouncer.JOB_INTERVAL`).
    job_interval: Duration,
    /// One job per discoverable interface, most-overdue picked each tick.
    jobs: Vec<DiscoveryAnnounceJob>,
}

/// Event received from any interface
enum RecvEvent {
    /// A complete packet from an interface
    Packet(InterfaceId, IncomingPacket),
    /// An interface disconnected (its incoming channel closed)
    Disconnected(InterfaceId),
}

/// Reason a `wait_for_interface_ready` call did not return `Ok(())`.
///
/// Returned by [`ReticulumNode::wait_for_interface_ready`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterfaceReadyError {
    /// The interface index did not match any registered interface.
    Unknown { idx: usize },
    /// The readiness deadline elapsed before the interface signalled
    /// ready.
    TimedOut { idx: usize },
    /// `start()` has not been called yet, so no interfaces are
    /// registered.
    NotStarted,
}

impl std::fmt::Display for InterfaceReadyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterfaceReadyError::Unknown { idx } => write!(f, "unknown interface index {idx}"),
            InterfaceReadyError::TimedOut { idx } => {
                write!(f, "interface {idx} did not become ready in time")
            }
            InterfaceReadyError::NotStarted => write!(f, "node not started"),
        }
    }
}

impl std::error::Error for InterfaceReadyError {}

/// Per-interface readiness state reported by
/// [`ReticulumNode::wait_for_interfaces_ready`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyState {
    /// The interface did not signal ready before the shared deadline.
    TimedOut,
    /// `start()` had not been called when the wait began.
    NotStarted,
}

/// Read-only snapshot of one interface, for diagnostics. Joins the core's name
/// and online status with the byte counters from the I/O tasks.
#[derive(Debug, Clone)]
pub struct InterfaceStatusSnapshot {
    /// The interface id assigned by the node — the same id a runtime handle
    /// reports (see
    /// [`RNodeChannelHandle::id`](crate::interfaces::rnode::RNodeChannelHandle::id)),
    /// so a caller can pair its own handle up with the snapshot instead of
    /// matching on the name.
    pub interface_id: leviculum_core::transport::InterfaceId,
    /// Human-readable interface name.
    pub name: String,
    /// Whether this is a local IPC client interface (shared-instance client).
    pub is_local_client: bool,
    /// Whether the interface is currently online.
    pub online: bool,
    /// Bytes received on this interface.
    pub rx_bytes: u64,
    /// Bytes transmitted on this interface.
    pub tx_bytes: u64,
    /// Ingress-limited announces currently held for later release on this
    /// interface (Codeberg #87; Python len(Interface.held_announces)).
    pub held_announces: usize,
    /// Whether the announce ingress burst limiter is currently active (Codeberg
    /// #87; Python ic_burst_active).
    pub burst_active: bool,
    /// Effective configured bitrate in bits per second (Codeberg #93), or `None`
    /// when the interface has no configured bitrate and reporting falls back to
    /// the medium default guess.
    pub configured_bitrate: Option<u32>,
    /// Transport medium the interface runs over (TCP, UDP, I2P, LoRa, …), so a
    /// status consumer can group by transport rather than by the peer-label name.
    pub kind: leviculum_core::traits::InterfaceKind,
}

/// Aggregated AutoInterface peer count across every configured section.
///
/// Each `[[AutoInterface]]` section spawns its own orchestrator (its own
/// `group_id`, multicast address and ports), and each publishes its live peer
/// count over a `watch` channel. This holds one receiver per section and sums
/// them on demand, so `peers` in `rnstatus` reflects all discovery domains
/// rather than only the last section to be initialised. The receiver list is
/// shared behind an `Arc<Mutex>` so a clone handed to the RPC server also sees
/// sections added at runtime.
#[derive(Clone, Default)]
pub(crate) struct AutoPeerCount {
    receivers: Arc<Mutex<Vec<watch::Receiver<usize>>>>,
}

impl AutoPeerCount {
    /// Register another section's peer-count receiver.
    fn push(&self, rx: watch::Receiver<usize>) {
        self.receivers.lock_recover().push(rx);
    }

    /// Sum the current peer count across all AutoInterface sections.
    pub(crate) fn total(&self) -> usize {
        self.receivers
            .lock_recover()
            .iter()
            .map(|rx| *rx.borrow())
            .sum()
    }
}

/// Reject configs where two `[[AutoInterface]]` sections share a unicast
/// discovery port or a data port (Codeberg #7).
///
/// Distinct `group_id`s already isolate multicast discovery (distinct multicast
/// addresses), so N sections can coexist as separate discovery domains. But the
/// unicast discovery socket (`discovery_port + 1`) and the data socket bind the
/// NIC's link-local address, not the multicast address; two sections reusing
/// either port bind the same `(link-local, port)` with `SO_REUSEPORT`, and the
/// kernel then load-balances incoming unicast/data datagrams between the two
/// orchestrators, silently splitting traffic. Fail fast with a clear message
/// instead. Disabled sections are ignored.
fn validate_auto_interface_ports(interfaces: &[InterfaceConfig]) -> Result<(), Error> {
    use crate::interfaces::auto_interface::{
        unicast_discovery_port, DEFAULT_DATA_PORT, DEFAULT_DISCOVERY_PORT,
    };

    let mut seen_discovery: std::collections::HashSet<u16> = std::collections::HashSet::new();
    let mut seen_data: std::collections::HashSet<u16> = std::collections::HashSet::new();

    for config in interfaces
        .iter()
        .filter(|c| c.enabled && c.interface_type == "AutoInterface")
    {
        let discovery_port = config.discovery_port.unwrap_or(DEFAULT_DISCOVERY_PORT);
        let data_port = config.data_port.unwrap_or(DEFAULT_DATA_PORT);

        if !seen_discovery.insert(discovery_port) {
            return Err(Error::Config(format!(
                "AutoInterface: discovery_port {} is used by more than one AutoInterface \
                 section; each section needs a distinct discovery_port (its unicast port {} \
                 would otherwise be split between sections by SO_REUSEPORT)",
                discovery_port,
                unicast_discovery_port(discovery_port),
            )));
        }
        if !seen_data.insert(data_port) {
            return Err(Error::Config(format!(
                "AutoInterface: data_port {} is used by more than one AutoInterface section; \
                 each section needs a distinct data_port (it would otherwise be split between \
                 sections by SO_REUSEPORT)",
                data_port,
            )));
        }
    }

    Ok(())
}

/// High-level async Reticulum node
///
/// `ReticulumNode` provides an async API for interacting with the Reticulum
/// network. It manages the internal event loop and provides methods for sending
/// data, establishing links, and handling incoming messages.
pub struct ReticulumNode {
    /// Handle to the core node
    inner: Arc<Mutex<StdNodeCore>>,
    /// Interface configurations
    interfaces: Vec<InterfaceConfig>,
    /// Channel-backed RNode interfaces (host-supplied byte channels), spawned
    /// alongside the file-config interfaces in `initialize_interfaces`.
    rnode_channels: Vec<crate::interfaces::rnode::RNodeChannelConfig>,
    /// Control-plane event sender, cloned into the runner's `EventSink`.
    /// `None` when built with `without_events()` (daemon-mode); the loop
    /// then never forwards `NodeEvent`s. Kept here so the channel stays open.
    control_tx: Option<mpsc::Sender<NodeEvent>>,
    /// Data-plane event sender, cloned into the runner's `EventSink`.
    data_tx: Option<mpsc::Sender<NodeEvent>>,
    /// Capacity of the control channel, needed to build the runner's
    /// `EventSink` (used for the overflow warn log).
    control_channel_capacity: usize,
    /// Merged event receiver for consuming events. `None` either because the
    /// node was built with `without_events()`, or because
    /// `take_event_receiver()` already handed it out.
    event_rx: Option<EventReceiver>,
    /// Shutdown sender
    shutdown_tx: Option<watch::Sender<bool>>,
    /// Runner task handle
    runner_handle: Option<tokio::task::JoinHandle<()>>,
    /// Channel for dispatching TickOutput from outside the event loop
    /// (used by connect, send_on_link, close_link, announce)
    action_dispatch_tx: mpsc::Sender<TickOutput>,
    /// Fault injection: corrupt ~1 byte per N bytes on TCP write
    corrupt_every: Option<u64>,
    /// Outbound-socket hook applied to each TCP client's connect socket.
    outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
    /// Interval between periodic storage flushes (seconds).
    /// Crash protection only, normal shutdown calls flush() via signal handler.
    /// Lost data from a crash is recovered via fresh announces.
    flush_interval_secs: u64,
    /// Aggregated peer count across all AutoInterface sections (if any
    /// configured). Empty when no AutoInterface is present.
    auto_peer_count: AutoPeerCount,
    /// Shared instance name (if enabled). When Some, the daemon listens on
    /// abstract Unix socket `\0rns/{name}` for local IPC clients.
    share_instance_name: Option<String>,
    /// Shared instance to connect to as client. When Some, the node connects
    /// to abstract Unix socket `\0rns/{name}` instead of starting its own
    /// interfaces from config.
    connect_instance_name: Option<String>,
    /// Time when the node was created (for RPC uptime reporting).
    start_time: std::time::Instant,
    /// Shared interface I/O counters, populated by the event loop.
    iface_stats_map: InterfaceStatsMap,
    /// Reporting-side interface inventory (Codeberg #177): the listeners this
    /// daemon runs, plus the reference display identity of the connections
    /// they spawn. Transport holds only interfaces it can route packets on,
    /// so listeners live here and nowhere else; `interface_stats` reports the
    /// union of the two, which is the collection a Python `rnsd` reports.
    inventory: crate::interfaces::inventory::SharedInventory,
    /// Live shared-instance client count, shared with the local accept loop.
    /// The reference labels an accepted IPC client with this count at accept
    /// time (LocalInterface.py:441/355).
    local_client_count: Arc<AtomicUsize>,
    /// Per-interface readiness signals, keyed by interface index.
    /// Populated by `start()` once interfaces are spawned.  Read by
    /// [`wait_for_interface_ready`](Self::wait_for_interface_ready)
    /// and [`wait_for_interfaces_ready`](Self::wait_for_interfaces_ready).
    iface_ready_map: crate::interfaces::InterfaceReadyMap,
    /// Dedicated, time-enabled runtime that hosts the event loop and every
    /// interface task. Owning our own runtime means the node works regardless
    /// of how the *embedding* application built its runtime — e.g. a PyO3 host
    /// that constructed a current-thread runtime without `enable_time()`, which
    /// previously panicked the timer-driven event loop (`sleep_until`) and the
    /// interface timers. Torn down via `shutdown_background()` in `Drop` so the
    /// runtime is never dropped blocking inside a host async context.
    runtime: Option<tokio::runtime::Runtime>,
    /// Runtime interface-registration sender, cloned from the channel the event
    /// loop consumes. `Some` once `start()` has run. Lets the node attach a
    /// fresh interface (e.g. a hot-plugged RNode radio) while running. See
    /// [`spawn_rnode_channel_interface`](Self::spawn_rnode_channel_interface).
    new_iface_tx: Option<mpsc::Sender<InterfaceHandle>>,
    /// Shared monotonic interface-id allocator (same counter the event loop and
    /// `initialize_interfaces` use, so runtime ids never collide). `Some` once
    /// `start()` has run.
    iface_id_counter: Option<Arc<AtomicUsize>>,
    /// Reconnect-notify sender handed to runtime-attached interfaces so their
    /// re-announce-on-recovery works like config interfaces. `Some` after
    /// `start()`.
    reconnect_tx: Option<mpsc::Sender<InterfaceId>>,
    /// Tunnel-synthesize notify sender handed to runtime-attached TCP clients so
    /// they initiate the synthesize handshake like config interfaces (Codeberg
    /// #64). `Some` after `start()`.
    tunnel_notify_tx: Option<mpsc::Sender<InterfaceId>>,
    /// Peer-transition sender handed to runtime-attached multi-peer
    /// interfaces (BLE) so their per-peer path culling and peer-up pull
    /// work like config interfaces (Codeberg #365). `Some` after `start()`.
    peer_event_tx: Option<mpsc::Sender<(InterfaceId, crate::interfaces::PeerEvent)>>,
    /// Runtime interface-removal sender. [`remove_interface`](Self::remove_interface)
    /// fires an id here; the event loop tears the interface down through the
    /// same path as a channel-close disconnect. `Some` after `start()`.
    remove_iface_tx: Option<mpsc::Sender<InterfaceId>>,
    /// Storage directory, needed by interface types that persist per-interface
    /// state (currently only `I2PInterface`, which stores its SAM destination
    /// private key so its `.b32.i2p` address survives restarts). Set by the
    /// builder; `None` falls back to the default config dir.
    storage_path: Option<PathBuf>,
    /// Runtime auto-connect cap (Codeberg #32, sub-task b). `0` disables
    /// auto-connect of discovered interfaces; `N > 0` enables it capped at `N`.
    /// Set by the builder.
    autoconnect_max: usize,
    /// Network identity for a private (encrypted) discovery network (Codeberg
    /// #32, sub-task d). `Some` when `network_identity` is configured; the
    /// event loop uses it to decrypt encrypted discovery announces before
    /// stamp validation. `None` keeps the plaintext discovery path.
    discovery_network_identity: Option<Arc<leviculum_core::Identity>>,
    /// Discovery announcer job interval in seconds (Codeberg #107, Python
    /// `InterfaceAnnouncer.JOB_INTERVAL`). Each tick self-advertises the most-
    /// overdue discoverable interface. Set by the builder from config; lowered
    /// by fast tests. Default 60.
    discovery_job_interval_secs: u64,
    /// Registered in-driver core processor (Codeberg #196). Installed by the
    /// builder — i.e. before `start()` creates the real `action_dispatch_tx` —
    /// and moved into the event loop by `start()`.
    ///
    /// Behind a `Mutex` only to keep `ReticulumNode: Sync`, which consumers
    /// (lnomad shares one across tasks) depend on: a bare
    /// `Box<dyn CoreProcessor>` is `Send` but not `Sync`. Both accessors hold
    /// `&mut self`, so this never locks — requiring `Sync` of the processor
    /// itself would instead tax every single-threaded state machine that will
    /// ever be plugged in here, for nothing.
    core_processor: Mutex<Option<Box<dyn CoreProcessor>>>,
    /// Completion futures resolved at the dispatch layer (leviculum#42).
    /// Arc-shared with the event loop like `iface_stats_map`; a LEAF lock,
    /// never taken while the node lock is held (upstream #199: no new
    /// lock-taking wait paths).
    completions: Arc<CompletionRegistry>,
}

impl Drop for ReticulumNode {
    fn drop(&mut self) {
        // Codeberg #77: give work queued right before drop (e.g. a responder
        // `close_link`) a bounded chance to flush before the runtime is torn
        // down. Signal the event loop to shut down — it drains
        // action_dispatch_rx, dispatches the queued outputs to the interfaces,
        // and waits for the interface tasks to flush them to the socket (see
        // `run_event_loop` Branch 4) before returning. We then wait a bounded
        // wall-clock window for the runner to finish on the node's own worker
        // thread. The wait POLLS the join handle rather than awaiting it: Drop
        // may run inside another runtime's async context (a PyO3 host dropping
        // the node from one of its tasks), where a blocking await would panic.
        // The node owns a separate worker thread, so the polling sleep here does
        // not stall its event loop / interface tasks.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.runner_handle.take() {
            let deadline = std::time::Instant::now() + DROP_FLUSH_BOUND;
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(SHUTDOWN_FLUSH_POLL);
            }
        }
        // Tear the node's runtime down without blocking. Dropping a tokio
        // `Runtime` directly performs a blocking shutdown, which panics if the
        // drop happens inside another runtime's async context (e.g. the PyO3
        // host dropping the node from one of its own tasks).
        // `shutdown_background` aborts the event loop + interface tasks and
        // returns immediately.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl ReticulumNode {
    /// Create a new ReticulumNode (internal use - use ReticulumNodeBuilder)
    pub(crate) fn new(
        core: StdNodeCore,
        interfaces: Vec<InterfaceConfig>,
        corrupt_every: Option<u64>,
        events_enabled: bool,
        flush_interval_secs: u64,
        control_channel_capacity: usize,
        data_channel_capacity: usize,
    ) -> Self {
        // When events are disabled (daemon-mode), no channels are constructed
        // at all — neither senders nor receiver. The event loop's
        // `dispatch_output` then skips its event-forwarding branch and
        // `output.events` falls out of scope unread, mirroring the NRF
        // daemon binaries.
        //
        // Codeberg #71: the single bounded channel is split into a lossless
        // control plane and a droppable data plane, merged back for the
        // application by `EventReceiver`.
        let (control_tx, data_tx, event_rx) = if events_enabled {
            let (control_tx, control_rx) = mpsc::channel(control_channel_capacity);
            let (data_tx, data_rx) = mpsc::channel(data_channel_capacity);
            (
                Some(control_tx),
                Some(data_tx),
                Some(EventReceiver {
                    control: control_rx,
                    data: data_rx,
                }),
            )
        } else {
            (None, None, None)
        };
        // Create dummy channel; real one is created in start()
        let (action_dispatch_tx, _) = mpsc::channel(1);

        Self {
            inner: Arc::new(Mutex::new(core)),
            interfaces,
            rnode_channels: Vec::new(),
            control_tx,
            data_tx,
            control_channel_capacity,
            event_rx,
            shutdown_tx: None,
            runner_handle: None,
            action_dispatch_tx,
            corrupt_every,
            outbound_socket_hook: None,
            flush_interval_secs,
            auto_peer_count: AutoPeerCount::default(),
            share_instance_name: None,
            connect_instance_name: None,
            start_time: std::time::Instant::now(),
            iface_stats_map: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            inventory: crate::interfaces::inventory::InterfaceInventory::shared(),
            local_client_count: Arc::new(AtomicUsize::new(0)),
            iface_ready_map: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
            runtime: None,
            new_iface_tx: None,
            iface_id_counter: None,
            reconnect_tx: None,
            tunnel_notify_tx: None,
            peer_event_tx: None,
            remove_iface_tx: None,
            storage_path: None,
            autoconnect_max: 0,
            discovery_network_identity: None,
            discovery_job_interval_secs: crate::config::DEFAULT_DISCOVERY_JOB_INTERVAL_SECS,
            core_processor: Mutex::new(None),
            completions: CompletionRegistry::new(),
        }
    }

    /// Set the storage directory (called by the builder). Used by interface
    /// types that persist per-interface state under `<storage>/i2p/`.
    pub(crate) fn set_storage_path(&mut self, path: PathBuf) {
        self.storage_path = Some(path);
    }

    /// Install the in-driver core processor (called by the builder, Codeberg
    /// #196). Deliberately not public and deliberately not callable after
    /// `start()`: registering before the event loop exists is what guarantees
    /// no live `action_dispatch_tx` can be captured into the processor.
    pub(crate) fn set_core_processor(&mut self, processor: Box<dyn CoreProcessor>) {
        *self
            .core_processor
            .get_mut()
            .unwrap_or_else(|e| e.into_inner()) = Some(processor);
    }

    /// Set the runtime auto-connect cap (called by the builder, Codeberg #32).
    pub(crate) fn set_autoconnect_max(&mut self, max: usize) {
        self.autoconnect_max = max;
    }

    /// Set the discovery network identity (called by the builder, Codeberg #32
    /// sub-task d). `Some` enables decrypt-on-receive for a private discovery
    /// network; `None` keeps the plaintext path.
    pub(crate) fn set_discovery_network_identity(
        &mut self,
        identity: Option<Arc<leviculum_core::Identity>>,
    ) {
        self.discovery_network_identity = identity;
    }

    /// Set the discovery announcer job interval in seconds (called by the
    /// builder, Codeberg #107). Python `InterfaceAnnouncer.JOB_INTERVAL`.
    pub(crate) fn set_discovery_job_interval_secs(&mut self, secs: u64) {
        self.discovery_job_interval_secs = secs;
    }

    /// The storage root under which the discovered-interface registry lives
    /// (`<storage>/discovery/interfaces`). Falls back to the default config
    /// dir's `storage` when no explicit path was configured, matching the
    /// resolution used elsewhere in the run loop.
    pub(crate) fn discovery_storage_root(&self) -> PathBuf {
        self.storage_path
            .clone()
            .unwrap_or_else(|| crate::config::Config::default_config_dir().join("storage"))
    }

    /// Build the producer-side discovery wiring (Codeberg #107): register the
    /// `rnstransport.discovery.interface` destination and pre-stamp one announce
    /// job per `discoverable` interface.
    ///
    /// Returns `None` when no enabled interface is `discoverable`. The discovery
    /// destination is owned by the network identity on an encrypted network,
    /// else by the node identity (Python `Discovery.py` `InterfaceAnnouncer`).
    /// Each job's `app_data` is stamped (and optionally network-identity
    /// encrypted) once here, reusing
    /// [`build_announce_app_data`](leviculum_core::discovery::build_announce_app_data),
    /// so the announcer arm never runs proof-of-work on the event loop.
    fn build_discovery_announce_wiring(&self) -> Option<DiscoveryAnnounceWiring> {
        let discoverable: Vec<(
            &InterfaceConfig,
            leviculum_core::discovery::InterfaceDescriptor,
        )> = self
            .interfaces
            .iter()
            .filter(|c| c.enabled && c.discoverable)
            .filter_map(|c| crate::discovery::descriptor_from_config(c).map(|d| (c, d)))
            .collect();
        if discoverable.is_empty() {
            return None;
        }

        let network_identity = self.discovery_network_identity.clone();

        // Register the discovery destination, keyed by the network identity
        // (encrypted network) or the node identity (plaintext). Snapshot the
        // node's transport identity hash + transport-enabled flag for the
        // descriptors while the lock is held.
        let (dest_hash, transport_id, transport_enabled) = {
            let mut core = self.inner.lock_recover();
            let transport_enabled = core.transport_config().enable_transport;
            let transport_id: [u8; TRUNCATED_HASHBYTES] = *core.identity().hash();
            let dest_identity: leviculum_core::Identity = match &network_identity {
                Some(id) => (**id).clone(),
                None => core.identity().clone(),
            };
            let dest = match Destination::new(
                Some(dest_identity),
                leviculum_core::Direction::In,
                leviculum_core::DestinationType::Single,
                leviculum_core::discovery::APP_NAME,
                &leviculum_core::discovery::DISCOVERY_ASPECTS,
            ) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("discovery: failed to build discovery destination: {e:?}");
                    return None;
                }
            };
            let dest_hash = *dest.hash();
            core.register_destination(dest);
            (dest_hash, transport_id, transport_enabled)
        };

        let mut rng = rand_core::OsRng;
        let mut jobs = Vec::new();
        for (cfg, desc) in discoverable {
            let interval =
                Duration::from_secs(crate::discovery::resolve_announce_interval_secs(cfg));
            let app_data = if cfg.discovery_encrypt {
                match &network_identity {
                    Some(id) => leviculum_core::discovery::build_announce_app_data_encrypted(
                        &desc,
                        &transport_id,
                        transport_enabled,
                        id,
                        &mut rng,
                    ),
                    None => {
                        tracing::error!(
                            "discovery: interface {:?} requests discovery_encrypt but no \
                             network_identity is configured, skipping",
                            desc.name
                        );
                        None
                    }
                }
            } else {
                leviculum_core::discovery::build_announce_app_data(
                    &desc,
                    &transport_id,
                    transport_enabled,
                    &mut rng,
                )
            };
            let Some(app_data) = app_data else {
                tracing::warn!(
                    "discovery: could not build announce data for {} interface, skipping",
                    desc.interface_type
                );
                continue;
            };
            let label = desc
                .name
                .clone()
                .unwrap_or_else(|| desc.interface_type.clone());
            jobs.push(DiscoveryAnnounceJob {
                app_data,
                interval,
                last_announce: None,
                label,
            });
        }
        if jobs.is_empty() {
            return None;
        }

        tracing::info!(
            "discovery: self-advertising {} interface(s) on {} every {}s",
            jobs.len(),
            leviculum_core::discovery::DISCOVERY_ASPECT_FILTER,
            self.discovery_job_interval_secs.max(1),
        );
        Some(DiscoveryAnnounceWiring {
            dest_hash,
            job_interval: Duration::from_secs(self.discovery_job_interval_secs.max(1)),
            jobs,
        })
    }

    /// Start the node
    ///
    /// This spawns the internal event loop and initializes interfaces.
    /// The node will process incoming packets and emit events until `stop()` is called.
    pub async fn start(&mut self) -> Result<(), Error> {
        if self.runner_handle.is_some() {
            return Err(Error::Config("node already running".to_string()));
        }

        // Build a dedicated, time-enabled runtime to host the event loop and
        // all interface tasks. Entering it here routes every `tokio::spawn`
        // performed by the rest of `start()` — and transitively the child tasks
        // those spawn — onto this runtime, so the timer-driven event loop and
        // interface timers work even when the *embedding* runtime was built
        // without `enable_time()` (the PyO3/edge case that panicked at
        // `sleep_until`). `start()`'s body is synchronous up to the spawns, so
        // holding the enter guard across it (no await) is sound.
        //
        // Single worker thread: the node's work is async-I/O bound (network +
        // light per-packet crypto), so one cooperatively-scheduled worker is
        // sufficient, and it keeps the node from adding `num_cpus` threads on
        // top of an embedding host's own runtime — that oversubscription, plus
        // the genuine parallelism a multi-worker pool introduced between the
        // event loop and the public API, is the kind of thing that surfaces
        // latent ordering races in a cohabiting host.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("reticulum-node")
            .thread_stack_size(128 * 1024)
            .build()
            .map_err(|e| Error::Config(format!("failed to build node runtime: {e}")))?;
        let enter_guard = runtime.enter();

        // Shared monotonic counter for interface IDs.
        // Initialized at interfaces.len() so static and dynamic IDs never collide.
        let next_id = Arc::new(AtomicUsize::new(self.interfaces.len()));

        // Channel for dynamically registering interfaces (e.g. from TCP server accept loop)
        let (new_iface_tx, new_iface_rx) = mpsc::channel::<InterfaceHandle>(32);

        // Channel for TCP client reconnection notifications (Block D).
        // When a reconnecting TCP client re-establishes its connection, it sends
        // its InterfaceId here so the event loop can call handle_interface_up()
        // to re-announce destinations on the recovered link.
        let (reconnect_tx, reconnect_rx) = mpsc::channel::<InterfaceId>(16);

        // Channel for tunnel-synthesize initiation (Codeberg #64 initiator side).
        // A tunnel-capable TCP client fires its InterfaceId here on every
        // successful connect (initial AND reconnect); the event loop then calls
        // core.send_tunnel_synthesize() to initiate the handshake toward the peer.
        let (tunnel_notify_tx, tunnel_notify_rx) = mpsc::channel::<InterfaceId>(16);

        // Runtime interface removal: `remove_interface` fires an id here and the
        // event loop tears it down through the shared disconnect path.
        let (remove_iface_tx, remove_iface_rx) = mpsc::channel::<InterfaceId>(16);

        // Per-peer transition reports from multi-peer interfaces (Codeberg
        // #365). The BLE interface fires (id, Lost/Up) here when a peer's
        // last link died or its first link came up; the event loop culls
        // the paths via a lost peer, mirrors the live peer count into the
        // core and reports a fresh peer's arrival.
        let (peer_event_tx, peer_event_rx) =
            mpsc::channel::<(InterfaceId, crate::interfaces::PeerEvent)>(16);

        // Retain clones so the node can attach and detach interfaces at runtime
        // (hot-plug), not just at construction. Used by `spawn_interface` /
        // `remove_interface` and `spawn_rnode_channel_interface`.
        self.new_iface_tx = Some(new_iface_tx.clone());
        self.iface_id_counter = Some(Arc::clone(&next_id));
        self.reconnect_tx = Some(reconnect_tx.clone());
        self.tunnel_notify_tx = Some(tunnel_notify_tx.clone());
        self.remove_iface_tx = Some(remove_iface_tx);
        self.peer_event_tx = Some(peer_event_tx.clone());

        // Initialize interfaces, the driver owns them, NOT NodeCore.
        // Interface init is the one fallible step after the runtime exists
        // (e.g. a TCPServerInterface bind failure). On error, tear the runtime
        // down with shutdown_background() before propagating — a bare `?` would
        // drop the live Runtime here, and a blocking Runtime drop inside the
        // caller's async context panics, masking the real interface error.
        let registry = match self.initialize_interfaces(
            &next_id,
            &new_iface_tx,
            &reconnect_tx,
            &tunnel_notify_tx,
            &peer_event_tx,
        ) {
            Ok(registry) => registry,
            Err(e) => {
                drop(enter_guard);
                runtime.shutdown_background();
                return Err(e);
            }
        };

        {
            let mut core = self.inner.lock_recover();

            // Register human-readable interface names, HW_MTU, and counters with core
            {
                let mut stats = self.iface_stats_map.lock_recover();
                let mut ready = self.iface_ready_map.lock_recover();
                for handle in registry.handles() {
                    core.set_interface_name(handle.info.id.0, handle.info.name.clone());
                    if let Some(hw_mtu) = handle.info.hw_mtu {
                        core.set_interface_hw_mtu(handle.info.id.0, hw_mtu);
                    }
                    if let Some(bitrate) = handle.info.bitrate {
                        tracing::info!("Interface {} bitrate: {} bps", handle.info.name, bitrate);
                        // Read-only backchannel (no scheduling reads it): what
                        // this interface says about its own medium, so a caller
                        // sizing a delivery window can ask instead of assuming.
                        core.register_interface_link_profile(
                            handle.info.id.0,
                            leviculum_core::transport::LinkProfile {
                                bitrate_bps: bitrate,
                                tx_jitter_max_ms: handle.info.tx_jitter_max_ms,
                            },
                        );
                    }
                    stats.insert(handle.info.id.0, Arc::clone(&handle.counters));
                    ready.insert(handle.info.id.0, Arc::clone(&handle.ready));
                }
            }

            // Register IFAC configurations for static interfaces (TCP client, UDP, RNode).
            // TCPServerInterface IFAC is handled via spawn_tcp_server → InterfaceInfo.ifac,
            // because the server listener itself doesn't register as an interface, only
            // accepted connections do, and they get dynamic interface IDs.
            for (idx, iface_config) in self.interfaces.iter().enumerate() {
                if !iface_config.enabled {
                    continue;
                }
                if iface_config.interface_type == "TCPServerInterface" {
                    continue; // IFAC passed to spawn_tcp_server in initialize_interfaces
                }
                // Tunnel-capable interfaces (Codeberg #64 initiator side): a
                // static TCP client registers a stable, peer-opaque interface
                // hash so it can initiate the synthesize handshake on connect and
                // reconnect. The hash is derived from the interface's stable name
                // (mirrors Python `interface.get_hash() = full_hash(str(self))`);
                // it only needs to stay constant across the interface's
                // reconnects so the derived tunnel id is stable. The medium
                // decision ("a non-KISS TCP client wants a tunnel") lives here in
                // the driver; transport treats the hash as opaque bytes.
                if iface_config.interface_type == "TCPClientInterface" {
                    let iface_name = format!("tcp_client_{}", idx);
                    let interface_hash = leviculum_core::crypto::full_hash(iface_name.as_bytes());
                    core.register_tunnel_interface(idx, interface_hash);
                }
                if let Some(ifac) = build_ifac_config(iface_config) {
                    core.set_ifac_config(idx, ifac);
                    tracing::info!(
                        "IFAC enabled on interface {} (size={})",
                        idx,
                        iface_config
                            .ifac_size
                            .unwrap_or(leviculum_core::constants::IFAC_DEFAULT_SIZE_NETWORK)
                    );
                }
                // Announce-rate config (Codeberg #92): drives both status
                // reporting and per-destination rebroadcast rate limiting
                // (enforced per receiving interface in transport).
                if let Some(ar) = build_announce_rate_config(iface_config) {
                    core.set_announce_rate_config(idx, ar);
                }
                apply_bitrate_and_announce_cap(&mut core, idx, iface_config);
                // Transport medium, resolved from the configured interface type so
                // status can group by transport rather than by the peer-label name.
                core.set_interface_kind(
                    idx,
                    kind_from_interface_type(&iface_config.interface_type),
                );
                // Interface propagation mode (Codeberg #91). Resolve the config
                // string to an InterfaceMode and hand it to transport, which
                // owns the per-interface mode map and applies the propagation
                // rules. An unrecognised value logs and keeps the Full default,
                // matching Python (which leaves the mode unchanged on an
                // unknown string).
                if let Some(mode_str) = iface_config.mode.as_deref() {
                    match leviculum_core::traits::InterfaceMode::from_config_str(mode_str) {
                        Some(mode) => {
                            core.set_interface_mode(idx, mode);
                            if mode != leviculum_core::traits::InterfaceMode::Full {
                                tracing::info!("Interface {} mode: {}", idx, mode);
                            }
                        }
                        None => {
                            tracing::warn!(
                                "Interface {}: unknown mode '{}', using Full",
                                idx,
                                mode_str
                            );
                        }
                    }
                }
                // Ingress control (Codeberg #8). The interface's medium decides
                // the default (point-to-point off, shared/broadcast on); an
                // explicit config value overrides it. The driver (media-aware)
                // resolves the flag and hands it to transport, which owns the
                // per-interface map and stays interface-type agnostic.
                let ingress_on = iface_config.resolve_ingress_control();
                core.set_interface_ingress_control(idx, ingress_on);
                if !ingress_on {
                    tracing::info!("Interface {} ingress control: off", idx);
                }
                // Egress control (Codeberg #172). Off unless the operator sets
                // `egress_control`, matching the reference default
                // (`Interface.EGRESS_CONTROL = False`). No medium-class default
                // here: the reference has none either, and switching it on by
                // guess would silently drop path requests.
                let egress_on = iface_config.egress_control.unwrap_or(false);
                core.set_interface_egress_control(idx, egress_on);
                if egress_on {
                    tracing::info!("Interface {} egress control: on", idx);
                }
            }

            let transport_enabled = core.transport_config().enable_transport;
            let iface_count = self.interfaces.iter().filter(|c| c.enabled).count();
            tracing::info!(
                "Node started with {} interface(s), transport {}",
                iface_count,
                if transport_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        // Channel for dispatching TickOutput from outside the event loop.
        // connect(), send_on_link(), close_link(), and
        // announce_destination() produce actions that must reach the event loop
        // for interface dispatch.  Capacity 256 is generous, each call
        // produces exactly one TickOutput, and the event loop drains them on
        // every iteration, so the queue only backs up if the event loop is
        // blocked (which also stalls all other I/O).
        let (action_dispatch_tx, action_dispatch_rx) = mpsc::channel(ACTION_DISPATCH_CAPACITY);
        self.action_dispatch_tx = action_dispatch_tx;

        // Clone handles for the runner. The event loop owns a single
        // `EventSink` built from clones of both plane senders; it is the only
        // writer, so the dropped-counter needs no synchronisation.
        let inner = Arc::clone(&self.inner);
        let event_sink = match (self.control_tx.clone(), self.data_tx.clone()) {
            (Some(control_tx), Some(data_tx)) => Some(EventSink {
                control_tx,
                data_tx,
                control_capacity: self.control_channel_capacity,
                control_dropped: 0,
            }),
            // `without_events()` leaves both senders None.
            _ => None,
        };
        let iface_stats_map = Arc::clone(&self.iface_stats_map);
        let inventory = Arc::clone(&self.inventory);
        let local_client_count = Arc::clone(&self.local_client_count);
        let flush_interval_secs = self.flush_interval_secs;

        // Remote-management `/status` responder (Codeberg #86). Enabled when the
        // core created the `rnstransport.remote.management` destination at build
        // time. The event loop drives it even in daemon mode (no app event
        // sink), because it consumes `RequestReceived` from the raw
        // `TickOutput`, not the forwarded event stream.
        let remote_mgmt = self
            .inner
            .lock_recover()
            .remote_mgmt_dest_hash()
            .is_some()
            .then(|| {
                RemoteMgmtResponder::new(
                    Arc::clone(&self.iface_stats_map),
                    Arc::clone(&self.inventory),
                    self.start_time,
                    self.auto_peer_count.clone(),
                )
            });

        // Storage root for the discovered-interface registry: the event loop
        // persists validated discovery announces under
        // `<storage>/discovery/interfaces` (Codeberg #32).
        let discovery_storage = Some(self.discovery_storage_root());

        // Network identity for decrypting encrypted discovery announces on a
        // private discovery network (Codeberg #32, sub-task d). `None` keeps the
        // plaintext path.
        let discovery_network_identity = self.discovery_network_identity.clone();

        // Producer-side discovery (Codeberg #107): register the discovery
        // destination and pre-stamp one self-advertise job per discoverable
        // interface. `None` leaves the announcer arm dormant.
        let discovery_announce = self.build_discovery_announce_wiring();

        // In-driver core processor (Codeberg #196). Moved out of the node here:
        // it belongs to the event loop from now on, and a restart therefore
        // runs without it rather than with a half-owned one.
        let core_processor = self
            .core_processor
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        // Auto-connect wiring (Codeberg #32, sub-task b): the event loop spawns
        // discovered TCP endpoints at runtime through the same interface-id
        // allocator and registration channel the static/hot-plug paths use.
        let autoconnect_max = self.autoconnect_max;
        let autoconnect_new_iface_tx = new_iface_tx.clone();
        let autoconnect_reconnect_tx = reconnect_tx.clone();
        let autoconnect_next_id = Arc::clone(&next_id);
        let autoconnect_corrupt_every = self.corrupt_every;
        let autoconnect_socket_hook = self.outbound_socket_hook.clone();

        // A previous stop() closed the completion registry when its event loop
        // exited; this loop is about to observe events again, so registrations
        // must park rather than resolve NodeStopped.
        self.completions.reopen();
        let completions = Arc::clone(&self.completions);

        // Spawn the runner
        let runner_handle = tokio::spawn(async move {
            run_event_loop(
                inner,
                registry,
                EventLoopChannels {
                    event_sink,
                    action_dispatch_rx,
                    new_interface_rx: new_iface_rx,
                    reconnect_rx,
                    tunnel_notify_rx,
                    remove_iface_rx,
                    peer_event_rx,
                    shutdown: shutdown_rx,
                },
                iface_stats_map,
                inventory,
                local_client_count,
                flush_interval_secs,
                remote_mgmt,
                discovery_storage,
                discovery_network_identity,
                AutoConnectWiring {
                    max: autoconnect_max,
                    new_iface_tx: autoconnect_new_iface_tx,
                    reconnect_tx: autoconnect_reconnect_tx,
                    next_id: autoconnect_next_id,
                    corrupt_every: autoconnect_corrupt_every,
                    outbound_socket_hook: autoconnect_socket_hook,
                },
                discovery_announce,
                core_processor,
                completions,
            )
            .await;
        });

        self.runner_handle = Some(runner_handle);

        // Release the runtime context now that all tasks are spawned, then keep
        // the runtime alive in the node so its worker thread keeps driving them.
        drop(enter_guard);
        self.runtime = Some(runtime);

        Ok(())
    }

    /// Initialize interfaces from configuration
    ///
    /// Static interfaces (TCP clients) are connected and registered directly.
    /// Server listeners spawn accept loops that send new handles via `new_iface_tx`.
    fn initialize_interfaces(
        &mut self,
        next_id: &Arc<AtomicUsize>,
        new_iface_tx: &mpsc::Sender<InterfaceHandle>,
        reconnect_tx: &mpsc::Sender<InterfaceId>,
        tunnel_notify_tx: &mpsc::Sender<InterfaceId>,
        peer_event_tx: &mpsc::Sender<(InterfaceId, crate::interfaces::PeerEvent)>,
    ) -> Result<InterfaceRegistry, Error> {
        if self.share_instance_name.is_some() && self.connect_instance_name.is_some() {
            return Err(Error::Config(
                "cannot both share_instance and connect_to_shared_instance".to_string(),
            ));
        }

        let mut registry = InterfaceRegistry::new();
        let is_client_mode = self.connect_instance_name.is_some();

        // Only load config interfaces if NOT in shared-instance client mode.
        // Client mode routes everything through the daemon's Unix socket.
        if is_client_mode {
            tracing::info!("Shared instance client mode — skipping config interfaces");
        }

        if !is_client_mode {
            // Reject overlapping AutoInterface ports before spawning any
            // orchestrator (Codeberg #7). Different `group_id`s keep multicast
            // discovery isolated, but two sections sharing a unicast discovery
            // port or data port bind the same (link-local, port) with
            // SO_REUSEPORT and the kernel splits incoming datagrams between the
            // orchestrators, mis-delivering traffic.
            validate_auto_interface_ports(&self.interfaces)?;

            let build_ctx = interface_build::InterfaceBuildCtx {
                next_id,
                new_iface_tx,
                reconnect_tx,
                tunnel_notify_tx,
                peer_event_tx,
                corrupt_every: self.corrupt_every,
                storage_path: self.storage_path.clone(),
                outbound_socket_hook: self.outbound_socket_hook.clone(),
                inventory: Arc::clone(&self.inventory),
                transport_enabled: self.is_transport_enabled(),
                identity_hash: self.identity_hash(),
            };
            for (idx, config) in self.interfaces.iter().enumerate() {
                if !config.enabled {
                    continue;
                }
                match interface_build::build_interface(
                    idx,
                    config,
                    &build_ctx,
                    &self.auto_peer_count,
                )? {
                    interface_build::Built::Handles(handles) => {
                        for handle in handles {
                            registry.register(handle);
                        }
                    }
                    interface_build::Built::SelfManaged => {}
                }
            }

            // Channel-backed RNode interfaces (host-supplied byte channels:
            // phone USB/BLE). Same lifecycle as the serial RNode path; the
            // factory replaces the serial-port open. Ids continue past the
            // file-config interfaces via the shared `next_id` allocator.
            #[cfg(any())]
            for spec in std::mem::take(&mut self.rnode_channels) {
                let id = InterfaceId(next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                let iface_name = format!("rnode_channel_{}", id.0);
                let handle = crate::interfaces::rnode::spawn_rnode_channel_interface(
                    crate::interfaces::rnode::RNodeChannelInterfaceConfig {
                        id,
                        name: iface_name,
                        channel_factory: spec.factory,
                        frequency: spec.frequency,
                        bandwidth: spec.bandwidth,
                        tx_power: spec.tx_power,
                        sf: spec.sf,
                        cr: spec.cr,
                        st_alock: spec.st_alock,
                        lt_alock: spec.lt_alock,
                        flow_control: spec.flow_control,
                        buffer_size: spec.buffer_size,
                        reconnect_notify: Some(reconnect_tx.clone()),
                    },
                    None,
                );
                registry.register(handle);
            }
        } // end if !is_client_mode

        // Connect to shared instance daemon as client
        if let Some(ref instance_name) = self.connect_instance_name {
            let id = InterfaceId(next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            let handle = crate::interfaces::local::spawn_local_client(
                id,
                instance_name,
                crate::interfaces::local::LOCAL_DEFAULT_BUFFER_SIZE,
            )?;
            tracing::info!("Connected to shared instance '{}'", instance_name);
            // Mark this as the uplink to the shared instance so packets arriving
            // from the instance do not count the local IPC hop (Python's
            // interface_to_shared_instance branch, Transport.py:1484). Without
            // this the client's whole path table would read one hop too many.
            {
                let mut core = self.inner.lock_recover();
                core.set_interface_shared_instance(Some(id.0));
            }
            registry.register(handle);
        }

        // Start local (shared instance) server if enabled
        if let Some(ref instance_name) = self.share_instance_name {
            // The shared-instance server carries no packets either, so it
            // takes an id from the same allocator and lives in the reporting
            // inventory only (Codeberg #177). Allocated here, before any
            // client can connect, so it precedes every spawned interface.
            let server_id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::interfaces::local::spawn_local_server(
                instance_name,
                next_id.clone(),
                new_iface_tx.clone(),
                crate::interfaces::local::LOCAL_DEFAULT_BUFFER_SIZE,
                server_id,
                Arc::clone(&self.inventory),
                Arc::clone(&self.local_client_count),
            )
            .map_err(|e| match e.kind() {
                // A name collision is the one bind failure with a remedy
                // the operator can act on, and the only one a packaged
                // daemon hits routinely: installing on a host that
                // already runs a daemon under the same name. Left as a
                // bare io::Error it surfaces from lnsd's main as
                // `Io(Os { code: 98, kind: AddrInUse, .. })`, which names
                // neither the instance nor what to do about it.
                std::io::ErrorKind::AddrInUse => Error::SharedInstanceNameInUse {
                    name: instance_name.clone(),
                },
                _ => Error::Io(e),
            })?;

            // Start RPC server for Python CLI tool compatibility (rnstatus, rnpath, rnprobe)
            let authkey = {
                let core = self.inner.lock_recover();
                match core.identity().private_key_bytes() {
                    Ok(prv) => {
                        use sha2::Digest;
                        let hash = sha2::Sha256::digest(prv);
                        let mut key = [0u8; 32];
                        key.copy_from_slice(&hash);
                        key
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Cannot derive RPC authkey (no private key: {}), RPC server disabled",
                            e
                        );
                        return Ok(registry);
                    }
                }
            };
            #[cfg(feature = "rpc")]
            {
                if let Err(e) = crate::rpc::spawn_rpc_server(
                    instance_name,
                    Arc::clone(&self.inner),
                    authkey,
                    self.start_time,
                    Arc::clone(&self.iface_stats_map),
                    Arc::clone(&self.inventory),
                    self.auto_peer_count.clone(),
                    Some(self.discovery_storage_root()),
                ) {
                    tracing::warn!("Failed to start RPC server: {}", e);
                }
            }
        }

        // Spawn background traffic counter (matches Python Transport.count_traffic_loop)
        crate::interfaces::spawn_traffic_counter(Arc::clone(&self.iface_stats_map));

        Ok(registry)
    }

    /// Borrow the node's runtime if started.
    pub fn runtime(&self) -> Option<&tokio::runtime::Runtime> {
        self.runtime.as_ref()
    }

    /// Synchronous start: initializes runtime and background event loop.
    pub fn start_sync(&mut self) -> Result<(), Error> {
        let mut fut = Box::pin(self.start());
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Ready(res) => res,
            std::task::Poll::Pending => unreachable!("start() has no awaits"),
        }
    }

    /// Synchronous stop: signals shutdown, awaits runner, flushes storage, tears down runtime.
    pub fn stop_sync(&mut self) -> Result<(), Error> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        if let Some(handle) = self.runner_handle.take() {
            if let Some(rt) = &self.runtime {
                let _ = rt.block_on(handle);
            }
        }

        self.save_persistent_state();

        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }

        tracing::info!("ReticulumNode stopped");
        Ok(())
    }

    /// Stop the node
    pub async fn stop(&mut self) -> Result<(), Error> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        if let Some(handle) = self.runner_handle.take() {
            handle
                .await
                .map_err(|e| Error::Config(format!("runner panicked: {}", e)))?;
        }

        self.save_persistent_state();

        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }

        tracing::info!("ReticulumNode stopped");
        Ok(())
    }

    /// Persist all state to disk on shutdown.
    ///
    /// Delegates to `Storage::flush()` which saves known_destinations
    /// and packet_hashlist in Python-compatible formats.
    fn save_persistent_state(&self) {
        use leviculum_core::traits::Storage as _;
        let mut core = self.inner.lock_recover();
        core.storage_mut().flush();
    }

    /// Enable shared instance with the given instance name.
    ///
    /// Called by the builder when `share_instance = true`.
    pub(crate) fn set_share_instance(&mut self, name: String) {
        self.share_instance_name = Some(name);
    }

    /// Connect to a shared instance daemon as a client.
    ///
    /// Called by the builder when `connect_to_shared_instance` is set.
    pub(crate) fn set_connect_instance(&mut self, name: String) {
        self.connect_instance_name = Some(name);
    }

    /// Called by the builder to carry channel-backed RNode interfaces
    /// (host-supplied byte channels) into `initialize_interfaces`.
    pub(crate) fn set_rnode_channels(
        &mut self,
        specs: Vec<crate::interfaces::rnode::RNodeChannelConfig>,
    ) {
        self.rnode_channels = specs;
    }

    /// Check if the node is running
    pub fn is_running(&self) -> bool {
        self.runner_handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// Register a destination for incoming links
    pub fn register_destination(&self, destination: Destination) {
        let mut inner = self.inner.lock_recover();
        inner.register_destination(destination);
    }

    /// Retire a destination. Inverse of
    /// [`register_destination`](Self::register_destination) (leviculum#54).
    ///
    /// After this returns, the hash is no longer one of ours: packets
    /// addressed to it are not accepted as local, so a peer that still holds
    /// a path and dials it finds nobody home. That is what makes a rotated
    /// address retirable — without it a superseded address keeps answering
    /// forever, which re-confirms the node to anyone still probing it.
    ///
    /// Two contract points, both pinned by
    /// `leviculum-std/tests/destination_lifecycle.rs`:
    ///
    /// - **Idempotent.** Retiring a hash that is already gone — or one that
    ///   was never registered here — is a no-op, not a panic or an error. A
    ///   caller whose retirement is timing-driven may fire it twice.
    /// - **Established links are left running.** A link is keyed by its
    ///   `LinkId`, not by the destination it was dialled through, so traffic
    ///   already flowing keeps flowing; only *new* link requests are refused.
    ///   Retiring an address is therefore non-disruptive by design — close
    ///   the links explicitly with [`close_link`](Self::close_link) if the
    ///   intent is to cut them.
    pub fn unregister_destination(&self, hash: &DestinationHash) {
        let mut inner = self.inner.lock_recover();
        inner.unregister_destination(hash);
    }

    /// Install (or clear, with `None`) the per-destination announce-suppression
    /// policy. Suppressed destinations stay routable but are never gossiped.
    /// See [`AnnounceControl`].
    pub fn set_announce_control(&self, policy: Option<Box<dyn AnnounceControl>>) {
        let mut inner = self.inner.lock_recover();
        inner.set_announce_control(policy);
    }

    /// Attach a channel-backed RNode interface to the **running** node at
    /// runtime, returning a lifecycle handle.
    ///
    /// This is the hot-plug counterpart to
    /// [`ReticulumNodeBuilder::add_rnode_channel_interface`](crate::driver::ReticulumNodeBuilder::add_rnode_channel_interface):
    /// the builder wires a radio at construction; this plugs one in at any point
    /// during the node's lifetime — a USB/BLE radio that appears after startup,
    /// or a detach-and-replace. The radio lifecycle (detect → configure →
    /// online → reconnect) runs on the node's own runtime.
    ///
    /// **Hold the returned [`RNodeChannelHandle`](crate::interfaces::rnode::RNodeChannelHandle) to keep the radio attached;
    /// drop it (or call [`RNodeChannelHandle::detach`](crate::interfaces::rnode::RNodeChannelHandle::detach)) to detach** — the
    /// interface task stops, its channel closes, and the event loop removes the
    /// interface from routing, cleanly, without rebuilding the node.
    ///
    /// The node assigns the [`InterfaceId`]. Returns [`Error::NotRunning`] if
    /// called before [`start`](Self::start).
    pub fn spawn_rnode_channel_interface(
        &self,
        _config: crate::interfaces::rnode::RNodeChannelConfig,
    ) -> Result<crate::interfaces::rnode::RNodeChannelHandle, Error> {
        Err(Error::Config("RNodeChannelInterface is disabled in this build".to_string()))
    }

    /// Attach a TCP client interface to the running node, optionally egressing
    /// through a SOCKS5 proxy.
    ///
    /// The node dials `host:port`; when `socks_proxy` is `Some((proxy_host,
    /// proxy_port))` it instead dials the proxy and issues a SOCKS5 CONNECT to
    /// `host:port`. The target host is sent to the proxy as a domain name, so a
    /// name only the proxy can resolve works without local DNS. The interface
    /// reconnects with backoff exactly like a file-configured client.
    ///
    /// **Hold the returned [`TcpClientHandle`] to keep the interface attached;
    /// drop it (or call [`detach`](TcpClientHandle::detach)) to detach** — the
    /// interface stops, its channel closes, and the event loop removes it from
    /// routing, cleanly, without rebuilding the node.
    ///
    /// The node assigns the [`InterfaceId`]. Returns [`Error::NotRunning`] if
    /// called before [`start`](Self::start), or [`Error::Config`] if the dialed
    /// address (the proxy address when `socks_proxy` is set) does not resolve.
    pub fn spawn_tcp_client(
        &self,
        name: &str,
        host: &str,
        port: u16,
        socks_proxy: Option<(String, u16)>,
    ) -> Result<TcpClientHandle, Error> {
        use std::sync::atomic::Ordering;

        let runtime = self.runtime.as_ref().ok_or(Error::NotRunning)?;
        let new_iface_tx = self.new_iface_tx.as_ref().ok_or(Error::NotRunning)?;
        let next_id = self.iface_id_counter.as_ref().ok_or(Error::NotRunning)?;

        // With a proxy, the dialed endpoint is the proxy (its CONNECT reaches the
        // peer) and only it is resolved locally; the target host travels to the
        // proxy verbatim. Without one, the peer is dialed directly.
        let (dial_host, dial_port, socks_target) = match socks_proxy {
            Some((proxy_host, proxy_port)) => {
                (proxy_host, proxy_port, Some((host.to_string(), port)))
            }
            None => (host.to_string(), port, None),
        };
        // addr resolution moved to reconnect loop

        let id = InterfaceId(next_id.fetch_add(1, Ordering::Relaxed));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let handle = {
            let _enter = runtime.enter();
            spawn_tcp_client_with_reconnect(TcpClientConfig {
                id,
                name: name.to_string(),
                target_host: dial_host,
                target_port: dial_port,
                buffer_size: TCP_DEFAULT_BUFFER_SIZE,
                corrupt_every: self.corrupt_every,
                reconnect_interval: Duration::from_secs(5),
                max_reconnect_tries: None,
                reconnect_max_interval: DEFAULT_RECONNECT_MAX_INTERVAL,
                connect_timeout: DEFAULT_TCP_CONNECT_TIMEOUT,
                reconnect_notify: self.reconnect_tx.clone(),
                tunnel_notify: None,
                socks_target,
                shutdown: Some(shutdown_rx),
                outbound_socket_hook: self.outbound_socket_hook.clone(),
            })
        };

        new_iface_tx
            .try_send(handle)
            .map_err(|_| Error::NotRunning)?;

        Ok(TcpClientHandle::new(id, shutdown_tx))
    }

    /// Attach a PipeInterface subprocess to the running node.
    ///
    /// The node spawns `command` as a child process, HDLC-frames outgoing
    /// packets into its stdin, and HDLC-deframes incoming packets from its
    /// stdout — the same wire contract as a file-configured PipeInterface. On
    /// child exit the supervisor respawns it after `respawn_delay` (or the
    /// default when `None`), matching the file-config lifecycle.
    ///
    /// **Hold the returned [`PipeClientHandle`](crate::interfaces::PipeClientHandle)
    /// to keep the interface attached;
    /// drop it (or call [`detach`](crate::interfaces::PipeClientHandle::detach))
    /// to detach** — the supervisor stops, any live child is killed, the
    /// channel closes, and the event loop removes the interface from routing.
    /// Detach preempts the respawn backoff so a stuck child cannot delay it.
    ///
    /// The node assigns the [`InterfaceId`]. Returns [`Error::NotRunning`] if
    /// called before [`start`](Self::start).
    pub fn spawn_pipe_client(
        &self,
        _name: &str,
        _command: &str,
        _respawn_delay: Option<Duration>,
    ) -> Result<crate::interfaces::PipeClientHandle, Error> {
        Err(Error::Config("PipeInterface is disabled in this build".to_string()))
    }

    /// Attach any configured interface type to the running node, through the
    /// same constructor the startup path uses.
    ///
    /// Returns the assigned [`InterfaceId`]s to pass to
    /// [`remove_interface`](Self::remove_interface): one for a leaf interface,
    /// several for a fan-out type (RNodeMulti, I2P peers), and empty for a
    /// self-registering listener (TCP server, AutoInterface) whose children
    /// surface as [`NodeEvent`]s. [`Error::NotRunning`] before
    /// [`start`](Self::start); [`Error::Config`] on an invalid config.
    #[must_use = "the returned ids are the only handle for removing the interface"]
    pub fn spawn_interface(&self, config: InterfaceConfig) -> Result<Vec<InterfaceId>, Error> {
        use std::sync::atomic::Ordering;

        let runtime = self.runtime.as_ref().ok_or(Error::NotRunning)?;
        let next_id = self.iface_id_counter.as_ref().ok_or(Error::NotRunning)?;
        let new_iface_tx = self.new_iface_tx.as_ref().ok_or(Error::NotRunning)?;
        let reconnect_tx = self.reconnect_tx.as_ref().ok_or(Error::NotRunning)?;
        let tunnel_notify_tx = self.tunnel_notify_tx.as_ref().ok_or(Error::NotRunning)?;
        let peer_event_tx = self.peer_event_tx.as_ref().ok_or(Error::NotRunning)?;

        // A runtime interface draws a fresh base id so it never collides with a
        // config-index id; fan-out children draw more.
        let base = next_id.fetch_add(1, Ordering::Relaxed);

        let ctx = interface_build::InterfaceBuildCtx {
            next_id,
            new_iface_tx,
            reconnect_tx,
            tunnel_notify_tx,
            peer_event_tx,
            corrupt_every: self.corrupt_every,
            storage_path: self.storage_path.clone(),
            outbound_socket_hook: self.outbound_socket_hook.clone(),
            inventory: Arc::clone(&self.inventory),
            transport_enabled: self.is_transport_enabled(),
            identity_hash: self.identity_hash(),
        };

        let built = {
            let _enter = runtime.enter();
            interface_build::build_interface(base, &config, &ctx, &self.auto_peer_count)?
        };

        match built {
            interface_build::Built::Handles(mut handles) => {
                apply_runtime_ifac(&config, &mut handles);
                let ids: Vec<InterfaceId> = handles.iter().map(|h| h.info.id).collect();
                for handle in handles {
                    new_iface_tx
                        .try_send(handle)
                        .map_err(|_| Error::NotRunning)?;
                }
                Ok(ids)
            }
            interface_build::Built::SelfManaged => Ok(Vec::new()),
        }
    }

    /// Detach an interface by id, config-loaded or runtime-attached.
    ///
    /// Fires the id to the event loop, which removes it from routing and drops
    /// its task through the same path as a channel-close disconnect. Idempotent:
    /// removing an unknown id is a no-op. Returns [`Error::NotRunning`] if called
    /// before [`start`](Self::start).
    pub fn remove_interface(&self, id: InterfaceId) -> Result<(), Error> {
        self.remove_iface_tx
            .as_ref()
            .ok_or(Error::NotRunning)?
            .try_send(id)
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Connect to a remote destination
    ///
    /// Sends a link request to the destination and returns a `LinkHandle`
    /// for async read/write operations. The returned handle is usable
    /// immediately, but the link is not yet established, watch for
    /// `NodeEvent::LinkEstablished` on the event channel before sending data.
    ///
    /// Returns `Err` only if the event loop is down (the request could not
    /// be dispatched). Link-level failures arrive as `NodeEvent::LinkClosed`.
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash to connect to
    /// * `dest_signing_key` - The destination's signing key (from announce)
    pub async fn connect(
        &self,
        dest_hash: &DestinationHash,
        dest_signing_key: &[u8; 32],
    ) -> Result<LinkHandle, Error> {
        // Request link from NodeCore
        let (link_id, _was_routed, output) = {
            let mut inner = self.inner.lock_recover();
            inner.connect(*dest_hash, dest_signing_key)
        };
        // Send output to event loop for dispatch (backpressure, waits if full)
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;

        Ok(LinkHandle::new(
            link_id,
            Arc::clone(&self.inner),
            self.action_dispatch_tx.clone(),
        ))
    }

    /// Attach a byte-channel interface over a caller-supplied duplex to the
    /// **running** node, returning a lifecycle handle.
    ///
    /// The stream carries HDLC-framed packets — the PipeInterface wire contract,
    /// but in-process over any duplex the caller provides. Ready immediately; on
    /// stream EOF/error or handle drop it detaches.
    ///
    /// **Hold the returned [`ByteChannelHandle`](crate::interfaces::ByteChannelHandle)
    /// to keep the interface attached; drop it (or call
    /// [`detach`](crate::interfaces::ByteChannelHandle::detach)) to detach.**
    ///
    /// The node assigns the [`InterfaceId`]. Returns [`Error::NotRunning`] if
    /// called before [`start`](Self::start).
    pub fn spawn_byte_channel<S>(
        &self,
        name: &str,
        stream: S,
    ) -> Result<crate::interfaces::ByteChannelHandle, Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        use std::sync::atomic::Ordering;

        let runtime = self.runtime.as_ref().ok_or(Error::NotRunning)?;
        let new_iface_tx = self.new_iface_tx.as_ref().ok_or(Error::NotRunning)?;
        let next_id = self.iface_id_counter.as_ref().ok_or(Error::NotRunning)?;

        let id = InterfaceId(next_id.fetch_add(1, Ordering::Relaxed));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let handle = {
            let _enter = runtime.enter();
            crate::interfaces::byte_channel::spawn_byte_channel_interface(
                crate::interfaces::byte_channel::ByteChannelConfig {
                    id,
                    name: name.to_string(),
                    buffer_size: crate::interfaces::byte_channel::BYTE_CHANNEL_DEFAULT_BUFFER_SIZE,
                    shutdown: Some(shutdown_rx),
                },
                stream,
            )
        };

        new_iface_tx
            .try_send(handle)
            .map_err(|_| Error::NotRunning)?;

        Ok(crate::interfaces::ByteChannelHandle::new(id, shutdown_tx))
    }

    /// Obtain a writable handle for an already-established inbound link.
    ///
    /// Incoming links are accepted and proved automatically by the core (Python
    /// parity): once a `LinkEstablished` event fires for a link this node did not
    /// initiate, the link is live. Call this to mint a [`LinkHandle`] for that
    /// link so the application can send on it. Purely a handle constructor; it has
    /// no wire side effect (the establishment proof was already sent).
    ///
    /// # Arguments
    /// * `link_id` - The link ID from the responder-side `LinkEstablished` event
    pub fn link_handle(&self, link_id: &LinkId) -> LinkHandle {
        LinkHandle::new(
            *link_id,
            Arc::clone(&self.inner),
            self.action_dispatch_tx.clone(),
        )
    }

    /// Take the event receiver
    ///
    /// This allows consuming node events directly. Can only be called once.
    ///
    /// The returned [`EventReceiver`] merges the split control/data planes
    /// (Codeberg #71), draining control events with strict priority over data
    /// events. Use [`EventReceiver::recv`] exactly like a
    /// `tokio::sync::mpsc::Receiver`.
    pub fn take_event_receiver(&mut self) -> Option<EventReceiver> {
        self.event_rx.take()
    }

    /// Wait until the interface at index `idx` has reached its readiness
    /// condition, or return `Err(InterfaceReadyError)` after `timeout`.
    ///
    /// # Readiness contract (per interface type)
    ///
    /// - **TCP client (`add_tcp_client`):** ready once the kernel-level
    ///   TCP three-way handshake has succeeded
    ///   (`TcpStream::connect` returned Ok).  This is Option α
    ///   semantics from the Codeberg #49 audit: it does **not**
    ///   guarantee that the remote peer has completed any
    ///   post-accept registration steps it may run.  Tests that
    ///   need the daemon-side peer-registration acknowledgement
    ///   should pair this call with a daemon-side check (e.g. the
    ///   test harness's `TestDaemon::wait_for_peer_count`).
    /// - **TCP server (`add_tcp_server`):** the listener is bound
    ///   before the handle is registered; the API returns
    ///   immediately as ready.
    /// - **RNode (serial or byte-channel):** ready once the reconnect
    ///   task has opened the channel, the firmware answered the detect
    ///   probe, and the radio configuration was confirmed (L-0024). Like
    ///   the TCP client's signal it is one-way: reconnect cycles after
    ///   the first successful configure keep it set.
    /// - **UDP, AutoInterface, Local IPC:** ready once the
    ///   underlying socket / port is bound or the IPC stream is
    ///   connected — currently signalled at handle construction
    ///   time, so the API returns immediately as ready.
    ///
    /// Returns `Err(InterfaceReadyError::Unknown)` if `idx` does not
    /// match any registered interface; `Err(InterfaceReadyError::TimedOut)`
    /// if the readiness deadline elapsed before the signal fired;
    /// `Err(InterfaceReadyError::NotStarted)` if `start()` has not
    /// yet been called.
    pub async fn wait_for_interface_ready(
        &self,
        idx: usize,
        timeout: std::time::Duration,
    ) -> Result<(), InterfaceReadyError> {
        if self.runner_handle.is_none() {
            return Err(InterfaceReadyError::NotStarted);
        }
        let signal = {
            let map = self.iface_ready_map.lock_recover();
            map.get(&idx).cloned()
        };
        match signal {
            Some(s) => s
                .wait(timeout)
                .await
                .map_err(|_| InterfaceReadyError::TimedOut { idx }),
            None => Err(InterfaceReadyError::Unknown { idx }),
        }
    }

    /// Wait until **all** registered interfaces are ready, or return
    /// `Err` listing the ones that timed out.
    ///
    /// The deadline is shared across all interfaces; each
    /// individual wait gets the remaining budget rather than the
    /// full `timeout`.  See [`wait_for_interface_ready`](Self::wait_for_interface_ready)
    /// for the per-interface readiness contract.
    pub async fn wait_for_interfaces_ready(
        &self,
        timeout: std::time::Duration,
    ) -> Result<(), Vec<(usize, ReadyState)>> {
        if self.runner_handle.is_none() {
            return Err(vec![(0, ReadyState::NotStarted)]);
        }
        let signals: Vec<(usize, std::sync::Arc<crate::interfaces::ReadySignal>)> = {
            let map = self.iface_ready_map.lock_recover();
            map.iter()
                .map(|(k, v)| (*k, std::sync::Arc::clone(v)))
                .collect()
        };
        if signals.is_empty() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut failures = Vec::new();
        for (idx, sig) in signals {
            let now = tokio::time::Instant::now();
            let remaining = if deadline > now {
                deadline - now
            } else {
                std::time::Duration::ZERO
            };
            if sig.wait(remaining).await.is_err() {
                failures.push((idx, ReadyState::TimedOut));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }

    /// Get the number of active (established) links
    pub fn active_link_count(&self) -> usize {
        self.inner.lock_recover().active_link_count()
    }

    /// Get the number of pending (not yet established) links
    pub fn pending_link_count(&self) -> usize {
        self.inner.lock_recover().pending_link_count()
    }

    /// Whether the link `link_id` is ESTABLISHED (handshake complete → `Active`).
    ///
    /// Resolves the `link_id` through the `#66` re-key alias table
    /// (`link()` → `resolve_link_id`), so a caller holding the ORIGINAL id from
    /// `connect` still gets a correct answer after an establishment-retry re-key
    /// (Codeberg #66) that re-keyed the link under a fresh wire id. Returns
    /// `false` for an unknown link OR a link still `Pending` (i.e. inserted by
    /// `connect` but not yet handshake-complete) — unlike a raw `links.contains`
    /// or `link_negotiated_mtu(..).is_some()`, both of which are true for a
    /// pending link too.
    ///
    /// This is the establishment gate a sender must poll before `send_resource`:
    /// a consumer tracking establishment by its own event-populated set keyed on
    /// the original id misses the re-keyed id the `LinkEstablished` event
    /// carries, waits forever, and never sends (CIRISEdge#342).
    pub fn link_is_established(&self, link_id: &LinkId) -> bool {
        self.inner
            .lock_recover()
            .link(link_id)
            .is_some_and(leviculum_core::link::Link::is_active)
    }

    /// Get the node's identity hash (16 bytes)
    pub fn identity_hash(&self) -> [u8; 16] {
        *self.inner.lock_recover().identity().hash()
    }

    /// Every tunnel id this node has advertised as a tunnel initiator (Codeberg
    /// #64). A peer that validated our synthesize keys its tunnel table by one
    /// of these ids. Observability / interop-test hook.
    pub fn tunnel_ids(&self) -> Vec<[u8; 32]> {
        self.inner.lock_recover().own_tunnel_ids()
    }

    /// Get the negotiated MTU for a link
    ///
    /// Returns `None` if the link does not exist.
    pub fn link_negotiated_mtu(&self, link_id: &LinkId) -> Option<u32> {
        self.inner
            .lock_recover()
            .link(link_id)
            .map(|l| l.negotiated_mtu())
    }

    /// The DESTINATION a link points at — the dest hash the initiator dialed
    /// (`Link::destination_hash()`). Resolves `link_id` through the `#66`
    /// re-key alias table (`link()` → `resolve_link_id`), so a caller holding
    /// either the original id from `connect` or the re-keyed id an event
    /// carried gets the same answer.
    ///
    /// Returns `None` if the link does not exist.
    ///
    /// Rationale (CIRISEdge#353): a link INITIATOR receiving data back over
    /// its own dialed link needs to attribute the sender. It cannot rely on a
    /// `LinkIdentified` event (only the initiator may identify a link, so the
    /// responder's reply direction never produces one) and it cannot key state
    /// on the original link id (a `#66` establishment-retry re-keys the link
    /// under a fresh wire id, and later events carry the re-keyed id). The
    /// stateless, re-key-proof basis is the link's destination: the initiator
    /// knows which dest it dialed and can map that dest back to a peer.
    pub fn link_destination(&self, link_id: &LinkId) -> Option<DestinationHash> {
        self.inner
            .lock_recover()
            .link(link_id)
            .map(|l| *l.destination_hash())
    }

    /// Get the encrypted link MDU (maximum data unit) for a link
    ///
    /// Returns `None` if the link does not exist.
    pub fn link_mdu(&self, link_id: &LinkId) -> Option<usize> {
        self.inner.lock_recover().link(link_id).map(|l| l.mdu())
    }

    /// Register a known identity for a destination
    ///
    /// Identities learned from received announces are cached automatically.    /// call this only for out-of-band identity registration or testing.
    pub fn remember_identity(
        &self,
        dest_hash: DestinationHash,
        identity: leviculum_core::Identity,
    ) {
        self.inner
            .lock_recover()
            .remember_identity(dest_hash, identity);
    }

    /// Get a handle to the inner NodeCore
    ///
    /// Use this for direct access to the core API.
    #[cfg(test)]
    pub(crate) fn inner(&self) -> Arc<Mutex<StdNodeCore>> {
        Arc::clone(&self.inner)
    }

    /// Check if a path to a destination is known
    pub fn has_path(&self, dest_hash: &leviculum_core::DestinationHash) -> bool {
        self.inner.lock_recover().has_path(dest_hash)
    }

    /// Look up a known identity for a destination hash.
    ///
    /// Returns the identity if it was previously learned from an announce.
    /// The Ed25519 verifying key (bytes 32..64 of `public_key_bytes()`)
    /// is the `dest_signing_key` required by `connect()`.
    pub fn get_identity(
        &self,
        dest_hash: &leviculum_core::DestinationHash,
    ) -> Option<leviculum_core::Identity> {
        self.inner
            .lock_recover()
            .storage()
            .get_identity(dest_hash.as_bytes())
            .cloned()
    }

    /// Request a path to a destination.
    ///
    /// Sends a PATH_REQUEST. The result will arrive as a `PathFound` event
    /// and `has_path()` will return true.
    pub async fn request_path(
        &self,
        dest_hash: &leviculum_core::DestinationHash,
    ) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            inner.request_path(dest_hash)
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Wait until a path to `dest_hash` is known, actively re-issuing a
    /// PATH_REQUEST if it has not arrived passively within `retry_interval`.
    ///
    /// Returns `Ok(true)` as soon as `has_path` is satisfied, or `Ok(false)`
    /// if `timeout` elapses first. The common case (path already known, or an
    /// inbound announce installs it within the first `retry_interval`) never
    /// emits a PATH_REQUEST, so healthy behaviour is unchanged.
    ///
    /// When the passive announce is delayed the explicit PATH_REQUEST forces
    /// the upstream to answer over its path-response code path, which a Python
    /// `rnsd` does not subject to the `inbound()` announce-forward ingress hold
    /// (Codeberg #44): a young daemon-to-daemon peer interface under the
    /// stricter burst rate can hold a forwarded peer announce for
    /// `IC_BURST_HOLD` seconds, well past a client path-wait budget. A
    /// client-issued PATH_REQUEST both registers a waiting path request (which
    /// skips the ingress-limit check on the next inbound announce) and is
    /// answered directly from the daemon's path table, so it bypasses the hold.
    ///
    /// This is purely client-side: it issues the same PATH_REQUEST the stack
    /// already sends on demand and carries no medium awareness, so it stays
    /// within the interface-isolation rule. `retry_interval` should be well
    /// under `timeout` so several requests can be attempted before the
    /// deadline.
    pub async fn wait_for_path(
        &self,
        dest_hash: &leviculum_core::DestinationHash,
        timeout: Duration,
        retry_interval: Duration,
    ) -> Result<bool, Error> {
        const POLL_INTERVAL: Duration = Duration::from_millis(100);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut next_request = tokio::time::Instant::now() + retry_interval;
        loop {
            if self.has_path(dest_hash) {
                return Ok(true);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            if now >= next_request {
                self.request_path(dest_hash).await?;
                next_request = now + retry_interval;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Get hop count to a destination
    pub fn hops_to(&self, dest_hash: &leviculum_core::DestinationHash) -> Option<u8> {
        self.inner.lock_recover().hops_to(dest_hash)
    }

    /// Returns the current ratchet public key for a registered destination.
    pub fn destination_ratchet_public(
        &self,
        dest_hash: &leviculum_core::DestinationHash,
    ) -> Option<[u8; 32]> {
        self.inner
            .lock_recover()
            .destination_ratchet_public(dest_hash)
    }

    /// Returns the KNOWN REMOTE ratchet public key for a destination, learned
    /// from a ratcheted announce (read-only view over the transport store).
    pub fn known_remote_ratchet(
        &self,
        dest_hash: &leviculum_core::DestinationHash,
    ) -> Option<[u8; 32]> {
        self.inner.lock_recover().known_remote_ratchet(dest_hash)
    }

    /// What the next-hop interface toward `dest_hash` reports about its own
    /// medium — on-air bitrate and pre-TX jitter ceiling.
    ///
    /// `None` when no path is known or that interface reports no bitrate
    /// (TCP, UDP, Local: media with no airtime cost to account for). See
    /// [`leviculum_core::transport::LinkProfile`].
    pub fn next_hop_link_profile(
        &self,
        dest_hash: &leviculum_core::DestinationHash,
    ) -> Option<leviculum_core::transport::LinkProfile> {
        self.inner
            .lock_recover()
            .next_hop_link_profile(dest_hash.as_bytes())
    }

    /// Get the number of known paths
    pub fn path_count(&self) -> usize {
        self.inner.lock_recover().path_count()
    }

    /// Read the current monotonic-clock value (milliseconds since
    /// NodeCore construction).
    ///
    /// Exposed to let observability surfaces convert
    /// `PathTableExport.expires_ms` / `RateTableExport.blocked_until_ms`
    /// (both monotonic) into wall-clock projections by anchoring
    /// against `std::time::SystemTime::now()` at call time.
    pub fn now_ms(&self) -> u64 {
        self.inner.lock_recover().now_ms()
    }

    /// Snapshot every known path-table entry.
    ///
    /// Returns owned `PathTableExport` clones — the inner storage map
    /// is unlocked before the result returns to the caller, so no
    /// mutex-borrowed references escape. Intended for downstream
    /// observability surfaces (routing-table inspectors, federation
    /// diagnostics). Snapshot reflects the table at call time; entries
    /// may be evicted by subsequent expiry sweeps.
    pub fn path_table_entries(&self) -> Vec<leviculum_core::transport::PathTableExport> {
        self.inner.lock_recover().path_table_entries()
    }

    /// Snapshot every announce-rate-table entry.
    ///
    /// Returns owned `RateTableExport` clones; same deep-clone /
    /// mutex-release contract as [`Self::path_table_entries`]. Used by
    /// operator tools that need to inspect per-source announce
    /// frequency / ban state.
    pub fn rate_table_entries(&self) -> Vec<leviculum_core::transport::RateTableExport> {
        self.inner.lock_recover().rate_table_entries()
    }

    /// Look up a single path entry by destination hash.
    ///
    /// Returns a cloned `PathEntry` (no mutex-borrowed reference
    /// escapes) or `None` when the destination is unknown.
    pub fn get_path_clone(
        &self,
        dest_hash: &leviculum_core::DestinationHash,
    ) -> Option<leviculum_core::storage_types::PathEntry> {
        self.inner
            .lock_recover()
            .get_path_clone(dest_hash.as_bytes())
    }

    /// Drop a specific path entry by destination hash.
    ///
    /// Returns `true` if the entry existed and was removed, `false`
    /// when it was not present. The local path cache only — does
    /// not emit any wire-level invalidation packet.
    pub fn remove_path(&self, dest_hash: &leviculum_core::DestinationHash) -> bool {
        self.inner.lock_recover().remove_path(dest_hash.as_bytes())
    }

    /// Drop every path whose `next_hop` matches the supplied transport
    /// identity hash.
    ///
    /// Local cache surgery only, mirroring the rnsd RPC drop-all-via
    /// semantics: no wire-level invalidation is emitted.
    ///
    /// Returns the count of paths removed. Useful when a transport
    /// peer is known to be down: the caller bulk-evicts every path
    /// routed via that peer in a single call rather than iterating
    /// the table and calling [`Self::remove_path`] per entry.
    pub fn drop_all_paths_via(&self, via_hash: &leviculum_core::DestinationHash) -> usize {
        self.inner
            .lock_recover()
            .drop_all_paths_via(via_hash.as_bytes())
    }

    /// Get transport statistics (packets sent, received, forwarded, dropped)
    pub fn transport_stats(&self) -> leviculum_core::transport::TransportStats {
        self.inner.lock_recover().transport_stats()
    }

    /// A read-only snapshot of every interface: its name and online status
    /// from the core, joined with the byte counters tracked by the I/O tasks.
    /// Additive; built for diagnostics (an `rnstatus`-style interface view).
    pub fn interface_stats(&self) -> Vec<InterfaceStatusSnapshot> {
        use std::sync::atomic::Ordering;
        // Take the core's name/status list first, then release that lock before
        // touching the counters map, so the two locks never nest.
        let entries = { self.inner.lock_recover().interface_stats() };
        let bytes = self.iface_stats_map.lock_recover();
        entries
            .into_iter()
            .map(|e| {
                // Online state lives on the shared counters, flipped by the
                // owning interface task at its connect boundaries (L-0020).
                // A missing entry falls back to `true`, preserving the
                // pre-#56 behavior for any caller-side mismatch.
                let (rx_bytes, tx_bytes, online) = bytes
                    .get(&e.id)
                    .map(|c| {
                        (
                            c.rx_bytes.load(Ordering::Relaxed),
                            c.tx_bytes.load(Ordering::Relaxed),
                            c.is_online(),
                        )
                    })
                    .unwrap_or((0, 0, true));
                InterfaceStatusSnapshot {
                    interface_id: leviculum_core::transport::InterfaceId(e.id),
                    name: e.name,
                    is_local_client: e.is_local_client,
                    online,
                    rx_bytes: rx_bytes as u64,
                    tx_bytes: tx_bytes as u64,
                    held_announces: e.held_announces,
                    burst_active: e.burst_active,
                    configured_bitrate: e.configured_bitrate,
                    kind: e.kind,
                }
            })
            .collect()
    }

    /// The bound socket addresses of this node's TCP server listeners, in
    /// daemon start order. Each listener binds synchronously during
    /// `start()`, so after a successful start the list is complete. A server
    /// configured with port 0 reports the kernel-assigned port here — the
    /// caller binds `:0` and reads the chosen port back instead of probing a
    /// free port up front and racing every co-tenant for the re-bind
    /// (Codeberg #221).
    pub fn tcp_listen_addrs(&self) -> Vec<std::net::SocketAddr> {
        let inv = self.inventory.lock_recover();
        inv.listeners()
            .filter_map(|(_, row)| row.bound_addr)
            .collect()
    }

    /// Change the announce bandwidth cap on a registered interface at runtime.
    ///
    /// `cap_percent` is the share (1..=100) of the interface's bandwidth the
    /// throttler is allowed to spend on announces. Returns `false` if the
    /// interface has no cap entry (unlimited bitrate, or unknown id) or the
    /// percentage is out of range; the existing queue and next-allowed-at
    /// timestamp carry over so the change takes effect on the next scheduling.
    pub fn set_interface_announce_cap(
        &self,
        iface_id: leviculum_core::transport::InterfaceId,
        cap_percent: u32,
    ) -> bool {
        self.inner
            .lock_recover()
            .set_interface_announce_cap(iface_id.0, cap_percent)
    }

    /// Get link statistics for a link
    pub fn link_stats(
        &self,
        link_id: &leviculum_core::link::LinkId,
    ) -> Option<leviculum_core::node::LinkStats> {
        self.inner.lock_recover().link_stats(link_id)
    }

    /// Announce a registered destination on all interfaces
    ///
    /// Builds the announce packet and queues it as a Broadcast action.
    /// The event loop dispatches the action on the next iteration.
    ///
    /// # Arguments
    /// * `dest_hash` - Hash of the registered destination to announce
    /// * `app_data` - Optional application data to include in the announce
    pub async fn announce_destination(
        &self,
        dest_hash: &leviculum_core::DestinationHash,
        app_data: Option<&[u8]>,
    ) -> Result<(), Error> {
        let output = self
            .inner
            .lock_recover()
            .announce_destination(dest_hash, app_data)?;
        // Send output to event loop for dispatch (backpressure, waits if full)
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Close a link gracefully
    ///
    /// Sends a LINKCLOSE packet to the peer and removes the link.
    ///
    /// # Arguments
    /// * `link_id` - The link ID of the link to close
    pub async fn close_link(&self, link_id: &LinkId) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            inner.close_link(link_id)
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Identify our identity to the link peer.
    ///
    /// See [`NodeCore::identify_link()`] for protocol details.
    pub async fn identify_link(
        &self,
        link_id: &LinkId,
        identity: &leviculum_core::Identity,
    ) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            inner.identify_link(link_id, identity)?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Get the remote identity for a link, if the peer has identified.
    pub fn get_remote_identity(&self, link_id: &LinkId) -> Option<leviculum_core::Identity> {
        let inner = self.inner.lock_recover();
        inner.get_remote_identity(link_id).cloned()
    }

    // Request/Response API
    /// Register a request handler for a given path on a destination.
    pub fn register_request_handler(
        &self,
        destination_hash: leviculum_core::DestinationHash,
        path: &str,
        policy: leviculum_core::RequestPolicy,
    ) {
        let mut inner = self.inner.lock_recover();
        inner.register_request_handler(destination_hash, path, policy);
    }

    /// Remove the request handler for `path`, returning whether one was
    /// registered.
    ///
    /// Requests to a path with no handler are dropped silently, which is what
    /// a served page disappearing has to look like: the protocol has no 404,
    /// so the client sees a clean timeout.
    pub fn deregister_request_handler(
        &self,
        destination_hash: leviculum_core::DestinationHash,
        path: &str,
    ) -> bool {
        let mut inner = self.inner.lock_recover();
        inner.deregister_request_handler(&destination_hash, path)
    }

    /// Send a request on an established link.
    ///
    /// Returns the request_id identifying this request.
    pub async fn send_request(
        &self,
        link_id: &LinkId,
        path: &str,
        data: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<[u8; 16], Error> {
        let (request_id, output) = {
            let mut inner = self.inner.lock_recover();
            inner
                .send_request(link_id, path, data, timeout_ms)
                .map_err(Error::Request)?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(request_id)
    }

    /// Send a response to a received request.
    ///
    /// `response_data` must be exactly one valid msgpack-encoded value.
    pub async fn send_response(
        &self,
        link_id: &LinkId,
        request_id: &[u8; 16],
        response_data: &[u8],
    ) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            inner
                .send_response(link_id, request_id, response_data)
                .map_err(Error::Request)?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Answer a received request with a response Resource, for responses
    /// larger than the link MDU. Use after [`send_response`](Self::send_response)
    /// returns [`RequestError`](leviculum_core::RequestError)`::PayloadTooLarge`.
    /// The `[request_id, response]` msgpack wrapper is added internally; pass
    /// the raw response value bytes as `response_data`.
    ///
    /// `response_data` must be exactly one valid msgpack-encoded value.
    pub async fn send_response_resource(
        &self,
        link_id: &LinkId,
        request_id: &[u8; 16],
        response_data: &[u8],
    ) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            let (_resource_hash, output) = inner
                .send_response_resource(link_id, request_id, response_data)
                .map_err(Error::Resource)?;
            output
        };
        self.completions.note_send_began(*link_id);
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Send a file-style response to a received request: a response Resource
    /// carrying the RAW bytes plus msgpack-encoded `metadata`, with no
    /// `[request_id, response]` wrapper — the wire form NomadNet's
    /// `serve_file` uses for `/file/` downloads.
    pub async fn send_file_response(
        &self,
        link_id: &LinkId,
        request_id: &[u8; 16],
        data: &[u8],
        metadata: &[u8],
    ) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            let (_resource_hash, output) = inner
                .send_file_response(link_id, request_id, data, metadata)
                .map_err(Error::Resource)?;
            output
        };
        self.completions.note_send_began(*link_id);
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    // Resource Transfer API
    /// Initiate a resource transfer on an established link.
    ///
    /// Returns the resource hash identifying this transfer. The ADV packet is
    /// queued and dispatched by the event loop immediately.
    ///
    /// # Arguments
    /// * `link_id` - The link to send over (must be Active)
    /// * `data` - The application data to transfer
    /// * `metadata` - Optional metadata bytes, must be msgpack-encoded by the
    ///   caller (Python's Resource constructor calls `umsgpack.packb(metadata)`)
    pub async fn send_resource(
        &self,
        link_id: &LinkId,
        data: &[u8],
        metadata: Option<&[u8]>,
        auto_compress: bool,
    ) -> Result<[u8; 32], Error> {
        // Phased so the CPU-heavy resource build — bz2 compress, bulk token
        // encrypt, full/map hashing — runs OUTSIDE the node mutex
        // (leviculum#29). Before this split, a round-sized send_resource held
        // the one lock for the whole build, blocking every inbound
        // decrypt/route and every other outbound call for milliseconds at a
        // time. Now the lock is held only to snapshot params and to install
        // the finished transfer.
        //
        // If the link re-keys mid-build (#66), commit refuses the stale
        // ciphertext with LinkStateChanged; rebuild once against fresh params.
        let mut attempts = 0;
        let (resource_hash, output) = loop {
            let params = {
                let inner = self.inner.lock_recover();
                inner
                    .resource_send_params(link_id)
                    .map_err(Error::Resource)?
            };
            let prepared = leviculum_core::resource::prepare_resource_send(
                &params,
                data,
                metadata,
                auto_compress,
                &mut rand_core::OsRng,
            )
            .map_err(Error::Resource)?;
            let committed = {
                let mut inner = self.inner.lock_recover();
                inner.commit_resource_send(prepared)
            };
            match committed {
                Ok(pair) => break pair,
                Err(leviculum_core::resource::ResourceError::LinkStateChanged) if attempts == 0 => {
                    attempts += 1;
                    continue;
                }
                Err(e) => return Err(Error::Resource(e)),
            }
        };
        self.completions.note_send_began(*link_id);
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(resource_hash)
    }

    /// Set the resource acceptance strategy for a link.
    ///
    /// # Arguments
    /// * `link_id` - The link to configure
    /// * `strategy` - The acceptance strategy (AcceptNone, AcceptAll, AcceptApp)
    pub fn set_resource_strategy(
        &self,
        link_id: &LinkId,
        strategy: leviculum_core::resource::ResourceStrategy,
    ) -> Result<(), Error> {
        self.inner
            .lock_recover()
            .set_resource_strategy(link_id, strategy)
            .map_err(Error::Resource)
    }

    /// Accept a pending resource advertisement on a link.
    ///
    /// Call this after receiving a `NodeEvent::ResourceAdvertised` event.
    pub async fn accept_resource(&self, link_id: &LinkId) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            inner.accept_resource(link_id).map_err(Error::Resource)?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Reject a pending resource advertisement on a link.
    ///
    /// Call this after receiving a `NodeEvent::ResourceAdvertised` event.
    pub async fn reject_resource(&self, link_id: &LinkId) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            inner.reject_resource(link_id).map_err(Error::Resource)?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Send a single (fire-and-forget) packet to a destination
    ///
    /// Builds an unreliable data packet addressed to `dest_hash` and queues it
    /// for dispatch. A path to the destination must already be known.
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash to send to
    /// * `data` - The data to send (must fit in a single packet)
    ///
    /// # Returns
    /// The truncated packet hash, usable for tracking delivery proofs.
    pub async fn send_single_packet(
        &self,
        dest_hash: &DestinationHash,
        data: &[u8],
    ) -> Result<[u8; TRUNCATED_HASHBYTES], Error> {
        let (packet_hash, output) = {
            let mut inner = self.inner.lock_recover();
            inner.send_single_packet(dest_hash, data)?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(packet_hash)
    }

    /// Send a delivery proof for a previously received packet, after a
    /// `PacketProofRequested` event under `ProofStrategy::App`. Additive: built
    /// on the core `send_proof`, dispatched like the other send paths.
    pub async fn send_proof(
        &self,
        packet_hash: &[u8; 32],
        dest_hash: &DestinationHash,
    ) -> Result<(), Error> {
        let output = {
            let mut inner = self.inner.lock_recover();
            inner
                .send_proof(packet_hash, dest_hash)
                .map_err(|e| match e {
                    leviculum_core::transport::TransportError::NoPath => {
                        Error::Send(leviculum_core::SendError::NoPath)
                    }
                    other => Error::Config(format!("proof send failed: {other:?}")),
                })?
        };
        self.action_dispatch_tx
            .send(output)
            .await
            .map_err(|_| Error::NotRunning)?;
        Ok(())
    }

    /// Create a PacketSender for a destination
    ///
    /// Returns a self-contained handle for sending single packets.
    /// No path or destination validation, errors are reported on send().
    pub fn packet_sender(&self, dest_hash: &DestinationHash) -> PacketSender {
        PacketSender::new(
            *dest_hash,
            Arc::clone(&self.inner),
            self.action_dispatch_tx.clone(),
        )
    }

    /// Return a diagnostic dump of all protocol state memory usage
    pub fn diagnostic_dump(&self) -> String {
        self.inner.lock_recover().diagnostic_dump()
    }

    /// Check if transport mode (relay/routing) is enabled
    pub fn is_transport_enabled(&self) -> bool {
        self.inner
            .lock_recover()
            .transport_config()
            .enable_transport
    }

    /// Get the number of discovered AutoInterface peers
    ///
    /// Returns 0 if no AutoInterface is configured.
    pub fn auto_interface_peer_count(&self) -> usize {
        self.auto_peer_count.total()
    }

    // Completion futures (leviculum#42). The awaited send variants register
    // interest AFTER the core commit releases the node lock and BEFORE the
    // TickOutput is handed to the event loop: only the event loop can produce
    // the outcome, and it cannot act on a dispatch it has not received, so
    // the outcome can never precede the registration (register-before-
    // dispatch). The after-the-fact await_* variants are mirror/ring-backed.

    /// [`connect`](Self::connect) plus a future for the establishment proof.
    ///
    /// Race-free by construction: the waiter is registered before the link
    /// request is dispatched, so `LinkEstablished` cannot be missed. The
    /// future resolves `Err(CompletionError::LinkClosed)` if the link dies
    /// first (timeout, refusal) and `Err(NodeStopped)` on node stop — it
    /// never hangs on a dead link. It is `tokio::select!`/cancel-safe
    /// (dropping it unregisters the waiter). The caller owns any wall-clock
    /// bound: wrap it in `tokio::time::timeout`.
    pub async fn connect_awaited(
        &self,
        dest_hash: &DestinationHash,
        dest_signing_key: &[u8; 32],
    ) -> Result<(LinkHandle, LinkEstablishedFuture), Error> {
        let (link_id, _was_routed, output) = {
            let mut inner = self.inner.lock_recover();
            inner.connect(*dest_hash, dest_signing_key)
        };
        let established = self.completions.register_link_established(link_id);
        if self.action_dispatch_tx.send(output).await.is_err() {
            // The request never left; Drop unregisters the waiter.
            drop(established);
            return Err(Error::NotRunning);
        }
        Ok((
            LinkHandle::new(
                link_id,
                Arc::clone(&self.inner),
                self.action_dispatch_tx.clone(),
            ),
            established,
        ))
    }

    /// [`send_resource`](Self::send_resource) plus a future for the
    /// sender-side transfer outcome (the peer's completion proof).
    ///
    /// The waiter is registered between the core commit and the dispatch; a
    /// concurrent tick killing the link inside that microgap is caught by
    /// the register-time check against the recently-closed ring, under the
    /// same mutex observation takes. Resolves `Err(Resource)` on transfer
    /// failure and `Err(LinkClosed)` if the link dies with the transfer in
    /// flight and core reports only the link's death — typed either way,
    /// never a hang. Cancel-safe; the caller owns timeouts.
    pub async fn send_resource_awaited(
        &self,
        link_id: &LinkId,
        data: &[u8],
        metadata: Option<&[u8]>,
        auto_compress: bool,
    ) -> Result<([u8; 32], ResourceSentFuture), Error> {
        // Same phased build as send_resource (leviculum#29): CPU work
        // off-lock, one rebuild on a mid-build re-key (#66).
        let mut attempts = 0;
        let (resource_hash, output) = loop {
            let params = {
                let inner = self.inner.lock_recover();
                inner
                    .resource_send_params(link_id)
                    .map_err(Error::Resource)?
            };
            let prepared = leviculum_core::resource::prepare_resource_send(
                &params,
                data,
                metadata,
                auto_compress,
                &mut rand_core::OsRng,
            )
            .map_err(Error::Resource)?;
            let committed = {
                let mut inner = self.inner.lock_recover();
                inner.commit_resource_send(prepared)
            };
            match committed {
                Ok(pair) => break pair,
                Err(leviculum_core::resource::ResourceError::LinkStateChanged) if attempts == 0 => {
                    attempts += 1;
                    continue;
                }
                Err(e) => return Err(Error::Resource(e)),
            }
        };
        self.completions.note_send_began(*link_id);
        let sent = self
            .completions
            .register_resource_sent(resource_hash, *link_id);
        if self.action_dispatch_tx.send(output).await.is_err() {
            drop(sent);
            return Err(Error::NotRunning);
        }
        Ok((resource_hash, sent))
    }

    /// [`send_request`](Self::send_request) plus a future for the response.
    ///
    /// The waiter is registered between the core commit and the dispatch
    /// (same microgap closure as
    /// [`send_resource_awaited`](Self::send_resource_awaited)). Resolves
    /// `Err(RequestTimedOut)` on the in-protocol timeout and
    /// `Err(LinkClosed)` if the link dies first — never a hang. Cancel-safe;
    /// the caller owns any additional wall-clock bound.
    pub async fn send_request_awaited(
        &self,
        link_id: &LinkId,
        path: &str,
        data: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<([u8; 16], RequestResponseFuture), Error> {
        let (request_id, output) = {
            let mut inner = self.inner.lock_recover();
            inner
                .send_request(link_id, path, data, timeout_ms)
                .map_err(Error::Request)?
        };
        let response = self
            .completions
            .register_request_response(request_id, *link_id);
        if self.action_dispatch_tx.send(output).await.is_err() {
            drop(response);
            return Err(Error::NotRunning);
        }
        Ok((request_id, response))
    }

    /// Await establishment of a link by id, after the fact.
    ///
    /// Takes NO node lock — neither here nor when polled (upstream
    /// Lew_Palm/leviculum#199: wait paths must not add lock-taking pub fns);
    /// only the leaf completion registry is consulted. A link that is
    /// already established (per the bounded mirror) or recently closed (per
    /// the recent-outcomes ring) resolves immediately.
    ///
    /// An id the node has NEVER issued matches nothing in the waiters map,
    /// the established mirror, or the recent-outcomes ring, so nothing but a
    /// later `LinkClosed` for the passed id, node stop, or the caller's own
    /// timeout resolves it — for a mistyped or foreign id the caller-owned
    /// timeout is the only bound. (The never-hang guarantee covers dead
    /// objects the node knows about, not objects it never knew.)
    pub fn await_link_established(&self, link_id: &LinkId) -> LinkEstablishedFuture {
        self.completions.register_link_established(*link_id)
    }

    /// Await the sender-side outcome of a resource transfer, after the fact.
    ///
    /// Takes NO node lock, here or when polled (upstream #199). `link_id` is
    /// what lets a `LinkClosed` sweep resolve this waiter, so it never hangs
    /// on a dead link. For split (multi-segment) transfers still in flight
    /// the completion is matched by link — each segment carries its own hash
    /// on the wire — so awaiting the hash `send_resource` returned resolves.
    ///
    /// Two honest gaps, both bounded by the caller's own timeout: (1) a hash
    /// the node never sent (or whose outcome already left the bounded
    /// recent-outcomes ring) matches nothing — only a later `LinkClosed` for
    /// `link_id` or node stop resolves it; (2) a split transfer that already
    /// COMPLETED leaves its ring marker under the final segment's hash, not
    /// the one `send_resource` returned, so a late awaiter parks the same
    /// way. For split transfers prefer [`Self::send_resource_awaited`],
    /// which registers before dispatch and has neither gap.
    pub fn await_resource_sent(
        &self,
        resource_hash: &[u8; 32],
        link_id: &LinkId,
    ) -> ResourceSentFuture {
        self.completions
            .register_resource_sent(*resource_hash, *link_id)
    }

    /// Await the response to a previously sent request, after the fact.
    ///
    /// Takes NO node lock, here or when polled (upstream #199). A response
    /// that was already delivered on the event stream resolves
    /// `Err(CompletionError::AlreadyCompleted)`: response payloads are
    /// deliberately not mirrored (the ring stores markers, never bytes), so
    /// the payload must come from the event stream that already carried it.
    ///
    /// A request id the node never issued matches nothing in the waiters
    /// map or the recent-outcomes ring, so only a later `LinkClosed` for
    /// `link_id`, node stop, or the caller's own timeout resolves it — for
    /// a mistyped/foreign id that timeout is the only bound.
    pub fn await_request_response(
        &self,
        request_id: &[u8; 16],
        link_id: &LinkId,
    ) -> RequestResponseFuture {
        self.completions
            .register_request_response(*request_id, *link_id)
    }

    /// Subscribe a secondary bounded event observer (leviculum#42).
    ///
    /// The tap is fed clones at the dispatch layer, BEFORE the two-plane
    /// sink: it never consumes from or races the primary event receiver, it
    /// sees events the data plane is entitled to drop, and it works on a
    /// daemon-mode node built `without_events()`. A slow tap loses oldest
    /// events (reported via [`TapEvent::Lagged`]); the node never blocks on
    /// it. With no live subscriber the per-event cost is one atomic load.
    pub fn subscribe_events(&self) -> EventTap {
        self.completions.subscribe()
    }
}

// Sans-I/O Event Loop
/// Poll all interface channels with round-robin fairness
///
/// Returns `RecvEvent::Packet` when a complete packet is available, or
/// `RecvEvent::Disconnected` when an interface's incoming channel closes.
/// Returns `Poll::Pending` when no interface has data ready.
///
/// A removal request on `remove_rx` is surfaced as `RecvEvent::Disconnected` so
/// detach and channel-close share one teardown path; it is polled even with an
/// empty registry so removal never wedges behind the pend-forever.
async fn recv_any(
    registry: &mut InterfaceRegistry,
    remove_rx: &mut mpsc::Receiver<InterfaceId>,
) -> RecvEvent {
    std::future::poll_fn(|cx| {
        if let Poll::Ready(Some(id)) = remove_rx.poll_recv(cx) {
            return Poll::Ready(RecvEvent::Disconnected(id));
        }

        if !registry.is_empty() {
            let (handles, poll_start) = registry.handles_mut();
            let len = handles.len();

            for offset in 0..len {
                let idx = (*poll_start + offset) % len;
                let handle = &mut handles[idx];
                let id = handle.info.id;

                match handle.incoming.poll_recv(cx) {
                    Poll::Ready(Some(pkt)) => {
                        *poll_start = (idx + 1) % len;
                        return Poll::Ready(RecvEvent::Packet(id, pkt));
                    }
                    Poll::Ready(None) => {
                        *poll_start = (idx + 1) % len;
                        return Poll::Ready(RecvEvent::Disconnected(id));
                    }
                    Poll::Pending => {}
                }
            }
        }
        Poll::Pending
    })
    .await
}

/// Retire a spawned interface from the reporting inventory (Codeberg #177).
///
/// Called on every disconnect path, BEFORE the counters are dropped from
/// `iface_stats_map`, so the bytes the connection carried stay banked on its
/// listener. The reference gets this for free: a spawned interface increments
/// its parent's `rxb`/`txb` alongside its own (TCPInterface.py:306-308,
/// 327-329) and the parent outlives it, so a listener's totals never fall when
/// a client goes away. Ours has to move them explicitly.
///
/// A departing shared-instance client also frees its slot in the live client
/// count, which is what the reference labels the NEXT client with
/// (LocalInterface.py:355/441).
fn retire_from_inventory(
    inventory: &crate::interfaces::inventory::SharedInventory,
    iface_stats_map: &InterfaceStatsMap,
    local_client_count: &Arc<AtomicUsize>,
    iface_id: usize,
) {
    let (rxb, txb) = {
        let stats = iface_stats_map.lock_recover();
        stats
            .get(&iface_id)
            .map(|c| {
                (
                    c.rx_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    c.tx_bytes.load(std::sync::atomic::Ordering::Relaxed),
                )
            })
            .unwrap_or((0, 0))
    };
    let mut inv = inventory.lock_recover();
    let was_local_client = inv
        .identity(iface_id)
        .is_some_and(|i| i.type_name == "LocalClientInterface");
    inv.remove_spawned(iface_id, rxb as u64, txb as u64);
    drop(inv);
    if was_local_client {
        // `fetch_update` rather than `fetch_sub`: an unmatched retire (a
        // client removed twice through two disconnect paths) must not wrap the
        // counter to usize::MAX and label every later client from there.
        let _ = local_client_count.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |n| Some(n.saturating_sub(1)),
        );
    }
}

// ── leviculum#29 stages 2-3: off-lock inbound precompute ───────────────────

/// One inbound packet plus everything precomputed OFF the node lock for it.
struct PreparedRx {
    iface: InterfaceId,
    data: Vec<u8>,
    pre: leviculum_core::node::PrecomputedRx,
    /// The off-lock decrypt failed against the cached key snapshot — evict it
    /// (most likely a ratchet rotated) and let the in-lock path decrypt.
    evict_decryptor_for: Option<[u8; 16]>,
}

/// Inbound crypto class, decided from a header peek (no lock, no decrypt).
enum RxClass {
    /// Ed25519 signature verify — pure function of the bytes.
    Announce,
    /// Single-destination datagram — X25519 ECDH decrypt against a key
    /// snapshot. Carries the destination hash from the header.
    SingleDest([u8; 16]),
    /// Everything else: cheap crypto or state-entangled; stays inline.
    Cheap,
}

fn classify_rx(data: &[u8]) -> RxClass {
    use leviculum_core::packet::{HeaderType, Packet, PacketContext, PacketType};
    match Packet::unpack(data) {
        Ok(p) => match p.flags.packet_type {
            PacketType::Announce => RxClass::Announce,
            PacketType::Data
                if p.flags.header_type == HeaderType::Type1
                    && p.flags.dest_type == leviculum_core::DestinationType::Single
                    && p.context == PacketContext::None =>
            {
                RxClass::SingleDest(p.destination_hash)
            }
            _ => RxClass::Cheap,
        },
        Err(_) => RxClass::Cheap,
    }
}

/// The no-precompute preparation: just the dedup hash (already off-lock).
fn prepare_cheap(iface: InterfaceId, data: Vec<u8>) -> PreparedRx {
    let pre = leviculum_core::node::PrecomputedRx {
        packet_hash: Some(leviculum_core::packet::packet_hash(&data)),
        ..Default::default()
    };
    PreparedRx {
        iface,
        data,
        pre,
        evict_decryptor_for: None,
    }
}

/// Announce job: hash + full parse + Ed25519 verify, all off-lock. A failed
/// verify passes `announce_verified: false`, so core re-checks and drops the
/// packet exactly as it would have — the memo can only skip REDUNDANT work.
fn precompute_announce(iface: InterfaceId, data: Vec<u8>) -> PreparedRx {
    let packet_hash = leviculum_core::packet::packet_hash(&data);
    let announce_verified = leviculum_core::packet::Packet::unpack(&data)
        .map(|p| leviculum_core::verify_announce_packet(&p))
        .unwrap_or(false);
    PreparedRx {
        iface,
        data,
        pre: leviculum_core::node::PrecomputedRx {
            packet_hash: Some(packet_hash),
            announce_verified,
            ..Default::default()
        },
        evict_decryptor_for: None,
    }
}

/// Single-destination job: hash + ECDH decrypt against the snapshot, off-lock.
/// The decrypt is self-authenticating (token HMAC), so a success is exactly
/// what the in-lock decrypt would produce; a failure falls back in-lock and
/// evicts the snapshot.
fn precompute_single_dest(
    iface: InterfaceId,
    data: Vec<u8>,
    dest: [u8; 16],
    decryptor: std::sync::Arc<leviculum_core::SingleDestDecryptor>,
) -> PreparedRx {
    let packet_hash = leviculum_core::packet::packet_hash(&data);
    let decrypted = leviculum_core::packet::Packet::unpack(&data)
        .ok()
        .and_then(|p| decryptor.decrypt(p.data.as_slice()));
    let failed = decrypted.is_none();
    PreparedRx {
        iface,
        data,
        pre: leviculum_core::node::PrecomputedRx {
            packet_hash: Some(packet_hash),
            single_dest_plaintext: decrypted.map(|(plaintext, ratchet_used)| {
                leviculum_core::node::SingleDestPlaintext {
                    dest_hash: dest,
                    plaintext,
                    ratchet_used,
                }
            }),
            ..Default::default()
        },
        evict_decryptor_for: failed.then_some(dest),
    }
}

/// Run the internal event loop (sans-I/O architecture)
///
/// The driver owns the interfaces and acts as the I/O bridge between the
/// pure state machine (`NodeCore`) and the actual network. Uses `select!`
/// to wake immediately on socket readability, outgoing data, or timer expiry.
#[allow(clippy::too_many_arguments)]
async fn run_event_loop(
    inner: Arc<Mutex<StdNodeCore>>,
    mut registry: InterfaceRegistry,
    channels: EventLoopChannels,
    iface_stats_map: InterfaceStatsMap,
    inventory: crate::interfaces::inventory::SharedInventory,
    local_client_count: Arc<AtomicUsize>,
    flush_interval_secs: u64,
    remote_mgmt: Option<RemoteMgmtResponder>,
    discovery_storage: Option<PathBuf>,
    discovery_network_identity: Option<Arc<leviculum_core::Identity>>,
    autoconnect_wiring: AutoConnectWiring,
    discovery_announce: Option<DiscoveryAnnounceWiring>,
    core_processor: Option<Box<dyn CoreProcessor>>,
    completions: Arc<CompletionRegistry>,
) {
    // A slot rather than the bare box: a panicking hook is detached from
    // inside its own call frame, several `dispatch_output` frames down from
    // here (Codeberg #196).
    let mut core_processor = core_processor.map(processor::ProcessorSlot::new);

    /// Close the completion registry on ANY exit from the loop task —
    /// orderly return or panic unwind. Without this, a panic in the loop
    /// leaves the registry open: parked waiters hang forever and new
    /// registrations park against a node that will never answer (C5).
    struct CloseOnExit(Arc<CompletionRegistry>);
    impl Drop for CloseOnExit {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _close_on_exit = CloseOnExit(Arc::clone(&completions));
    let mut event_sink = channels.event_sink;
    let mut action_dispatch_rx = channels.action_dispatch_rx;
    let mut new_interface_rx = channels.new_interface_rx;
    let mut reconnect_rx = channels.reconnect_rx;
    let mut tunnel_notify_rx = channels.tunnel_notify_rx;
    let mut remove_iface_rx = channels.remove_iface_rx;
    let mut peer_event_rx = channels.peer_event_rx;
    // A removal can arrive before the event loop has registered its interface
    // (a detach racing a just-accepted add); held here, applied on arrival.
    let mut pending_removals: std::collections::HashSet<InterfaceId> =
        std::collections::HashSet::new();
    let mut shutdown = channels.shutdown;
    let mut next_poll = tokio::time::Instant::now();
    let mut next_flush = tokio::time::Instant::now() + Duration::from_secs(flush_interval_secs);
    // In-flight off-lock flush write (leviculum#44). Doubles as the overlap
    // guard: a flush timer fire while a write is in flight re-arms and does
    // nothing. Settled on every exit path, including shutdown.
    let mut flush_in_flight: Option<tokio::task::JoinHandle<crate::storage::FlushOutcome>> = None;
    let mut retry_queues: BTreeMap<usize, VecDeque<Vec<u8>>> = BTreeMap::new();
    // Live peer count per multi-peer interface (Codeberg #365), counted
    // from the same ordered Up/Lost channel the path cull and the
    // peer-up decision ride — the interface's LinkTable reports each
    // identity's first link as Up and its last link's death as Lost, so
    // the transitions balance per identity. Mirrored into the core
    // (`set_interface_peer_count`, the sibling of the firmware's
    // per-pass online mirror) before each event is handled, so the
    // peer-link path-request re-origination sees the interface as a
    // live-peer carrier exactly while it has one.
    let mut live_peer_counts: BTreeMap<usize, usize> = BTreeMap::new();
    // The peer-up announce gate (Codeberg #376), keyed on peer IDENTITY
    // for the reason the constant states: a BLE peer rotates its address
    // and a rotation must not buy it another announce. Loop-local like
    // `live_peer_counts` — it is per-run state about who has been spoken
    // to, and a restart legitimately starts over.
    let mut peer_announce_gate: PeerAnnounceLimiter<PEER_ANNOUNCE_SLOTS> =
        PeerAnnounceLimiter::new();
    // Track which per-interface queues have already emitted the
    // depth-high warning so we don't spam once the queue is deep.
    // Cleared when the queue drops back below RETRY_QUEUE_DEPTH_WARN.
    let mut retry_queue_warned: std::collections::BTreeSet<usize> =
        std::collections::BTreeSet::new();
    // Monotonic high-watermark of each retry_queue's depth since
    // process start. Logged at info! when it increases so hardware
    // benchmarks can read it out of the capture without extra
    // instrumentation.
    let mut retry_queue_max_depth: BTreeMap<usize, usize> = BTreeMap::new();

    // Clone IFAC configs from core so dispatch_output can apply IFAC outside the lock.
    // This is the canonical source of truth for "what IFAC config does interface N have
    // according to the INI config". On reconnect, we re-apply from this map.
    let mut ifac_configs: BTreeMap<usize, leviculum_core::ifac::IfacConfig> = {
        let core = inner.lock_recover();
        core.clone_ifac_configs()
    };

    // #151: per discovered endpoint, the IFAC of the interface its discovery
    // announce was last heard on under IFAC protection. Feeds the inherit rule
    // of `resolve_autoconnect_ifac`. In-memory only: after a restart the
    // hearing interface is unknown again, so an IFAC-requiring endpoint fails
    // closed until its next announce is heard (safe by design).
    let mut discovery_heard_ifac: HeardIfacMap = BTreeMap::new();
    // Interface ids ever spawned by the auto-connect spawner. Used to exclude
    // discovery-sourced IFAC registrations from the "does the operator run
    // IFAC on this node" check (a peer's advertised key must not close an
    // otherwise open node), and never pruned (ids are monotonic and tiny).
    let mut autoconnect_spawned_ids: std::collections::BTreeSet<usize> =
        std::collections::BTreeSet::new();
    // Endpoints already warned about as refused (fail closed), so the 1 s
    // auto-connect poll does not spam the log. Keyed by discovery_hash.
    let mut autoconnect_refused_warned: std::collections::BTreeSet<
        [u8; leviculum_core::discovery::STAMP_SIZE],
    > = std::collections::BTreeSet::new();

    // Runtime auto-connect (Codeberg #32, sub-task b). The manager owns the
    // spawn/register/teardown lifecycle; `poll` runs periodically off the live
    // discovered-interface registry. `None` when disabled (cap 0) or when there
    // is no storage root to read discovered records from.
    let mut autoconnect = (autoconnect_wiring.max > 0 && discovery_storage.is_some())
        .then(|| crate::autoconnect::AutoConnectManager::new(autoconnect_wiring.max));
    let mut next_autoconnect = tokio::time::Instant::now() + AUTOCONNECT_POLL_INTERVAL;

    // Periodic interface-discovery announcer (Codeberg #107, Python
    // `InterfaceAnnouncer`). `None` when no interface is discoverable. Each tick
    // self-advertises the most-overdue discoverable interface on
    // `rnstransport.discovery.interface`. The first tick fires one job interval
    // in (Python sleeps first).
    let mut discovery_announce = discovery_announce;
    let mut next_discovery_announce = discovery_announce
        .as_ref()
        .map(|d| tokio::time::Instant::now() + d.job_interval);

    // ── Off-lock inbound precompute (leviculum#29 stages 2-3) ──
    //
    // The expensive inbound crypto classes — announce Ed25519 verification and
    // Single-destination ECDH decryption — are computed BEFORE the node lock
    // is taken; the in-lock apply consumes the memo (`PrecomputedRx`) instead
    // of recomputing. Every memo is advisory (core falls back to computing
    // under the lock), so a stale key snapshot or failed off-lock decrypt
    // costs duplicate work, never correctness.
    // Single-destination decrypt-key snapshots, owned by this task (no lock).
    // Lazily filled from the core; an entry that fails to decrypt is evicted
    // (ratchet rotation) and refreshed on the next packet for that dest.
    let mut sd_decryptors: std::collections::HashMap<
        [u8; 16],
        std::sync::Arc<leviculum_core::SingleDestDecryptor>,
    > = std::collections::HashMap::new();

    macro_rules! apply_inbound {
        ($prepared:expr) => {{
            let prepared: PreparedRx = $prepared;
            if let Some(dest) = prepared.evict_decryptor_for {
                // The snapshot failed against these bytes — most likely a
                // ratchet rotated. Drop it; the next packet re-exports fresh
                // keys. The current packet falls back to the in-lock decrypt.
                sd_decryptors.remove(&dest);
            }
            let (output, now_ms) = {
                let mut core = inner.lock_recover();
                let output =
                    core.handle_packet_precomputed(prepared.iface, &prepared.data, prepared.pre);
                let now_ms = core.now_ms();
                (output, now_ms)
            };
            if let Some(deadline_ms) = output.next_deadline_ms {
                let delta = deadline_ms.saturating_sub(now_ms);
                let wake_at = tokio::time::Instant::now() + Duration::from_millis(delta);
                if wake_at < next_poll {
                    next_poll = wake_at;
                }
            }
            let processor_delay = dispatch_output(
                output,
                &mut registry,
                event_sink.as_mut(),
                &inner,
                &mut retry_queues,
                &mut retry_queue_warned,
                &mut retry_queue_max_depth,
                &ifac_configs,
                remote_mgmt.as_ref(),
                discovery_storage.as_deref(),
                discovery_network_identity.as_deref(),
                &mut discovery_heard_ifac,
                &completions,
                core_processor.as_mut(),
            );
            tighten_next_poll(&mut next_poll, processor_delay);
        }};
    }

    loop {
        // Auto-connect poll wake — only armed while the feature is enabled.
        let autoconnect_wake = autoconnect.as_ref().map(|_| next_autoconnect);

        // Discovery announcer wake — only armed while at least one interface is
        // discoverable.
        let discovery_announce_wake = next_discovery_announce;

        // Event-driven retry-queue drain. Any non-empty queue whose
        // front packet is currently ineligible for a slot contributes a
        // wake deadline; the earliest of those becomes the
        // tokio::time::sleep_until arm below. When all queues are empty
        // or the head is already ready, no sleep arm is activated.
        let retry_wake_instant: Option<tokio::time::Instant> = {
            let now_ms = inner.lock_recover().now_ms();
            compute_retry_wake_deadline_ms(&retry_queues, &registry, now_ms)
                .and_then(|slot_ms| slot_ms.checked_sub(now_ms))
                .map(|delta_ms| tokio::time::Instant::now() + Duration::from_millis(delta_ms))
        };

        tokio::select! {
            // Fires exactly when the earliest retry-queue head becomes
            // eligible. The arm only exists when retry_wake_instant is
            // Some; otherwise the select skips it. Inside, we call
            // drain + push_interface_state to get the packets out and
            // refresh Transport's caches.
            _ = async {
                match retry_wake_instant {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => core::future::pending::<()>().await,
                }
            } => {
                let now_ms = inner.lock_recover().now_ms();
                drain_retry_queues(&mut retry_queues, &mut registry, now_ms);
                push_interface_state(&mut registry, &inner);
            }

            // Branch 1: Packet from any interface
            event = recv_any(&mut registry, &mut remove_iface_rx) => {
                match event {
                    RecvEvent::Packet(iface_id, pkt) => {
                        tracing::debug!(
                            "driver: received {} bytes from iface {} ({})",
                            pkt.data.len(),
                            iface_id,
                            registry.name_of(iface_id),
                        );
                        // leviculum#29 stages 2-3: precompute the expensive
                        // inbound crypto OFF the node lock. The Ed25519
                        // announce verify and the Single-destination ECDH
                        // decrypt run right here on the loop thread — with NO
                        // lock held — and the in-lock apply consumes the memo
                        // instead of recomputing. Outbound callers (connect,
                        // sends, resource commits) therefore never wait behind
                        // inbound crypto anymore: the lock is free while it
                        // runs. (A parallel worker-pool variant was measured
                        // and rejected: on release builds the per-job task
                        // overhead equals or exceeds the crypto being moved,
                        // and it regressed announce throughput 2x. The memo
                        // seam is host-independent; the placement is not.)
                        //
                        // IFAC-protected interfaces skip precompute — core
                        // rewrites their bytes, invalidating anything computed
                        // here.
                        let prepared = if ifac_configs.contains_key(&iface_id.0) {
                            prepare_cheap(iface_id, pkt.data)
                        } else {
                            match classify_rx(&pkt.data) {
                                RxClass::Announce => precompute_announce(iface_id, pkt.data),
                                RxClass::SingleDest(dest) => {
                                    let decryptor = match sd_decryptors.get(&dest) {
                                        Some(d) => Some(std::sync::Arc::clone(d)),
                                        None => {
                                            let exported = inner
                                                .lock_recover()
                                                .export_single_dest_decryptor(
                                                    &leviculum_core::DestinationHash::new(dest),
                                                )
                                                .map(std::sync::Arc::new);
                                            if let Some(d) = &exported {
                                                sd_decryptors
                                                    .insert(dest, std::sync::Arc::clone(d));
                                            }
                                            exported
                                        }
                                    };
                                    match decryptor {
                                        Some(d) => {
                                            precompute_single_dest(iface_id, pkt.data, dest, d)
                                        }
                                        None => prepare_cheap(iface_id, pkt.data),
                                    }
                                }
                                RxClass::Cheap => prepare_cheap(iface_id, pkt.data),
                            }
                        };
                        apply_inbound!(prepared);
                    }
                    RecvEvent::Disconnected(iface_id) => {
                        tracing::warn!("Interface {} ({}) disconnected", iface_id, registry.name_of(iface_id));
                        // The core forgets its peer-count mirror in
                        // handle_interface_down; forget the driver's
                        // half too so a re-registered index starts at
                        // zero (Codeberg #365).
                        live_peer_counts.remove(&iface_id.0);
                        let output = {
                            let mut core = inner.lock_recover();
                            core.handle_interface_down(iface_id)
                        };
                        let processor_delay = dispatch_output(
                            output,
                            &mut registry,
                            event_sink.as_mut(),
                            &inner,
                            &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs,
                            remote_mgmt.as_ref(),
                            discovery_storage.as_deref(),
                            discovery_network_identity.as_deref(),
                            &mut discovery_heard_ifac,
                            &completions,
                            core_processor.as_mut(),
                        );
                        tighten_next_poll(&mut next_poll, processor_delay);
                        // Clear retry queue for disconnected interface. The legacy
                        // is_interface_congested flag was removed in Phase F;
                        // Transport's interface_next_slot_ms falls back to
                        // now_ms once the interface is removed from the
                        // backchannel, which happens naturally.
                        //
                        // #25 — the frames in that queue are DESTROYED here. The
                        // driver cannot re-home them (they are bound to this
                        // interface, and link traffic's link died with it), but
                        // destroying them SILENTLY is what made the loss
                        // invisible above the driver. Count them, say so, and
                        // emit it so the sender can re-send on a fresh link.
                        // For an accept-only node serving a NAT'd initiator this
                        // is the steady state (rebind ≈60 s), not an edge case.
                        //
                        // #196: dispatched rather than emitted straight to the
                        // sink, so the registered processor's tap sees it. A
                        // processor is a sender like any other, and this is the
                        // one loss signal it cannot learn any other way.
                        if let Some(purged) = retry_queues.remove(&iface_id.0) {
                            if !purged.is_empty() {
                                tracing::warn!(
                                    "Interface {} destroyed {} queued frame(s) on disconnect — \
                                     the sender MUST re-send on a fresh link (#25)",
                                    iface_id,
                                    purged.len()
                                );
                                let mut purge_output = TickOutput::empty();
                                purge_output.events.push(NodeEvent::FramesDropped {
                                    iface_id: iface_id.0,
                                    count: purged.len(),
                                    reason: FrameDropReason::RetryQueuePurged,
                                });
                                tighten_next_poll(&mut next_poll, dispatch_output(
                                    purge_output,
                                    &mut registry,
                                    event_sink.as_mut(),
                                    &inner,
                                    &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs,
                                    // A purge notice is neither a request nor
                                    // an announce; the responder and the
                                    // discovery registry have no business here.
                                    None,
                                    None,
                                    None,
                                    &mut discovery_heard_ifac,
                                    &completions,
                                    core_processor.as_mut(),
                                ));
                            }
                        }
                        if !registry.remove(iface_id) {
                            pending_removals.insert(iface_id);
                        }
                        retire_from_inventory(
                            &inventory,
                            &iface_stats_map,
                            &local_client_count,
                            iface_id.0,
                        );
                        {
                            let mut stats = iface_stats_map.lock_recover();
                            stats.remove(&iface_id.0);
                        }
                        // Drop auto-connect tracking so a later rediscovery may
                        // re-establish this endpoint (Codeberg #32).
                        if let Some(manager) = autoconnect.as_mut() {
                            manager.on_interface_removed(iface_id);
                        }
                    }
                }
            }

            // Branch 2: Dispatch TickOutput from outside the event loop
            // (connect, send_on_link, close_link, announce send here)
            Some(output) = action_dispatch_rx.recv() => {
                let processor_delay = dispatch_output(
                    output,
                    &mut registry,
                    event_sink.as_mut(),
                    &inner,
                    &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs,
                    remote_mgmt.as_ref(),
                    discovery_storage.as_deref(),
                    discovery_network_identity.as_deref(),
                    &mut discovery_heard_ifac,
                    &completions,
                    core_processor.as_mut(),
                );
                tighten_next_poll(&mut next_poll, processor_delay);
            }

            // Branch 3: Timer, persistent deadline, not recomputed per iteration
            _ = tokio::time::sleep_until(next_poll) => {
                let (output, tick_output, now_ms) = {
                    let mut core = inner.lock_recover();
                    let output = core.handle_timeout();
                    // Blackhole `until` timestamps are unix wall-clock values
                    // from the Python RPC, so the expiry sweep needs wall time
                    // injected here; the sweep self-throttles to one pass per
                    // 60 s (Python Transport.py:973-994).
                    let unix_now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    core.expire_blackholed_identities(unix_now);
                    // Cull expired tunnels/tunnel paths (Codeberg #64); the
                    // call self-throttles to one pass per minute.
                    core.cull_tunnels();
                    let now_ms = core.now_ms();
                    // Codeberg #196: the processor's periodic slot. An event tap
                    // alone can only react — it fires when the core has something
                    // to say — so a processor that wants to *initiate* (drain its
                    // own outbound queue, run its own timers) needs a slot that
                    // fires on the driver's clock.
                    //
                    // Kept SEPARATE from the core's own output rather than
                    // merged into it. Merging routed the processor's synthetic
                    // events into three consumers the detached dispatch below
                    // deliberately excludes: back into the tap (so a processor
                    // re-handled its own `on_tick` output, exactly the
                    // unbounded self-cycle `run_event_tap` documents as a node
                    // hang), into the `/status` responder — whose "any request
                    // that reaches here is authorised" rests on the core having
                    // checked it, which is false for an event the core never
                    // produced — and into the discovery registry, which would
                    // persist a synthetic announce. `on_event` output was
                    // isolated from all three; `on_tick` output from none.
                    let tick_output = core_processor
                        .as_mut()
                        .map(|slot| processor::run_tick(slot, &mut core, now_ms));
                    (output, tick_output, now_ms)
                };
                // Scheduling is preserved across the split by taking the same
                // `min()` `TickOutput::merge` used to take. Nothing else about
                // the deadline changes.
                let next = merged_next_deadline(
                    output.next_deadline_ms,
                    tick_output.as_ref().and_then(|o| o.next_deadline_ms),
                );
                let tap_delay = dispatch_output(
                    output,
                    &mut registry,
                    event_sink.as_mut(),
                    &inner,
                    &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs,
                    remote_mgmt.as_ref(),
                    discovery_storage.as_deref(),
                    discovery_network_identity.as_deref(),
                    &mut discovery_heard_ifac,
                    &completions,
                    core_processor.as_mut(),
                );
                // The processor's periodic output goes out on the driver's own
                // send path, detached exactly like the tap's: no responder, no
                // discovery persistence, no re-entry into the tap. The event
                // sink stays attached — these events belong on the
                // application's stream like any other.
                if let Some(tick_output) = tick_output {
                    if !tick_output.is_empty() {
                        // No return value to fold in: the tap is detached on
                        // this call, so nothing here can ask for a deadline.
                        // `on_tick`'s own deadline is already in `next` above.
                        let _ = dispatch_output(
                            tick_output,
                            &mut registry,
                            event_sink.as_mut(),
                            &inner,
                            &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs,
                            None,
                            None,
                            None,
                            &mut discovery_heard_ifac,
                            &completions,
                            None,
                        );
                    }
                }

                // Advance next_poll based on next_deadline_ms
                let interval = match next {
                    Some(deadline_ms) => {
                        let delta = deadline_ms.saturating_sub(now_ms);
                        Duration::from_millis(delta.clamp(1, 1000))
                    }
                    None => Duration::from_secs(1),
                };
                next_poll = tokio::time::Instant::now() + interval;
                tighten_next_poll(&mut next_poll, tap_delay);
            }

            // Branch 4: Shutdown — bounded graceful drain (Codeberg #77).
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("Node shutdown requested");
                    // Drain any TickOutputs still queued in action_dispatch_rx
                    // (e.g. a responder close_link enqueued just before stop()/
                    // drop) and dispatch them to the interfaces. Breaking here
                    // without draining would discard those outputs undispatched
                    // — including the SendPacket close bytes AND the LinkClosed
                    // event riding in the same output — which is the #77 loss.
                    while let Ok(output) = action_dispatch_rx.try_recv() {
                        // Shutting down; there is no next poll to bring forward.
                        let _ = dispatch_output(
                            output,
                            &mut registry,
                            event_sink.as_mut(),
                            &inner,
                            &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs,
                            remote_mgmt.as_ref(),
                            discovery_storage.as_deref(),
                            discovery_network_identity.as_deref(),
                            &mut discovery_heard_ifac,
                            &completions,
                            core_processor.as_mut(),
                        );
                    }
                    // Bounded graceful flush: dispatch only pushes onto the
                    // interface outgoing channels; wait for the interface tasks
                    // to pop and write_all them to the socket before the runtime
                    // aborts the tasks.
                    flush_outgoing_on_shutdown(&registry).await;
                    // Join any in-flight storage flush before breaking: stop()
                    // runs the synchronous shutdown flush the moment the runner
                    // joins, and an unjoined background write could rename an
                    // older merge over it. Unbounded await on local disk IO —
                    // the same exposure the old under-lock flush had here.
                    if let Some(handle) = flush_in_flight.take() {
                        let joined = handle.await;
                        settle_flush(&inner, joined);
                    }
                    break;
                }
            }

            // Branch 5: Dynamic interface registration (TCP server, local server accept loops)
            Some(handle) = new_interface_rx.recv() => {
                if pending_removals.remove(&handle.info.id) {
                    continue;
                }
                tracing::info!("New connection: {} ({})", handle.info.name, handle.info.id);
                let is_local = handle.info.is_local_client;
                let iface_idx = handle.info.id.0;
                let inherited_ifac = handle.info.ifac.clone();
                let inherited_mode = handle.info.mode;
                let inherited_kind = handle.info.kind;
                let inherited_ingress = handle.info.ingress_control;
                {
                    let mut core = inner.lock_recover();
                    core.set_interface_name(iface_idx, handle.info.name.clone());
                    if let Some(hw_mtu) = handle.info.hw_mtu {
                        core.set_interface_hw_mtu(iface_idx, hw_mtu);
                    }
                    if is_local {
                        core.set_interface_local_client(iface_idx, true);
                    }
                    // Codeberg #104: apply the mode inherited from the parent
                    // listener (e.g. a TCP server in AP/roaming mode) so the
                    // spawned-per-connection interface carries the server's mode
                    // and the inbound-side propagation rules apply to this peer.
                    core.set_interface_mode(iface_idx, inherited_mode);
                    core.set_interface_kind(iface_idx, inherited_kind);
                    // Ingress control (Codeberg #8, reshaped in #189): a
                    // dynamically-spawned interface inherits its listener's
                    // configured value, mirroring `spawned_interface
                    // .ingress_control = self.ingress_control`
                    // (TCPInterface.py:582, I2PInterface.py:951,
                    // BackboneInterface.py:409). #8 forced `false` here instead,
                    // which threw away the operator's `ingress_control` on every
                    // accepted connection — a larger claim than "this medium
                    // does not need it", and the reason a TCP server never
                    // limited an ingress burst at all.
                    //
                    // `None` means no listener declared a value. Every spawner
                    // in the tree declares one today (TCP accept, I2P accept and
                    // peer, local IPC accept), so this is the fallback for a
                    // future one; off is the safe reading, and it is what the
                    // local IPC path asks for explicitly anyway.
                    let ingress_on = inherited_ingress.unwrap_or(false);
                    core.set_interface_ingress_control(iface_idx, ingress_on);
                    // Inherit IFAC config from parent interface (e.g., TCP server listener).
                    // Removal path: handle_interface_down removes ifac_config when connection drops.
                    if let Some(ifac) = &inherited_ifac {
                        core.set_ifac_config(iface_idx, ifac.clone());
                    }
                }
                // Mirror inherited IFAC in driver-local ifac_configs for dispatch_actions.
                if let Some(ifac) = inherited_ifac {
                    ifac_configs.insert(iface_idx, ifac);
                }
                {
                    let mut stats = iface_stats_map.lock_recover();
                    stats.insert(iface_idx, Arc::clone(&handle.counters));
                }
                registry.register(handle);

                // Send cached local-destination announces on the new interface
                // so the new peer learns about our destinations even if the
                // original announce was sent before the connection was established.
                if !is_local {
                    let output = {
                        let mut core = inner.lock_recover();
                        core.handle_interface_up(iface_idx)
                    };
                    tighten_next_poll(&mut next_poll, dispatch_output(output, &mut registry, event_sink.as_mut(), &inner, &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs, remote_mgmt.as_ref(), discovery_storage.as_deref(), discovery_network_identity.as_deref(), &mut discovery_heard_ifac, &completions, core_processor.as_mut()));
                }
            }

            // Branch 6: TCP client reconnection (Block D)
            //
            // When a reconnecting TCP client re-establishes its connection, it
            // sends a notification here. We call handle_interface_up() to
            // re-announce all local destinations (daemon-owned get fresh announces,
            // client-cached get rebroadcast) so the remote peer re-learns paths.
            Some(iface_id) = reconnect_rx.recv() => {
                tracing::info!("Interface {} reconnected, re-announcing destinations", iface_id);
                // Re-apply IFAC config to core (E29: handle_interface_down removed it)
                if let Some(cfg) = ifac_configs.get(&iface_id.0) {
                    let mut core = inner.lock_recover();
                    core.set_ifac_config(iface_id.0, cfg.clone());
                }
                let output = {
                    let mut core = inner.lock_recover();
                    core.handle_interface_up(iface_id.0)
                };
                tighten_next_poll(&mut next_poll, dispatch_output(output, &mut registry, event_sink.as_mut(), &inner, &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs, remote_mgmt.as_ref(), discovery_storage.as_deref(), discovery_network_identity.as_deref(), &mut discovery_heard_ifac, &completions, core_processor.as_mut()));
            }

            // Branch 6c: Per-peer transitions on a multi-peer interface
            // (Codeberg #365). The BLE interface reports the identity hash
            // of a peer whose last link on its broadcast domain died (core
            // culls the path entries whose next hop is that peer on that
            // interface — the per-peer analog of the Disconnected cleanup
            // above, Transport.py:784-785 semantics) or whose first link
            // came up (the driver mirrors the live peer count into core and
            // reports the arrival, see `handle_interface_peer_up`; the
            // delivery-path pull there is opt-in). One ordered channel for
            // both, so a flap's cull always lands before its reconnect's
            // pull decision. The interface itself stays up — other peers
            // keep their links and paths.
            Some((iface_id, event)) = peer_event_rx.recv() => {
                // Keep the driver's live-peer count and its core mirror
                // in step with the transition before handling it
                // (Codeberg #365; see `live_peer_counts`).
                let count = live_peer_counts.entry(iface_id.0).or_insert(0);
                match event {
                    crate::interfaces::PeerEvent::Lost(_) => *count = count.saturating_sub(1),
                    crate::interfaces::PeerEvent::Up(_) => *count += 1,
                }
                let count = *count;
                let output = {
                    let mut core = inner.lock_recover();
                    core.set_interface_peer_count(iface_id.0, count);
                    match event {
                        crate::interfaces::PeerEvent::Lost(peer) => {
                            core.handle_interface_peer_lost(iface_id, peer)
                        }
                        crate::interfaces::PeerEvent::Up(peer) => {
                            let mut output = core.handle_interface_peer_up(iface_id, peer);
                            // Codeberg #376: and announce OURSELVES to the
                            // new peer, on its link alone. The pull above
                            // asks what the peer is; this tells it what we
                            // are, which it cannot learn any other way
                            // until our next announce happens to fall in
                            // its lifetime. The clock plausibility and the
                            // per-peer rate limit are read here, so the
                            // core keeps deciding only what an announce is.
                            let clock_ok = core.has_plausible_wall_clock();
                            let now_ms = core.now_ms();
                            output.actions.extend(
                                peer_up_announce(
                                    &mut core,
                                    &mut peer_announce_gate,
                                    iface_id,
                                    peer,
                                    now_ms,
                                    clock_ok,
                                )
                                .actions,
                            );
                            output
                        }
                    }
                };
                tighten_next_poll(&mut next_poll, dispatch_output(output, &mut registry, event_sink.as_mut(), &inner, &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs, remote_mgmt.as_ref(), discovery_storage.as_deref(), discovery_network_identity.as_deref(), &mut discovery_heard_ifac, &completions, core_processor.as_mut()));
            }

            // Branch 6b: Tunnel synthesize initiation (Codeberg #64).
            //
            // A tunnel-capable TCP client fires here on every successful connect
            // (initial AND reconnect). We initiate the synthesize handshake
            // toward the peer so it (re)establishes the tunnel and restores the
            // paths it learned from us. A no-op for non-tunnel interfaces.
            Some(iface_id) = tunnel_notify_rx.recv() => {
                let output = {
                    let mut core = inner.lock_recover();
                    core.send_tunnel_synthesize(iface_id.0)
                };
                tighten_next_poll(&mut next_poll, dispatch_output(output, &mut registry, event_sink.as_mut(), &inner, &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs, remote_mgmt.as_ref(), discovery_storage.as_deref(), discovery_network_identity.as_deref(), &mut discovery_heard_ifac, &completions, core_processor.as_mut()));
            }

            // Branch 7: Periodic storage flush (persist identities + packet
            // hashes). The node lock is held only to snapshot the dirty
            // state; the file read+merge+write runs on the blocking pool and
            // Branch 7b settles it (leviculum#44).
            _ = tokio::time::sleep_until(next_flush) => {
                if flush_in_flight.is_none() {
                    flush_in_flight = begin_flush(&inner);
                }
                next_flush = tokio::time::Instant::now() + Duration::from_secs(flush_interval_secs);
            }

            // Branch 7b: the off-lock flush write finished; brief re-lock to
            // clear the dirty flags the write covered.
            joined = async {
                match flush_in_flight.as_mut() {
                    Some(handle) => handle.await,
                    None => core::future::pending().await,
                }
            }, if flush_in_flight.is_some() => {
                flush_in_flight = None;
                settle_flush(&inner, joined);
            }

            // Branch 8: Runtime auto-connect of discovered interfaces (#32b).
            //
            // Reconcile the auto-connected interface set against the live
            // discovered-interface registry: spawn new auto-connectable
            // (Backbone/TCP) endpoints, and tear down interfaces whose backing
            // record is gone or that have stayed offline past the detach
            // threshold. Armed only while auto-connect is enabled.
            _ = async {
                match autoconnect_wake {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => core::future::pending::<()>().await,
                }
            } => {
                if let (Some(manager), Some(storage_root)) =
                    (autoconnect.as_mut(), discovery_storage.as_deref())
                {
                    let now_unix = crate::discovery::now_unix_secs();
                    let live = crate::discovery::list_discovered_interfaces(storage_root, now_unix);

                    // #151: operator-configured IFAC = any registered IFAC
                    // that did not arrive via this spawner (whose IFAC can be
                    // discovery-sourced and must not close an open node).
                    let operator_ifac_present = ifac_configs
                        .keys()
                        .any(|k| !autoconnect_spawned_ids.contains(k));
                    let mut spawner = AutoConnectLiveSpawner {
                        next_id: &autoconnect_wiring.next_id,
                        new_iface_tx: &autoconnect_wiring.new_iface_tx,
                        reconnect_tx: &autoconnect_wiring.reconnect_tx,
                        corrupt_every: autoconnect_wiring.corrupt_every,
                        outbound_socket_hook: autoconnect_wiring.outbound_socket_hook.clone(),
                        stats: &iface_stats_map,
                        teardown_ids: Vec::new(),
                        heard_ifac: &discovery_heard_ifac,
                        operator_ifac_present,
                        spawned_ids: &mut autoconnect_spawned_ids,
                        refused_warned: &mut autoconnect_refused_warned,
                    };
                    manager.poll(&live, now_unix, &mut spawner);
                    let teardown_ids = spawner.teardown_ids;

                    // Complete each requested teardown through the same cleanup
                    // path a hard `Disconnected` uses, so path/link state and
                    // the per-interface maps are consistently torn down.
                    for iface_id in teardown_ids {
                        tracing::info!(
                            "discovery: tearing down auto-connected interface {} ({})",
                            iface_id,
                            registry.name_of(iface_id),
                        );
                        live_peer_counts.remove(&iface_id.0);
                        let output = {
                            let mut core = inner.lock_recover();
                            core.handle_interface_down(iface_id)
                        };
                        let processor_delay = dispatch_output(
                            output,
                            &mut registry,
                            event_sink.as_mut(),
                            &inner,
                            &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs,
                            remote_mgmt.as_ref(),
                            discovery_storage.as_deref(),
                            discovery_network_identity.as_deref(),
                            &mut discovery_heard_ifac,
                            &completions,
                            core_processor.as_mut(),
                        );
                        tighten_next_poll(&mut next_poll, processor_delay);
                        retry_queues.remove(&iface_id.0);
                        registry.remove(iface_id);
                        retire_from_inventory(
                            &inventory,
                            &iface_stats_map,
                            &local_client_count,
                            iface_id.0,
                        );
                        {
                            let mut stats = iface_stats_map.lock_recover();
                            stats.remove(&iface_id.0);
                        }
                    }
                }
                next_autoconnect = tokio::time::Instant::now() + AUTOCONNECT_POLL_INTERVAL;
            }

            // Branch 9: Periodic interface-discovery announcer (Codeberg #107).
            //
            // Self-advertise discoverable interfaces on
            // `rnstransport.discovery.interface` so a Python `rnsd` (or another
            // lnsd) discovers this node autonomously. Every job interval, pick
            // the most-overdue due interface and announce its pre-stamped
            // payload -- one interface per tick, matching Python
            // `InterfaceAnnouncer.job`. Armed only while discovery is enabled.
            _ = async {
                match discovery_announce_wake {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => core::future::pending::<()>().await,
                }
            } => {
                if let Some(wiring) = discovery_announce.as_mut() {
                    let now = tokio::time::Instant::now();
                    // Due = never announced, or spacing elapsed. Pick the most
                    // overdue (largest time since last announce), like Python's
                    // `sort(key=now-last, reverse=True)[0]`.
                    let selected = wiring
                        .jobs
                        .iter_mut()
                        .filter(|j| match j.last_announce {
                            None => true,
                            Some(last) => now.duration_since(last) >= j.interval,
                        })
                        .max_by_key(|j| match j.last_announce {
                            None => Duration::MAX,
                            Some(last) => now.duration_since(last),
                        });

                    if let Some(job) = selected {
                        job.last_announce = Some(now);
                        let app_data = job.app_data.clone();
                        let label = job.label.clone();
                        let output = {
                            let mut core = inner.lock_recover();
                            core.announce_destination(&wiring.dest_hash, Some(&app_data))
                        };
                        match output {
                            Ok(output) => {
                                tracing::debug!(
                                    "discovery: self-advertised interface \"{}\" ({}B)",
                                    label,
                                    app_data.len(),
                                );
                                let processor_delay = dispatch_output(
                                    output,
                                    &mut registry,
                                    event_sink.as_mut(),
                                    &inner,
                                    &mut retry_queues, &mut retry_queue_warned, &mut retry_queue_max_depth, &ifac_configs,
                                    remote_mgmt.as_ref(),
                                    discovery_storage.as_deref(),
                                    discovery_network_identity.as_deref(),
                                    &mut discovery_heard_ifac,
                                    &completions,
                                    core_processor.as_mut(),
                                );
                                tighten_next_poll(&mut next_poll, processor_delay);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "discovery: self-advertise announce for \"{}\" failed: {e:?}",
                                    label,
                                );
                            }
                        }
                    }

                    next_discovery_announce = Some(tokio::time::Instant::now() + wiring.job_interval);
                }
            }
        }
    }

    // No event can be observed past this point: resolve every pending
    // completion waiter with NodeStopped rather than letting it park on a
    // node that will never answer (C5), after the shutdown drain above has
    // dispatched — and observed — everything still queued. The close itself
    // rides `_close_on_exit`'s Drop (armed at loop entry), so a panic
    // unwinding out of the loop resolves waiters the same way an orderly
    // exit does — a parked future must not outlive the loop task either way.
}

/// Production [`AutoConnectSpawner`](crate::autoconnect::AutoConnectSpawner):
/// spawns discovered TCP endpoints as reconnecting TCP-client interfaces and
/// registers them with the running event loop via `new_iface_tx`, exactly like
/// the static and hot-plug interface paths. Teardown is deferred: the manager
/// records the ids and the event loop completes removal through the shared
/// `Disconnected` cleanup so path/link state stays consistent.
struct AutoConnectLiveSpawner<'a> {
    next_id: &'a Arc<AtomicUsize>,
    new_iface_tx: &'a mpsc::Sender<InterfaceHandle>,
    reconnect_tx: &'a mpsc::Sender<InterfaceId>,
    corrupt_every: Option<u64>,
    outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
    /// Shared counters per registered interface; carries the live online
    /// state the spawned client tasks maintain (L-0020).
    stats: &'a InterfaceStatsMap,
    teardown_ids: Vec<InterfaceId>,
    /// #151: hearing-interface IFAC per discovered endpoint (inherit rule).
    heard_ifac: &'a HeardIfacMap,
    /// #151: whether any operator-configured IFAC exists on this node
    /// (discovery-sourced IFAC on auto-connected clients excluded).
    operator_ifac_present: bool,
    /// #151: ids of every interface this spawner path ever spawned.
    spawned_ids: &'a mut std::collections::BTreeSet<usize>,
    /// #151: endpoints already warned about as refused (fail closed).
    refused_warned: &'a mut std::collections::BTreeSet<[u8; leviculum_core::discovery::STAMP_SIZE]>,
}

impl crate::autoconnect::AutoConnectSpawner for AutoConnectLiveSpawner<'_> {
    fn spawn_tcp_client(
        &mut self,
        name: &str,
        host: &str,
        port: u16,
        rec: &leviculum_core::discovery::DiscoveredInterfaceRecord,
    ) -> Option<InterfaceId> {
        // #151: resolve the IFAC this client must carry BEFORE any socket
        // work; a refusal (fail closed) must not open a connection at all.
        let ifac = match resolve_autoconnect_ifac(
            rec.ifac_netname.as_deref(),
            rec.ifac_netkey.as_deref(),
            self.heard_ifac.get(&rec.discovery_hash),
            self.operator_ifac_present,
        ) {
            AutoConnectIfac::Open => None,
            AutoConnectIfac::Protected(cfg) => Some(*cfg),
            AutoConnectIfac::Refused { reason } => {
                if self.refused_warned.insert(rec.discovery_hash) {
                    tracing::warn!(
                        "discovery: NOT auto-connecting {} \"{}\" at {}:{}: {} (fail closed)",
                        rec.interface_type,
                        rec.name,
                        host,
                        port,
                        reason,
                    );
                }
                return None;
            }
        };
        // Fast path: a literal IP endpoint (the common discovery case) parses
        // without touching the resolver. Fall back to a name lookup otherwise.
        // addr resolution moved to reconnect loop
        let id = InterfaceId(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        let mut handle = spawn_tcp_client_with_reconnect(TcpClientConfig {
            id,
            name: name.to_string(),
            target_host: host.to_string(),
            target_port: port,
            buffer_size: TCP_DEFAULT_BUFFER_SIZE,
            corrupt_every: self.corrupt_every,
            reconnect_interval: Duration::from_secs(5),
            max_reconnect_tries: None,
            reconnect_max_interval: DEFAULT_RECONNECT_MAX_INTERVAL,
            connect_timeout: DEFAULT_TCP_CONNECT_TIMEOUT,
            reconnect_notify: Some(self.reconnect_tx.clone()),
            // Auto-connected (discovered) TCP clients do not yet initiate the
            // tunnel synthesize handshake: their core-side interface hash is not
            // registered on this dynamic path (Codeberg #64 covers static TCP
            // clients). They still respond to peer-initiated tunnels.
            tunnel_notify: None,
            socks_target: None,
            shutdown: None,
            outbound_socket_hook: self.outbound_socket_hook.clone(),
        });
        // #151: carry the resolved IFAC on the handle; the dynamic-
        // registration branch applies `info.ifac` to core + ifac_configs
        // exactly like a server-accepted child (the fc1bae4 mechanism).
        if let Some(cfg) = ifac {
            tracing::info!(
                "discovery: auto-connect \"{}\" carries IFAC ({})",
                rec.name,
                if rec.ifac_netname.is_some() || rec.ifac_netkey.is_some() {
                    "advertised by the discovered peer"
                } else {
                    "inherited from the hearing interface"
                },
            );
            handle.info.ifac = Some(cfg);
        }
        self.spawned_ids.insert(id.0);
        // Register with the running loop; the `new_interface_rx` branch does
        // the map/announce bookkeeping on the next iteration.
        self.new_iface_tx.try_send(handle).ok()?;
        Some(id)
    }

    fn teardown(&mut self, id: InterfaceId) {
        self.teardown_ids.push(id);
    }

    fn is_online(&self, id: InterfaceId) -> bool {
        self.stats
            .lock_recover()
            .get(&id.0)
            .map(|c| c.is_online())
            .unwrap_or(false)
    }
}

/// Bounded graceful flush of the interface outgoing queues during shutdown
/// (Codeberg #77). After the shutdown drain has dispatched queued outputs to
/// the interfaces, their tasks still need to pop each packet and `write_all`
/// it to the socket. This waits for every interface's outgoing channel to
/// drain, then a short [`SHUTDOWN_FLUSH_MARGIN`] for the final in-flight write,
/// before returning and letting the runtime abort the tasks. Bounded by
/// [`SHUTDOWN_FLUSH_BOUND`] so a wedged or back-pressured interface cannot hang
/// teardown. Returns immediately when nothing is queued (clean teardown).
async fn flush_outgoing_on_shutdown(registry: &InterfaceRegistry) {
    fn pending(registry: &InterfaceRegistry) -> usize {
        registry
            .handles()
            .iter()
            .map(|h| h.outgoing.max_capacity() - h.outgoing.capacity())
            .sum()
    }

    if pending(registry) == 0 {
        return;
    }
    let deadline = tokio::time::Instant::now() + SHUTDOWN_FLUSH_BOUND;
    loop {
        let remaining = pending(registry);
        if remaining == 0 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "shutdown flush bound ({} ms) reached with {} packet(s) still queued",
                SHUTDOWN_FLUSH_BOUND.as_millis(),
                remaining,
            );
            return;
        }
        // Yields to the co-scheduled interface tasks so they pop and write.
        tokio::time::sleep(SHUTDOWN_FLUSH_POLL).await;
    }
    // The outgoing channels are empty: the interface tasks have popped every
    // packet, but the last write_all may still be in flight. Yield a short
    // margin so it completes before the runtime aborts the task.
    tokio::time::sleep(SHUTDOWN_FLUSH_MARGIN).await;
}

/// The deadline the driver schedules against when the core's own output and
/// the processor's `on_tick` output are dispatched separately.
///
/// Codeberg #196: the two used to be folded together with `TickOutput::merge`
/// before the deadline was read, so the driver saw one `next_deadline_ms`. The
/// merge had to go — it also routed the processor's synthetic events into the
/// tap, the `/status` responder and the discovery registry — but the
/// *scheduling* half of it was correct and has to survive the split verbatim.
/// This is that half, lifted out so it can be pinned against
/// `TickOutput::merge` directly rather than argued
/// (`merged_next_deadline_matches_what_tickoutput_merge_computed`).
fn merged_next_deadline(core_ms: Option<u64>, processor_ms: Option<u64>) -> Option<u64> {
    match (core_ms, processor_ms) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Bring `next_poll` forward when a processor asked to be woken sooner.
///
/// Codeberg #196: `on_tick`'s `next_deadline_ms` was honoured by the timer
/// branch while `on_event`'s was read by nobody, which made the same field
/// mean two different things depending on which hook filled it in. This is the
/// `on_event` half; `dispatch_output` returns the delay because that is where
/// the tap runs and where `now_ms` is already in hand.
fn tighten_next_poll(next_poll: &mut tokio::time::Instant, delay: Option<Duration>) {
    if let Some(delay) = delay {
        let wake_at = tokio::time::Instant::now() + delay;
        if wake_at < *next_poll {
            *next_poll = wake_at;
        }
    }
}

/// Dispatch a TickOutput: drain retry queues, route Actions to interfaces, forward Events.
///
/// `event_sink` is `None` when the node was built with `without_events()`;
/// in that case, `output.events` is dropped at the end of this function
/// without being forwarded — identical to the NRF daemon path, where
/// the events vector simply falls out of scope.
///
/// Returns how long the caller may wait before polling again, when the
/// registered processor's event tap asked for a deadline. `None` means "no
/// request"; the caller keeps whatever it had. Fold it in with
/// [`tighten_next_poll`].
#[allow(clippy::too_many_arguments)]
fn dispatch_output(
    output: TickOutput,
    registry: &mut InterfaceRegistry,
    mut event_sink: Option<&mut EventSink>,
    inner: &Arc<Mutex<StdNodeCore>>,
    retry_queues: &mut BTreeMap<usize, VecDeque<Vec<u8>>>,
    retry_queue_warned: &mut std::collections::BTreeSet<usize>,
    retry_queue_max_depth: &mut BTreeMap<usize, usize>,
    ifac_configs: &BTreeMap<usize, leviculum_core::ifac::IfacConfig>,
    remote_mgmt: Option<&RemoteMgmtResponder>,
    discovery_storage: Option<&Path>,
    discovery_network_identity: Option<&leviculum_core::Identity>,
    discovery_heard_ifac: &mut HeardIfacMap,
    completions: &CompletionRegistry,
    core_processor: Option<&mut processor::ProcessorSlot>,
) -> Option<Duration> {
    // Drain retry queues before dispatching new actions
    let drain_now_ms = inner.lock_recover().now_ms();
    drain_retry_queues(retry_queues, registry, drain_now_ms);

    // Dispatch new actions to interfaces (protocol logic in core)
    let mut ifaces: Vec<&mut dyn leviculum_core::traits::Interface> = registry
        .handles_mut_slice()
        .iter_mut()
        .map(|h| h as &mut dyn leviculum_core::traits::Interface)
        .collect();
    let result =
        leviculum_core::transport::dispatch_actions(&mut ifaces, output.actions, ifac_configs);

    // Log dispatch errors.
    //
    // #25 — a `Disconnected` error here means the frame was DESTROYED: the
    // interface died with it in flight and the driver cannot re-home it (its
    // link died too). Logging alone made that loss invisible above the driver,
    // so a consumer's (correct) retry/backoff path was never told the dispatch
    // failed and could never engage. Count the losses per interface and EMIT
    // them, so the sender can re-send on a fresh link.
    let mut disconnected_drops: BTreeMap<usize, usize> = BTreeMap::new();
    for (iface_id, error) in &result.errors {
        match error {
            InterfaceError::BufferFull => {
                tracing::trace!("Interface {} buffer full", iface_id);
            }
            InterfaceError::Disconnected => {
                tracing::warn!("Interface {} disconnected during dispatch", iface_id);
                *disconnected_drops.entry(iface_id.0).or_default() += 1;
            }
        }
    }
    let drop_events: Vec<NodeEvent> = disconnected_drops
        .into_iter()
        .map(|(iface_id, count)| {
            tracing::warn!(
                "Interface {} destroyed {} in-flight frame(s) on dispatch — the sender MUST \
                 re-send on a fresh link (#25)",
                iface_id,
                count
            );
            NodeEvent::FramesDropped {
                iface_id,
                count,
                reason: FrameDropReason::DispatchDisconnected,
            }
        })
        .collect();

    // Completion futures (leviculum#42): observed here — the one place every
    // event flows, ahead of `EventSink::emit` — so waiters resolve in daemon
    // mode (`event_sink` None) too. The node lock is NOT held at this point;
    // the registry is a leaf and must stay one (see completions.rs).
    for ev in drop_events.iter().chain(output.events.iter()) {
        completions.observe(ev);
    }

    // Queue SendPacket retries (with cap enforcement)
    for retry in result.retries {
        let iface_idx = retry.iface_idx;
        let queue = retry_queues.entry(iface_idx).or_default();
        if queue.len() >= RETRY_QUEUE_CAP {
            queue.pop_front();
            tracing::warn!(
                "Retry queue full for iface {}, dropping oldest packet",
                iface_idx,
            );
        }
        push_retry_with_warn(
            queue,
            iface_idx,
            retry.data,
            retry_queue_warned,
            retry_queue_max_depth,
        );
    }

    // Remove empty queues to avoid accumulating stale entries.
    // Transport reads per-interface readiness from the
    // interface_next_slot_ms backchannel.
    retry_queues.retain(|_, queue| !queue.is_empty());

    // Clear the per-queue warned flag when the queue drops back
    // below RETRY_QUEUE_DEPTH_WARN so a future re-crossing re-emits
    // the warning. Also drop entries for queues that no longer exist.
    retry_queue_warned.retain(|idx| {
        retry_queues
            .get(idx)
            .map(|q| q.len() >= RETRY_QUEUE_DEPTH_WARN)
            .unwrap_or(false)
    });

    // Push per-interface next_slot_ms + max_airtime_ms into the
    // Transport backchannels. Transport can't hold handles
    // sans-I/O), so the driver mirrors both quantities here.
    // next_slot_ms is read by the announce-retry / direct-send
    // path; max_airtime_ms feeds the jitter-window helper that
    // scales announce retry randomness with the slowest link's
    // airtime.
    push_interface_state(registry, inner);

    // Remote-management `/status` responder (Codeberg #86). Runs even in
    // daemon mode: it consumes `RequestReceived` straight from the raw
    // `TickOutput` rather than the forwarded event stream, so it works with
    // `without_events()`. The core has already applied the destination and
    // allow-list checks before emitting the event, so any request that
    // reaches here is authorised. The response `TickOutput` is dispatched
    // after event forwarding to keep the borrow of `output.events` short.
    let mut mgmt_responses: Vec<TickOutput> = Vec::new();
    if let Some(responder) = remote_mgmt {
        for event in &output.events {
            if let NodeEvent::RequestReceived {
                link_id,
                request_id,
                path,
                data,
                ..
            } = event
            {
                if let Some(resp) =
                    responder.handle_request(inner, link_id, request_id, path, data, completions)
                {
                    mgmt_responses.push(resp);
                }
            }
        }
    }

    // Discovered-interface registry (Codeberg #32): a validated announce on the
    // `rnstransport.discovery.interface` destination is persisted as a
    // discovered-interface record. Detection is by the destination's name hash,
    // so it stays independent of the announcing node's identity (like Python's
    // aspect-filtered announce handler). Reads the same raw `output.events` as
    // the mgmt responder, so it works in daemon mode (no app event sink).
    if let Some(storage_root) = discovery_storage {
        for event in &output.events {
            if let NodeEvent::AnnounceReceived {
                announce,
                interface_index,
            } = event
            {
                record_discovery_announce(
                    inner,
                    storage_root,
                    announce,
                    discovery_network_identity,
                    *interface_index,
                    ifac_configs,
                    discovery_heard_ifac,
                );
            }
        }
    }

    // Registered core processor (Codeberg #196). Reads the SAME raw
    // `output.events` as the mgmt responder and the discovery registry, and
    // for the same reason plus a sharper one: seven of the event types LXMF
    // needs — `PacketReceived` and `LinkDataReceived` among them, i.e. how a
    // message arrives — are `EventClass::Data`, so the sink below is entitled
    // to drop them. A processor fed from the forwarded stream would lose
    // inbound messages with nothing underneath to retransmit them. The tap
    // therefore has to be here, ahead of `EventSink::emit`.
    //
    // The driver's own `FramesDropped` notices are tapped ALONGSIDE the core's
    // events, in the order the sink will see them. They are built above, in
    // this same call, and never pass through `handle_packet` — so the tap's
    // "everything your actions cause comes back on a later tick" only holds if
    // they are fed in here. #25's whole point is that the sender must re-send
    // on a fresh link, and a processor that issued the `SendPacket` is the
    // sender.
    //
    // Its output is dispatched after event forwarding, with the processor
    // detached — see `processor::run_event_tap` for the recursion bound.
    let tap_events: Vec<&NodeEvent> = drop_events.iter().chain(output.events.iter()).collect();
    let processor_output =
        core_processor.and_then(|slot| processor::run_event_tap(slot, inner, &tap_events));
    drop(tap_events);
    let processor_deadline = processor_output
        .as_ref()
        .and_then(|out| out.next_deadline_ms)
        .map(|deadline_ms| Duration::from_millis(deadline_ms.saturating_sub(drain_now_ms)));

    // Forward events to the application via the split-plane EventSink:
    // control events lossless-by-default (overflow surfaced via
    // ControlPlaneOverflow), data events droppable under load (Codeberg #71).
    // When event_sink is None (daemon-mode, built via `without_events()`),
    // events are dropped here without forwarding — the events vector
    // simply falls out of scope at the end of this function.
    if let Some(event_sink) = event_sink.as_deref_mut() {
        // #25 — the frames-destroyed signal rides the SAME sink as core's own
        // events, so a consumer learns of the loss on the stream it already
        // reads. Emitted first: the loss happened before anything core queued
        // here, and a sender reacting to it should not have to wait behind them.
        for event in drop_events {
            event_sink.emit(event);
        }
        for event in output.events {
            if let NodeEvent::LinkEstablished { link_id, .. } = &event {
                tracing::debug!("Link established: {:?}", link_id);
            }
            event_sink.emit(event);
        }
    }

    // Dispatch the `/status` responses produced above. `remote_mgmt` is None
    // on the recursive call: a response carries no `RequestReceived`, so this
    // never recurses further.
    for resp in mgmt_responses {
        let _ = dispatch_output(
            resp,
            registry,
            None,
            inner,
            retry_queues,
            retry_queue_warned,
            retry_queue_max_depth,
            ifac_configs,
            None,
            // Management responses carry no announces; no discovery persistence.
            None,
            None,
            discovery_heard_ifac,
            completions,
            // The `/status` response is the driver's own; it is not the
            // processor's business and must not re-enter the tap.
            None,
        );
    }

    // Dispatch the processor's answer on the driver's own send path — the same
    // `dispatch_actions` call its own outputs take, in this same tick. That is
    // what lets `PacketProofRequested`/`LinkProofRequested`/`ResourceAdvertised`
    // be answered synchronously rather than deferred to a later tick.
    //
    // `core_processor: None` on the recursive call is the recursion bound:
    // depth is exactly one, so a processor never sees the events it emitted.
    if let Some(resp) = processor_output {
        let _ = dispatch_output(
            resp,
            registry,
            // The processor's answer produces ordinary core events (a
            // `ResourceTransferStarted` behind an `accept_resource`, say).
            // They belong on the application's stream like any other.
            event_sink,
            inner,
            retry_queues,
            retry_queue_warned,
            retry_queue_max_depth,
            ifac_configs,
            None,
            None,
            None,
            discovery_heard_ifac,
            completions,
            None,
        );
    }

    processor_deadline
}

/// Persist a discovery announce into the discovered-interface registry, if it
/// is one. Filters by the `rnstransport.discovery.interface` destination name
/// hash, validates+decodes the announce `app_data` (PoW stamp check), and
/// writes the record under `<storage>/discovery/interfaces` (Codeberg #32).
///
/// Non-discovery announces (the overwhelming majority) are rejected by the
/// name-hash compare before any parsing, so this stays cheap on the hot path.
#[allow(clippy::too_many_arguments)]
fn record_discovery_announce(
    inner: &Arc<Mutex<StdNodeCore>>,
    storage_root: &Path,
    announce: &leviculum_core::ReceivedAnnounce,
    network_identity: Option<&leviculum_core::Identity>,
    interface_index: usize,
    ifac_configs: &BTreeMap<usize, leviculum_core::ifac::IfacConfig>,
    heard_ifac: &mut HeardIfacMap,
) {
    use leviculum_core::discovery::{APP_NAME, DISCOVERY_ASPECTS, MIN_REQUIRED_STAMP_VALUE};

    let discovery_name_hash =
        leviculum_core::Destination::compute_name_hash(APP_NAME, &DISCOVERY_ASPECTS);
    if announce.name_hash() != &discovery_name_hash {
        return;
    }

    let network_id = announce.computed_identity_hash();
    // On a private discovery network, decrypt encrypted announces with the
    // configured network identity before validation (Codeberg #32, sub-task d);
    // without one, only plaintext announces decode.
    //
    // The gate is `MIN_REQUIRED_STAMP_VALUE` (14), not the value we mint at
    // (16): 1.3.5 peers on the air still mint at 14, and requiring what we mint
    // would drop every one of them (Codeberg #328).
    let parsed = match network_identity {
        Some(identity) => leviculum_core::discovery::parse_announce_app_data_decrypt(
            announce.app_data(),
            &network_id,
            MIN_REQUIRED_STAMP_VALUE,
            identity,
        ),
        None => leviculum_core::discovery::parse_announce_app_data(
            announce.app_data(),
            &network_id,
            MIN_REQUIRED_STAMP_VALUE,
        ),
    };
    let Some(di) = parsed else {
        tracing::debug!(
            "discovery: announce on discovery destination failed validation \
             dest={} app_data_len={} iface={}",
            announce.destination_hash(),
            announce.app_data().len(),
            interface_index,
        );
        return;
    };

    // #151: remember the hearing interface's IFAC so an auto-connect to this
    // endpoint can inherit it (AutoInterface.py:559-561 parent-child rule).
    // Insert-only: once an endpoint was found through a protected interface,
    // a later sighting on an open interface must not downgrade it to an open
    // auto-connect (fail closed on ambiguity).
    if let Some(cfg) = ifac_configs.get(&interface_index) {
        heard_ifac.insert(di.discovery_hash, cfg.clone());
    }

    let hops = inner
        .lock_recover()
        .hops_to(announce.destination_hash())
        .map(|h| h as u32)
        .unwrap_or(1);
    let now = crate::discovery::now_unix_secs();

    if let Err(e) = crate::discovery::persist_discovered(storage_root, &di, hops, now) {
        tracing::warn!("discovery: failed to persist discovered interface: {e}");
    } else {
        tracing::debug!(
            "discovery: stored {} \"{}\" ({} hop(s), stamp value {})",
            di.interface_type,
            di.name,
            hops,
            di.value
        );
    }
}

/// Append `data` to the per-interface retry queue. Emit a single
/// tracing::warn when the queue depth first crosses
/// `RETRY_QUEUE_DEPTH_WARN`; update the monotonic max-depth high-
/// watermark and log at info! whenever it increases.
fn push_retry_with_warn(
    queue: &mut VecDeque<Vec<u8>>,
    iface_idx: usize,
    data: Vec<u8>,
    warned: &mut std::collections::BTreeSet<usize>,
    max_depth: &mut BTreeMap<usize, usize>,
) {
    let len_before = queue.len();
    queue.push_back(data);
    if len_before < RETRY_QUEUE_DEPTH_WARN
        && queue.len() == RETRY_QUEUE_DEPTH_WARN
        && !warned.contains(&iface_idx)
    {
        tracing::warn!(
            iface = iface_idx,
            depth = queue.len(),
            "retry queue depth high, first-order backpressure may be mis-tuned"
        );
        warned.insert(iface_idx);
    }
    // E2: monotonic max-depth watermark. Log at info! only when the
    // watermark actually advances, benchmarks can grep for this.
    let prev = max_depth.get(&iface_idx).copied().unwrap_or(0);
    if queue.len() > prev {
        max_depth.insert(iface_idx, queue.len());
        tracing::info!(
            iface = iface_idx,
            max_depth = queue.len(),
            "retry_queue max depth increased"
        );
    }
}

/// Compute the next wall-clock deadline at which any packet in the
/// retry queues becomes eligible to drain. Returns the MINIMUM over
/// all non-empty queues of `handle.next_slot_ms(front.len(), now)`.
/// `None` iff every retry queue is empty.
///
/// Used by run_event_loop to schedule a sleep_until arm so idle nodes
/// with retry-queued packets still drain at the right moment, no
/// polling, no fixed 500 ms fallback.
fn compute_retry_wake_deadline_ms(
    retry_queues: &BTreeMap<usize, VecDeque<Vec<u8>>>,
    registry: &InterfaceRegistry,
    now_ms: u64,
) -> Option<u64> {
    use leviculum_core::traits::Interface;
    let mut min_slot: Option<u64> = None;
    for (&iface_idx, queue) in retry_queues.iter() {
        let Some(front) = queue.front() else { continue };
        if let Some(handle) = registry.handles().iter().find(|h| h.id().0 == iface_idx) {
            let slot = handle.next_slot_ms(front.len(), now_ms);
            // Only count slots strictly in the future; ready slots don't
            // need waking, they'd drain at the next normal dispatch tick.
            if slot > now_ms {
                match min_slot {
                    Some(current) if slot < current => min_slot = Some(slot),
                    None => min_slot = Some(slot),
                    _ => {}
                }
            } else {
                // A ready queue head means we can drain NOW, return
                // None so the caller doesn't sleep at all.
                return None;
            }
        }
    }
    min_slot
}

/// Drain per-interface retry queues in-place, honouring per-packet
/// airtime gating. Before calling try_send, ask the handle's
/// `next_slot_ms` for the actual packet size. Transport's MTU-sized
/// backchannel cache is conservative for smaller packets, and the
/// drain's finer granularity recovers that headroom. Extracted so it
/// is unit-testable without spinning up the full driver.
fn drain_retry_queues(
    retry_queues: &mut BTreeMap<usize, VecDeque<Vec<u8>>>,
    registry: &mut InterfaceRegistry,
    now_ms: u64,
) {
    use leviculum_core::traits::Interface;
    for (iface_idx, queue) in retry_queues.iter_mut() {
        let iface_id = InterfaceId(*iface_idx);
        while let Some(data) = queue.front() {
            if let Some(handle) = registry
                .handles_mut_slice()
                .iter_mut()
                .find(|h| h.id() == iface_id)
            {
                if handle.next_slot_ms(data.len(), now_ms) > now_ms {
                    // Interface not yet ready for THIS packet size, leave
                    // it at the front, try next dispatch tick (driver-local
                    // wake in E3 will fire at the computed slot).
                    break;
                }
                // Retry queue only holds SendPacket data (directed traffic),
                // which is always high priority.
                match handle.try_send_prioritized(data, true) {
                    Ok(()) => {
                        queue.pop_front();
                    }
                    Err(InterfaceError::BufferFull) => break,
                    Err(InterfaceError::Disconnected) => {
                        queue.clear();
                        break;
                    }
                }
            } else {
                // Interface removed, clear queue
                queue.clear();
                break;
            }
        }
    }
}

/// Mirror each interface's per-tick state into Transport's
/// backchannels. Pushes `next_slot_ms(MTU, now_ms)` for the
/// readiness cache and, for LoRa-Serial interfaces with an airtime
/// credit bucket, the worst-case airtime that drives the jitter
/// window for announce retries. Non-LoRa interfaces have
/// `credit == None` and are simply skipped for the airtime push;
/// Transport's getter falls back to the legacy floor when no
/// interface contributes.
///
/// Extracted so it is unit-testable without spinning up the full
/// driver; called from `dispatch_output`.
fn push_interface_state(registry: &mut InterfaceRegistry, inner: &Arc<Mutex<StdNodeCore>>) {
    use leviculum_core::traits::Interface;
    let now_ms = inner.lock_recover().now_ms();
    let mut core = inner.lock_recover();
    for handle in registry.handles_mut_slice().iter_mut() {
        let mtu = handle.mtu();
        let iface_idx = handle.id().0;
        let slot = handle.next_slot_ms(mtu, now_ms);
        core.set_interface_next_slot_ms(iface_idx, slot);
        if let Some(credit) = handle.credit.as_ref() {
            let max_airtime = credit.lock_recover().max_airtime_ms();
            core.set_interface_max_airtime_ms(iface_idx, max_airtime);
        }
    }
}

/// Phases 1+2 of the periodic storage flush (leviculum#44): a brief node-lock
/// hold to snapshot the dirty state, then the file IO on the blocking pool.
/// `None` when nothing is dirty. The returned handle is the overlap guard;
/// the caller must settle it — including on the shutdown path — before the
/// synchronous shutdown flush may run.
fn begin_flush(
    inner: &Arc<Mutex<StdNodeCore>>,
) -> Option<tokio::task::JoinHandle<crate::storage::FlushOutcome>> {
    let snapshot = inner.lock_recover().storage_mut().take_flush_snapshot()?;
    Some(tokio::task::spawn_blocking(move || snapshot.write()))
}

/// Phase 3: brief re-lock to clear the dirty flags the write covered.
/// A JoinError leaves the flags set, so the next interval retries.
fn settle_flush(
    inner: &Arc<Mutex<StdNodeCore>>,
    joined: std::result::Result<crate::storage::FlushOutcome, tokio::task::JoinError>,
) {
    match joined {
        Ok(outcome) => inner.lock_recover().storage_mut().finish_flush(outcome),
        Err(e) => tracing::error!("Storage flush write task failed: {e}"),
    }
}

/// Map a configured interface type (Python-style class name) to its transport
/// medium, for status reporting.
fn kind_from_interface_type(interface_type: &str) -> leviculum_core::traits::InterfaceKind {
    use leviculum_core::traits::InterfaceKind;
    match interface_type {
        "TCPClientInterface" | "TCPServerInterface" => InterfaceKind::Tcp,
        "UDPInterface" => InterfaceKind::Udp,
        "I2PInterface" => InterfaceKind::I2p,
        "SerialInterface" => InterfaceKind::Serial,
        "RNodeInterface" | "RNodeMultiInterface" => InterfaceKind::Rnode,
        "KISSInterface" | "AX25KISSInterface" => InterfaceKind::Kiss,
        "PipeInterface" => InterfaceKind::Pipe,
        "AutoInterface" => InterfaceKind::Auto,
        "BLEInterface" => InterfaceKind::Ble,
        "LocalInterface" | "LocalServerInterface" | "LocalClientInterface" => InterfaceKind::Local,
        _ => InterfaceKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codeberg #293: pin `DEFAULT_IFAC_SIZE` for **every** interface type we
    /// build, so a newly supported type has to be classified deliberately
    /// instead of inheriting 16 from the fall-through arm.
    ///
    /// Why a whole-table pin and not two added names: an IFAC size mismatch is
    /// not a degradation, it is total rejection of the peer's frames in both
    /// directions, with no error naming the cause. The failure is
    /// indistinguishable from "the link never came up". Every entry below is
    /// quoted from `reference/Reticulum` (RNS 1.3.5) at the cited line.
    #[test]
    fn test_default_ifac_size_matches_python_reference() {
        // Every interface type `interface_build::build` dispatches on, plus the
        // Backbone aliases and the unrecognised case, with the reference line
        // each expectation is quoted from.
        //
        // Backbone never actually reaches `default_ifac_size` — `ini_config::
        // normalize_backbone_interface` rewrites it to the TCP types first —
        // but upstream all four classes are 16, so the rewrite is lossless for
        // IFAC purposes and asserting it keeps that true.
        //
        // WeaveInterface we do not build; it is here because upstream's value
        // is 16, so if it is ever added the fall-through happens to be right —
        // a fact worth recording rather than rediscovering.
        let table: &[(&str, usize, &str)] = &[
            ("AX25KISSInterface", 8, "AX25KISSInterface.py:70"),
            ("KISSInterface", 8, "KISSInterface.py:63"),
            ("PipeInterface", 8, "PipeInterface.py:57"),
            ("RNodeInterface", 8, "RNodeInterface.py:110"),
            ("RNodeMultiInterface", 8, "RNodeMultiInterface.py:137"),
            ("SerialInterface", 8, "SerialInterface.py:53"),
            ("AutoInterface", 16, "AutoInterface.py:50"),
            (
                "BLEInterface",
                16,
                "ble-reticulum@07d94130 BLEInterface.py DEFAULT_IFAC_SIZE",
            ),
            ("I2PInterface", 16, "I2PInterface.py:839"),
            ("TCPClientInterface", 16, "TCPInterface.py:77"),
            ("TCPServerInterface", 16, "TCPInterface.py:454"),
            ("UDPInterface", 16, "UDPInterface.py:42"),
            ("BackboneInterface", 16, "BackboneInterface.py:54"),
            ("BackboneClientInterface", 16, "BackboneInterface.py:508"),
            ("WeaveInterface", 16, "WeaveInterface.py:838"),
            (
                "SomeInterfaceInventedTomorrow",
                16,
                "deliberate `_` default",
            ),
        ];

        // Collected rather than asserted per row: a whole-table pin is only
        // useful if one wrong entry does not hide the rest.
        let wrong: Vec<String> = table
            .iter()
            .filter(|(iface, expected, _)| default_ifac_size(iface) != *expected)
            .map(|(iface, expected, cited)| {
                format!(
                    "{iface}: got {}, want {expected} per {cited}",
                    default_ifac_size(iface)
                )
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "IFAC default size diverges from the Python reference:\n  {}",
            wrong.join("\n  ")
        );

        // Guard the constants themselves, so the table above cannot be
        // satisfied by both families collapsing onto one value.
        assert_eq!(leviculum_core::constants::IFAC_DEFAULT_SIZE_SERIAL, 8);
        assert_eq!(leviculum_core::constants::IFAC_DEFAULT_SIZE_NETWORK, 16);
    }

    /// Codeberg #55: the EU lawful-by-default derives `lt_alock` from the TX
    /// frequency only when `airtime_limit_long` is absent; an explicit value
    /// (including the harness's `0`) always wins, and non-EU frequencies stay
    /// off.
    #[test]
    fn test_resolve_lt_alock_lawful_default() {
        // 869.525 MHz (P) with no explicit limit -> ETSI 10% = lt_alock 1000.
        assert_eq!(resolve_lt_alock(None, 869_525_000), Some(1000));
        // 868.1 MHz (M) with no explicit limit -> 1% = lt_alock 100.
        assert_eq!(resolve_lt_alock(None, 868_100_000), Some(100));

        // Explicit value wins over the auto-default (any value, incl. the
        // rig harness's explicit 0 = off in runner.rs).
        assert_eq!(resolve_lt_alock(Some(5.0), 869_525_000), Some(500));
        assert_eq!(resolve_lt_alock(Some(0.0), 869_525_000), Some(0));
        // Explicit value is honoured even on a non-EU frequency.
        assert_eq!(resolve_lt_alock(Some(2.0), 915_000_000), Some(200));

        // US / out-of-band frequency with no explicit limit -> stays off.
        assert_eq!(resolve_lt_alock(None, 915_000_000), None);
        // Guard gap with no explicit limit -> stays off.
        assert_eq!(resolve_lt_alock(None, 869_300_000), None);
    }

    fn auto_iface(
        discovery_port: Option<u16>,
        data_port: Option<u16>,
        enabled: bool,
    ) -> InterfaceConfig {
        InterfaceConfig {
            interface_type: "AutoInterface".to_string(),
            enabled,
            discovery_port,
            data_port,
            ..Default::default()
        }
    }

    /// Codeberg #7: distinct AutoInterface sections with distinct ports pass;
    /// a single section (default ports) passes; non-AutoInterface sections are
    /// ignored.
    #[test]
    fn validate_auto_ports_accepts_distinct_sections() {
        // Single default section.
        assert!(validate_auto_interface_ports(&[auto_iface(None, None, true)]).is_ok());

        // Two sections with distinct ports.
        let ok = vec![
            auto_iface(Some(29716), Some(42671), true),
            auto_iface(Some(30000), Some(43000), true),
        ];
        assert!(validate_auto_interface_ports(&ok).is_ok());

        // A default section plus an explicitly-distinct one.
        let ok2 = vec![
            auto_iface(None, None, true),
            auto_iface(Some(30000), Some(43000), true),
        ];
        assert!(validate_auto_interface_ports(&ok2).is_ok());
    }

    /// Codeberg #7: two sections sharing a discovery port (unicast split) are
    /// rejected with a clear message naming the port.
    #[test]
    fn validate_auto_ports_rejects_shared_discovery_port() {
        let bad = vec![
            auto_iface(Some(29716), Some(42671), true),
            auto_iface(Some(29716), Some(43000), true),
        ];
        let err = validate_auto_interface_ports(&bad).expect_err("shared discovery_port rejected");
        let msg = format!("{err}");
        assert!(msg.contains("discovery_port 29716"), "message: {msg}");
        assert!(msg.contains("SO_REUSEPORT"), "message: {msg}");
    }

    /// Codeberg #7: two sections sharing a data port (data split) are rejected.
    /// This also covers the default-vs-default collision (both omit ports).
    #[test]
    fn validate_auto_ports_rejects_shared_data_port() {
        let bad = vec![
            auto_iface(Some(29716), Some(42671), true),
            auto_iface(Some(30000), Some(42671), true),
        ];
        let err = validate_auto_interface_ports(&bad).expect_err("shared data_port rejected");
        assert!(format!("{err}").contains("data_port 42671"));

        // Two default sections collide on both ports (discovery reported first).
        let both_default = vec![auto_iface(None, None, true), auto_iface(None, None, true)];
        assert!(validate_auto_interface_ports(&both_default).is_err());
    }

    /// Codeberg #7: a disabled colliding section is ignored.
    #[test]
    fn validate_auto_ports_ignores_disabled_sections() {
        let cfgs = vec![
            auto_iface(Some(29716), Some(42671), true),
            auto_iface(Some(29716), Some(42671), false),
        ];
        assert!(validate_auto_interface_ports(&cfgs).is_ok());
    }

    /// Codeberg #90: build_ifac_config derives an IFAC only when a
    /// network_name and/or passphrase is present, picks the Python per-type
    /// DEFAULT_IFAC_SIZE when ifac_size is unset (16 bytes for network
    /// interfaces, 8 for serial/RNode), and honours an explicit size.
    #[test]
    fn build_ifac_config_semantics() {
        // Neither network_name nor passphrase → no IFAC (a lone ifac_size is a
        // no-op, matching Python which needs a netname or netkey).
        let cfg = InterfaceConfig {
            interface_type: "TCPClientInterface".to_string(),
            ifac_size: Some(16),
            ..Default::default()
        };
        assert!(build_ifac_config(&cfg).is_none());

        // network_name only, no explicit size → TCP default of 16 bytes, and
        // the derived identity matches a direct construction at that size.
        let cfg = InterfaceConfig {
            interface_type: "TCPClientInterface".to_string(),
            networkname: Some("mynet".to_string()),
            ..Default::default()
        };
        let built = build_ifac_config(&cfg).expect("IFAC built");
        assert_eq!(built.ifac_size(), 16);
        let expected =
            leviculum_core::ifac::IfacConfig::new(Some("mynet"), None, 16).expect("valid");
        assert_eq!(built.identity().hash(), expected.identity().hash());

        // RNode default size is 8 bytes.
        let cfg = InterfaceConfig {
            interface_type: "RNodeInterface".to_string(),
            passphrase: Some("s3cret".to_string()),
            ..Default::default()
        };
        assert_eq!(build_ifac_config(&cfg).expect("IFAC built").ifac_size(), 8);

        // Explicit ifac_size (bytes) overrides the default.
        let cfg = InterfaceConfig {
            interface_type: "TCPClientInterface".to_string(),
            networkname: Some("mynet".to_string()),
            passphrase: Some("s3cret".to_string()),
            ifac_size: Some(8),
            ..Default::default()
        };
        let built = build_ifac_config(&cfg).expect("IFAC built");
        assert_eq!(built.ifac_size(), 8);
        let expected =
            leviculum_core::ifac::IfacConfig::new(Some("mynet"), Some("s3cret"), 8).expect("valid");
        assert_eq!(built.identity().hash(), expected.identity().hash());
    }

    /// A runtime attach must carry the config's IFAC on the handle (the
    /// startup config loop that registers IFAC by index never runs for it),
    /// and must not clobber an IFAC the builder already set.
    #[test]
    fn runtime_attach_carries_ifac_on_the_handle() {
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};

        fn dummy_handle(ifac: Option<leviculum_core::ifac::IfacConfig>) -> InterfaceHandle {
            let (_inc_tx, inc_rx) = mpsc::channel(1);
            let (out_tx, _out_rx) = mpsc::channel(1);
            InterfaceHandle {
                info: InterfaceInfo {
                    id: InterfaceId(7),
                    name: "udp_7".into(),
                    hw_mtu: None,
                    is_local_client: false,
                    bitrate: None,
                    tx_jitter_max_ms: None,
                    ifac,
                    mode: leviculum_core::traits::InterfaceMode::default(),
                    kind: leviculum_core::traits::InterfaceKind::Udp,
                    ingress_control: None,
                },
                incoming: inc_rx,
                outgoing: out_tx,
                counters: Arc::new(InterfaceCounters::new()),
                credit: None,
                ready: crate::interfaces::ReadySignal::ready_immediate(),
            }
        }

        let cfg = InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            networkname: Some("mynet".to_string()),
            passphrase: Some("s3cret".to_string()),
            ..Default::default()
        };

        let mut handles = vec![dummy_handle(None)];
        apply_runtime_ifac(&cfg, &mut handles);
        let applied = handles[0].info.ifac.as_ref().expect("IFAC applied");
        let expected = build_ifac_config(&cfg).expect("valid");
        assert_eq!(applied.identity().hash(), expected.identity().hash());

        // A pre-set IFAC (I2P children) survives untouched.
        let own = leviculum_core::ifac::IfacConfig::new(Some("othernet"), None, 16).expect("valid");
        let mut handles = vec![dummy_handle(Some(own.clone()))];
        apply_runtime_ifac(&cfg, &mut handles);
        assert_eq!(
            handles[0].info.ifac.as_ref().unwrap().identity().hash(),
            own.identity().hash()
        );

        // No IFAC keys → nothing applied.
        let plain = InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            ..Default::default()
        };
        let mut handles = vec![dummy_handle(None)];
        apply_runtime_ifac(&plain, &mut handles);
        assert!(handles[0].info.ifac.is_none());
    }

    /// Codeberg #151: the decided IFAC semantics for auto-connected discovered
    /// endpoints, in precedence order: advertised material wins, then the
    /// hearing interface's IFAC is inherited, then a node running IFAC refuses
    /// (fail closed), and a fully open node keeps connecting openly.
    #[test]
    fn autoconnect_ifac_resolution_follows_decided_semantics() {
        use leviculum_core::ifac::IfacConfig;

        let advertised = IfacConfig::new(
            Some("closednet"),
            Some("closedkey"),
            leviculum_core::constants::IFAC_DEFAULT_SIZE_NETWORK,
        )
        .expect("valid");
        let parent = IfacConfig::new(Some("parentnet"), Some("parentkey"), 24).expect("valid");

        // 1. Record-advertised IFAC protects the client, derived at the
        //    Backbone/TCP default size like Python `_add_interface`.
        match resolve_autoconnect_ifac(Some("closednet"), Some("closedkey"), None, false) {
            AutoConnectIfac::Protected(c) => {
                assert_eq!(c.identity().hash(), advertised.identity().hash())
            }
            _ => panic!("advertised IFAC must protect the spawned client"),
        }
        // ... and takes precedence over the hearing interface's IFAC.
        match resolve_autoconnect_ifac(Some("closednet"), Some("closedkey"), Some(&parent), true) {
            AutoConnectIfac::Protected(c) => {
                assert_eq!(c.identity().hash(), advertised.identity().hash())
            }
            _ => panic!("advertised IFAC must win over inheritance"),
        }

        // 2. No advertised IFAC -> inherit the hearing interface's config
        //    verbatim (including its non-default size).
        match resolve_autoconnect_ifac(None, None, Some(&parent), true) {
            AutoConnectIfac::Protected(c) => {
                assert_eq!(c.identity().hash(), parent.identity().hash())
            }
            _ => panic!("hearing-interface IFAC must be inherited"),
        }

        // 3. Nothing resolves but the operator runs IFAC -> refuse, named.
        match resolve_autoconnect_ifac(None, None, None, true) {
            AutoConnectIfac::Refused { reason } => assert!(!reason.is_empty()),
            _ => panic!("IFAC-running node must fail closed"),
        }

        // 4. Open network -> unchanged open behaviour.
        assert!(matches!(
            resolve_autoconnect_ifac(None, None, None, false),
            AutoConnectIfac::Open
        ));
    }

    /// Codeberg #151 spawner-level fixture: a live spawner wired to test
    /// channels, driven with a synthetic discovery record.
    struct SpawnerFixture {
        next_id: Arc<AtomicUsize>,
        new_iface_tx: mpsc::Sender<InterfaceHandle>,
        new_iface_rx: mpsc::Receiver<InterfaceHandle>,
        reconnect_tx: mpsc::Sender<InterfaceId>,
        _reconnect_rx: mpsc::Receiver<InterfaceId>,
        stats: crate::interfaces::InterfaceStatsMap,
        heard_ifac: HeardIfacMap,
        spawned_ids: std::collections::BTreeSet<usize>,
        refused_warned: std::collections::BTreeSet<[u8; 32]>,
    }

    impl SpawnerFixture {
        fn new() -> Self {
            let (new_iface_tx, new_iface_rx) = mpsc::channel(4);
            let (reconnect_tx, _reconnect_rx) = mpsc::channel(4);
            Self {
                next_id: Arc::new(AtomicUsize::new(100)),
                new_iface_tx,
                new_iface_rx,
                reconnect_tx,
                _reconnect_rx,
                stats: Arc::new(Mutex::new(BTreeMap::new())),
                heard_ifac: BTreeMap::new(),
                spawned_ids: std::collections::BTreeSet::new(),
                refused_warned: std::collections::BTreeSet::new(),
            }
        }

        fn spawn(
            &mut self,
            rec: &leviculum_core::discovery::DiscoveredInterfaceRecord,
            operator_ifac_present: bool,
        ) -> Option<InterfaceId> {
            use crate::autoconnect::AutoConnectSpawner as _;
            let mut spawner = AutoConnectLiveSpawner {
                next_id: &self.next_id,
                new_iface_tx: &self.new_iface_tx,
                reconnect_tx: &self.reconnect_tx,
                corrupt_every: None,
                outbound_socket_hook: None,
                stats: &self.stats,
                teardown_ids: Vec::new(),
                heard_ifac: &self.heard_ifac,
                operator_ifac_present,
                spawned_ids: &mut self.spawned_ids,
                refused_warned: &mut self.refused_warned,
            };
            spawner.spawn_tcp_client(
                &format!("autoconnect/{}", rec.name),
                rec.reachable_on.as_deref().unwrap(),
                rec.port.unwrap() as u16,
                rec,
            )
        }
    }

    fn discovered_tcp_record(
        ifac_netname: Option<&str>,
        ifac_netkey: Option<&str>,
        seed: u8,
    ) -> leviculum_core::discovery::DiscoveredInterfaceRecord {
        use leviculum_core::discovery::{DiscoveredInterface, DiscoveredInterfaceRecord};
        let di = DiscoveredInterface {
            interface_type: "TCPServerInterface".to_string(),
            transport: true,
            name: format!("peer-{seed}"),
            transport_id: [seed; 16],
            network_id: [seed; 16],
            value: 20,
            stamp: [seed; 32],
            latitude: None,
            longitude: None,
            height: None,
            reachable_on: Some("127.0.0.1".to_string()),
            port: Some(4965),
            frequency: None,
            bandwidth: None,
            spreadingfactor: None,
            codingrate: None,
            ifac_netname: ifac_netname.map(str::to_string),
            ifac_netkey: ifac_netkey.map(str::to_string),
            discovery_hash: [seed; 32],
        };
        DiscoveredInterfaceRecord::from_discovered(&di, 1, 1000.0, 1000.0, 1000.0, 0)
    }

    /// #151 case 1: a record advertising netname/netkey -> the spawned client
    /// handle carries the derived IFAC (red before the fix: no IFAC at all on
    /// the auto-connect spawn path).
    #[tokio::test]
    async fn autoconnect_spawn_carries_record_ifac() {
        let mut fx = SpawnerFixture::new();
        let rec = discovered_tcp_record(Some("closednet"), Some("closedkey"), 1);

        let id = fx.spawn(&rec, false).expect("spawn succeeds");
        let handle = fx.new_iface_rx.try_recv().expect("handle registered");
        assert_eq!(handle.info.id, id);
        let expected = leviculum_core::ifac::IfacConfig::new(
            Some("closednet"),
            Some("closedkey"),
            leviculum_core::constants::IFAC_DEFAULT_SIZE_NETWORK,
        )
        .expect("valid");
        let got = handle
            .info
            .ifac
            .as_ref()
            .expect("spawned client must carry the advertised IFAC");
        assert_eq!(got.identity().hash(), expected.identity().hash());
    }

    /// #151 case 2: no IFAC in the record -> the client inherits the IFAC of
    /// the interface the announce was heard on.
    #[tokio::test]
    async fn autoconnect_spawn_inherits_hearing_interface_ifac() {
        let mut fx = SpawnerFixture::new();
        let rec = discovered_tcp_record(None, None, 2);
        let parent =
            leviculum_core::ifac::IfacConfig::new(Some("parentnet"), None, 16).expect("valid");
        fx.heard_ifac.insert(rec.discovery_hash, parent.clone());

        fx.spawn(&rec, true).expect("spawn succeeds");
        let handle = fx.new_iface_rx.try_recv().expect("handle registered");
        let got = handle
            .info
            .ifac
            .as_ref()
            .expect("spawned client must inherit the hearing interface's IFAC");
        assert_eq!(got.identity().hash(), parent.identity().hash());
    }

    /// #151 case 3: IFAC configured on this node, but the record offers none
    /// and the hearing interface had none -> NO interface is spawned (fail
    /// closed), and the refusal is observable (warn-once bookkeeping).
    #[tokio::test]
    async fn autoconnect_spawn_fails_closed_without_resolvable_ifac() {
        let mut fx = SpawnerFixture::new();
        let rec = discovered_tcp_record(None, None, 3);

        assert!(
            fx.spawn(&rec, true).is_none(),
            "IFAC-running node must not auto-connect an unresolvable endpoint"
        );
        assert!(
            fx.new_iface_rx.try_recv().is_err(),
            "no interface handle may be registered on refusal"
        );
        assert!(
            fx.refused_warned.contains(&rec.discovery_hash),
            "the refusal must be recorded (named-reason warn emitted once)"
        );
        assert!(fx.spawned_ids.is_empty());
    }

    /// #151 case 4: open network (no IFAC anywhere) -> unchanged behaviour,
    /// the client spawns without IFAC. Guards against fixing fail-closed by
    /// breaking discovery for open meshes.
    #[tokio::test]
    async fn autoconnect_spawn_stays_open_on_open_node() {
        let mut fx = SpawnerFixture::new();
        let rec = discovered_tcp_record(None, None, 4);

        fx.spawn(&rec, false)
            .expect("open node keeps auto-connecting");
        let handle = fx.new_iface_rx.try_recv().expect("handle registered");
        assert!(
            handle.info.ifac.is_none(),
            "open-network auto-connect must stay unauthenticated"
        );
    }

    /// Codeberg #67 Stage 2a: build_announce_rate_config mirrors Python's
    /// validation (Reticulum.py:798-821): target kept only when > 0, a set
    /// target defaults an unset penalty/grace to 0, and no keys → None.
    #[test]
    fn build_announce_rate_config_semantics() {
        let mut cfg = InterfaceConfig {
            interface_type: "TCPClientInterface".to_string(),
            ..Default::default()
        };

        // No keys set → None (resolves like an all-None config).
        assert!(build_announce_rate_config(&cfg).is_none());

        // Full config passes through verbatim.
        cfg.announce_rate_target = Some(7200);
        cfg.announce_rate_penalty = Some(30);
        cfg.announce_rate_grace = Some(2);
        let ar = build_announce_rate_config(&cfg).expect("some");
        assert_eq!(ar.target, Some(7200));
        assert_eq!(ar.penalty, Some(30));
        assert_eq!(ar.grace, Some(2));

        // target == 0 is invalid (Python `> 0`) → dropped to None.
        cfg.announce_rate_target = Some(0);
        cfg.announce_rate_penalty = None;
        cfg.announce_rate_grace = None;
        let ar = build_announce_rate_config(&cfg)
            .expect("some (penalty/grace absent but present-check)");
        assert_eq!(ar.target, None);
        // No target → no coupling, penalty/grace stay None.
        assert_eq!(ar.penalty, None);
        assert_eq!(ar.grace, None);

        // Valid target but unset penalty/grace → coupling defaults them to 0.
        cfg.announce_rate_target = Some(1800);
        cfg.announce_rate_penalty = None;
        cfg.announce_rate_grace = None;
        let ar = build_announce_rate_config(&cfg).expect("some");
        assert_eq!(ar.target, Some(1800));
        assert_eq!(ar.penalty, Some(0));
        assert_eq!(ar.grace, Some(0));
    }

    /// Codeberg #92: the `announce_cap` key was parsed into `InterfaceConfig`
    /// but never applied, so every interface ran at the registration default of
    /// 2% no matter what the config said. It is applied now; this pins the
    /// float-to-per-cent bridge, including the sub-1% case Python can express
    /// and the core cannot.
    #[test]
    fn announce_cap_percent_from_config_semantics() {
        let mut cfg = InterfaceConfig {
            interface_type: "RNodeInterface".to_string(),
            ..Default::default()
        };

        // Key absent → nothing to apply, the registration default stands.
        assert_eq!(announce_cap_percent_from_config(&cfg), None);

        // Whole per cent passes through.
        cfg.announce_cap = Some(5.0);
        assert_eq!(announce_cap_percent_from_config(&cfg), Some(5));

        // Fractional rounds to nearest.
        cfg.announce_cap = Some(2.5);
        assert_eq!(announce_cap_percent_from_config(&cfg), Some(3));
        cfg.announce_cap = Some(2.4);
        assert_eq!(announce_cap_percent_from_config(&cfg), Some(2));

        // A share below half a per cent still means "cap announces", not
        // "silence them": it resolves to the smallest expressible share.
        cfg.announce_cap = Some(0.4);
        assert_eq!(announce_cap_percent_from_config(&cfg), Some(1));

        // The ini layer keeps 100 (Python's `<= 100`); it stays in range.
        cfg.announce_cap = Some(100.0);
        assert_eq!(announce_cap_percent_from_config(&cfg), Some(100));
    }

    /// A fresh core to apply interface config against.
    fn test_core(tag: &str) -> StdNodeCore {
        use leviculum_core::node::NodeCoreBuilder;
        let tmp = std::env::temp_dir().join(format!("drv-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            crate::clock::SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        )
    }

    // --- Codeberg #376: the peer-up announce, on lnsd ---

    const PHONE: [u8; 16] = [0xb9; 16];
    const NEIGHBOUR: [u8; 16] = [0x5d; 16];
    const T0: u64 = 1_700_000_000_000;

    /// A daemon-owned destination that has already announced once, which
    /// is the state the re-announce paths require: announcing the first
    /// time is the application's decision, not the peer-up path's.
    fn announced_destination(core: &mut StdNodeCore) -> DestinationHash {
        use leviculum_core::Identity;
        use leviculum_core::{DestinationType, Direction};
        let dest = Destination::new(
            Some(Identity::generate(&mut rand_core::OsRng)),
            Direction::In,
            DestinationType::Single,
            "lxmf",
            &["delivery"],
        )
        .unwrap();
        let hash = *dest.hash();
        core.register_destination(dest);
        // The first announce is the application's decision, and the
        // re-announce paths only repeat destinations that have made it.
        // Its own actions go nowhere: this test has no interfaces.
        let _seed = core.announce_destination(&hash, Some(b"seed")).unwrap();
        hash
    }

    /// Every announce in an output, as (interface, delivery hint); a
    /// `Broadcast` has no interface, which is what makes "it went to every
    /// link" visible rather than silent.
    fn announced(out: &TickOutput) -> Vec<(Option<usize>, Option<[u8; 16]>)> {
        use leviculum_core::packet::{Packet, PacketType};
        out.actions
            .iter()
            .filter_map(|action| match action {
                leviculum_core::transport::Action::SendPacket { iface, data, peer } => {
                    Packet::unpack(data)
                        .ok()
                        .filter(|p| p.flags.packet_type == PacketType::Announce)
                        .map(|_| (Some(iface.0), *peer))
                }
                leviculum_core::transport::Action::Broadcast { data, .. } => Packet::unpack(data)
                    .ok()
                    .filter(|p| p.flags.packet_type == PacketType::Announce)
                    .map(|_| (None, None)),
            })
            .collect()
    }

    /// One announce per peer-up edge, on that peer's link alone. The
    /// daemon's mirror of the firmware test of the same name: an announce
    /// broadcast here would reach a neighbour on the same adapter, which
    /// forwards it, and the relayed copy racing the direct one is the
    /// two-hop reading #376 opened with.
    #[test]
    fn a_peer_up_announces_to_that_peer_alone() {
        let mut core = test_core("peerup-one");
        let mut gate = PeerAnnounceLimiter::new();
        announced_destination(&mut core);

        let out = peer_up_announce(&mut core, &mut gate, InterfaceId(0), PHONE, T0, true);

        assert_eq!(
            announced(&out),
            vec![(Some(0), Some(PHONE))],
            "one announce, on the new peer's link, addressed at it"
        );
    }

    /// The rate limit, in the shape the field produces it: the same
    /// identity relinking inside the window announces once. A phone
    /// rotates its BLE address on every reconnect, so the gate is keyed on
    /// the identity and a rotation cannot reset it.
    #[test]
    fn the_same_identity_relinking_inside_the_window_announces_once() {
        let mut core = test_core("peerup-limit");
        let mut gate = PeerAnnounceLimiter::new();
        announced_destination(&mut core);

        let first = peer_up_announce(&mut core, &mut gate, InterfaceId(0), PHONE, T0, true);
        assert_eq!(announced(&first).len(), 1);

        for minute in 1..15u64 {
            let again = peer_up_announce(
                &mut core,
                &mut gate,
                InterfaceId(0),
                PHONE,
                T0 + minute * 60_000,
                true,
            );
            assert!(
                announced(&again).is_empty(),
                "relink at +{minute} min is inside the 15 minute window"
            );
        }

        let after = peer_up_announce(
            &mut core,
            &mut gate,
            InterfaceId(0),
            PHONE,
            T0 + leviculum_announce_policy::PEER_UP_ANNOUNCE_MIN_INTERVAL_MS,
            true,
        );
        assert_eq!(announced(&after).len(), 1, "the window is over");
    }

    /// A second peer is a second budget: the phone and a neighbour daemon
    /// on the same adapter each get their own announce.
    #[test]
    fn the_limit_is_per_peer() {
        let mut core = test_core("peerup-perpeer");
        let mut gate = PeerAnnounceLimiter::new();
        announced_destination(&mut core);

        assert_eq!(
            announced(&peer_up_announce(
                &mut core,
                &mut gate,
                InterfaceId(0),
                PHONE,
                T0,
                true
            )),
            vec![(Some(0), Some(PHONE))]
        );
        assert_eq!(
            announced(&peer_up_announce(
                &mut core,
                &mut gate,
                InterfaceId(0),
                NEIGHBOUR,
                T0,
                true
            )),
            vec![(Some(0), Some(NEIGHBOUR))]
        );
    }

    /// No plausible wall clock, no announce. On a PC the clock is always
    /// there, so this is the gate as policy rather than as a condition
    /// lnsd reaches — but the same code runs on a board that boots
    /// without one, where an announce stamped from uptime would poison
    /// the receiver's path table for the life of the entry.
    #[test]
    fn no_announce_without_a_clock() {
        let mut core = test_core("peerup-noclock");
        let mut gate = PeerAnnounceLimiter::new();
        announced_destination(&mut core);

        let out = peer_up_announce(&mut core, &mut gate, InterfaceId(0), PHONE, T0, false);
        assert!(announced(&out).is_empty());

        // And nothing was spent: the announce follows the clock.
        let after = peer_up_announce(&mut core, &mut gate, InterfaceId(0), PHONE, T0 + 1, true);
        assert_eq!(announced(&after).len(), 1);
    }

    /// The other half of Codeberg #92: `announce_cap_percent_from_config`
    /// resolving the key is worth nothing if the resolved share never reaches
    /// the core. This pins the call, and with it the ordering the fix depends
    /// on — `register_interface_bitrate` recreates the cap entry at the 2%
    /// registration default, so a cap applied before the bitrate is discarded
    /// and the interface silently runs at the default the config overrode.
    #[test]
    fn configured_announce_cap_reaches_the_core() {
        let mut core = test_core("cap");
        let cfg = InterfaceConfig {
            interface_type: "RNodeInterface".to_string(),
            bitrate: Some(9_600),
            announce_cap: Some(5.0),
            ..Default::default()
        };

        apply_bitrate_and_announce_cap(&mut core, 0, &cfg);

        assert_eq!(
            core.interface_announce_cap(0),
            Some(5),
            "the configured announce_cap must be the share the throttler holds, \
             not the registration default"
        );
    }

    /// `announce_cap` without a `bitrate` has nothing to take a share of: the
    /// setter reports the miss and the interface keeps no cap entry. Pins the
    /// branch that logs the warning rather than pretending the key applied.
    #[test]
    fn announce_cap_without_a_bitrate_registers_no_cap() {
        let mut core = test_core("nocap");
        let cfg = InterfaceConfig {
            interface_type: "RNodeInterface".to_string(),
            bitrate: None,
            announce_cap: Some(5.0),
            ..Default::default()
        };

        apply_bitrate_and_announce_cap(&mut core, 0, &cfg);

        assert_eq!(
            core.interface_announce_cap(0),
            None,
            "with no bitrate there is no cap entry to hold a share"
        );
    }

    /// Default builder leaves the event channel enabled. The first
    /// `take_event_receiver()` returns the receiver; second call returns
    /// `None` (already taken).
    #[test]
    fn builder_default_events_enabled() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut node = ReticulumNodeBuilder::new()
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");

        assert!(
            node.control_tx.is_some() && node.data_tx.is_some(),
            "default build must keep both event planes on"
        );
        assert!(
            node.take_event_receiver().is_some(),
            "default build must hand out a receiver"
        );
        assert!(
            node.take_event_receiver().is_none(),
            "second take must return None"
        );
    }

    /// `without_events()` skips construction of the event channel; the
    /// node has neither sender nor receiver, so daemon-mode build never
    /// queues `NodeEvent`s.
    #[test]
    fn builder_without_events_disables_event_channel() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut node = ReticulumNodeBuilder::new()
            .storage_path(td.path().to_path_buf())
            .without_events()
            .build_sync()
            .expect("build_sync failed");

        assert!(
            node.control_tx.is_none() && node.data_tx.is_none(),
            "daemon-mode build must not have event senders"
        );
        assert!(
            node.take_event_receiver().is_none(),
            "daemon-mode build must not hand out a receiver"
        );
    }

    /// `dispatch_output` with `event_tx = None` accepts a TickOutput
    /// containing events and consumes them silently (no panic, no try_send,
    /// no warn). Mirrors the NRF daemon path where `output.events` simply
    /// falls out of scope.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_output_skips_event_forwarding_when_disabled() {
        use leviculum_core::node::{NodeCoreBuilder, NodeEvent};
        use leviculum_core::transport::TickOutput;
        use leviculum_core::DestinationHash;

        let tmp =
            std::env::temp_dir().join(format!("without-events-dispatch-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let core: Arc<Mutex<StdNodeCore>> = {
            let node = NodeCoreBuilder::new().enable_transport(true).build(
                rand_core::OsRng,
                SystemClock::new(),
                crate::storage::Storage::new(&tmp).unwrap(),
            );
            Arc::new(Mutex::new(node))
        };

        let mut registry = InterfaceRegistry::new();
        let mut retry_queues: BTreeMap<usize, VecDeque<Vec<u8>>> = BTreeMap::new();
        let mut retry_queue_warned: std::collections::BTreeSet<usize> =
            std::collections::BTreeSet::new();
        let mut retry_queue_max_depth: BTreeMap<usize, usize> = BTreeMap::new();
        let ifac_configs: BTreeMap<usize, leviculum_core::ifac::IfacConfig> = BTreeMap::new();

        let mut output = TickOutput::empty();
        output.events.push(NodeEvent::PathLost {
            destination_hash: DestinationHash::new([0xAA; 16]),
        });
        output.events.push(NodeEvent::InterfaceDown(7));

        // event_tx = None, the function must accept this and simply drop
        // the events. No panic, no channel send.
        dispatch_output(
            output,
            &mut registry,
            None,
            &core,
            &mut retry_queues,
            &mut retry_queue_warned,
            &mut retry_queue_max_depth,
            &ifac_configs,
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            None,
        );
    }

    /// #25 — a dispatch that fails `Disconnected` must EMIT the loss, not just
    /// log it. Before this, `dispatch_output` returned `()` and dropped
    /// `result.errors` on the floor, so a consumer's retry/backoff path was
    /// never told the dispatch failed and could never engage — the field trace
    /// lost 8 frames invisibly. Guards the emission, which is the whole
    /// recoverability contract.
    #[tokio::test]
    async fn dispatch_disconnected_emits_frames_dropped() {
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::transport::{Action, InterfaceId};

        let tmp = std::env::temp_dir().join(format!("frames-dropped-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let core: Arc<Mutex<StdNodeCore>> = {
            let node = NodeCoreBuilder::new().enable_transport(true).build(
                rand_core::OsRng,
                SystemClock::new(),
                crate::storage::Storage::new(&tmp).unwrap(),
            );
            Arc::new(Mutex::new(node))
        };

        let mut registry = InterfaceRegistry::new();
        let (_inc_tx, inc_rx) = mpsc::channel(4);
        let (out_tx, out_rx) = mpsc::channel(4);
        // DROP the outgoing receiver: the interface is now dead, so any send to
        // it fails `InterfaceError::Disconnected` — exactly the field case (the
        // socket died with frames in flight).
        drop(out_rx);
        registry.register(InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(12),
                name: "tcp_server/dead".into(),
                hw_mtu: None,
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: inc_rx,
            outgoing: out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: None,
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        });

        let mut retry_queues: BTreeMap<usize, VecDeque<Vec<u8>>> = BTreeMap::new();
        let mut retry_queue_warned: std::collections::BTreeSet<usize> =
            std::collections::BTreeSet::new();
        let mut retry_queue_max_depth: BTreeMap<usize, usize> = BTreeMap::new();
        let ifac_configs: BTreeMap<usize, leviculum_core::ifac::IfacConfig> = BTreeMap::new();

        // Two frames in flight to the dead interface.
        let mut output = TickOutput::empty();
        output.actions.push(Action::SendPacket {
            peer: None,
            iface: InterfaceId(12),
            data: vec![0xAA; 32],
        });
        output.actions.push(Action::SendPacket {
            peer: None,
            iface: InterfaceId(12),
            data: vec![0xBB; 32],
        });

        let (mut sink, mut rx) = sink_and_receiver(8, 8);
        dispatch_output(
            output,
            &mut registry,
            Some(&mut sink),
            &core,
            &mut retry_queues,
            &mut retry_queue_warned,
            &mut retry_queue_max_depth,
            &ifac_configs,
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            None,
        );

        let ev = rx
            .recv()
            .await
            .expect("a FramesDropped event must be emitted (#25)");
        match ev {
            NodeEvent::FramesDropped {
                iface_id,
                count,
                reason,
            } => {
                assert_eq!(iface_id, 12);
                assert_eq!(count, 2, "both in-flight frames must be reported destroyed");
                assert_eq!(reason, FrameDropReason::DispatchDisconnected);
            }
            other => panic!("expected FramesDropped, got {other:?}"),
        }
    }

    /// Build a connected control/data sink + merged receiver for the
    /// split-channel tests (Codeberg #71).
    fn sink_and_receiver(control_cap: usize, data_cap: usize) -> (EventSink, EventReceiver) {
        let (control_tx, control_rx) = mpsc::channel(control_cap);
        let (data_tx, data_rx) = mpsc::channel(data_cap);
        (
            EventSink {
                control_tx,
                data_tx,
                control_capacity: control_cap,
                control_dropped: 0,
            },
            EventReceiver {
                control: control_rx,
                data: data_rx,
            },
        )
    }

    fn path_found(i: usize) -> NodeEvent {
        NodeEvent::PathFound {
            destination_hash: leviculum_core::DestinationHash::new([0xAB; 16]),
            hops: (i % 256) as u8,
            interface_index: i,
        }
    }

    /// Adapted from emoore's PR #71 repro
    /// (`control_plane_burst_lossless_to_draining_consumer`). Their unbounded
    /// channel accepted a burst of `EVENT_CHANNEL_CAPACITY * 4`; our bounded
    /// control plane is lossless *up to its configured capacity*. So we burst
    /// exactly capacity control events into an empty channel and require all
    /// of them, in order, at a draining consumer — the property the old single
    /// bounded `try_send` channel violated by silently dropping once full.
    #[tokio::test]
    async fn control_plane_burst_lossless_to_draining_consumer() {
        let cap = crate::config::DEFAULT_CONTROL_CHANNEL_CAPACITY;
        // Tiny data plane to prove the control plane is independent of it.
        let (mut sink, mut rx) = sink_and_receiver(cap, 16);

        for i in 0..cap {
            sink.emit(path_found(i));
        }

        for i in 0..cap {
            match rx.recv().await {
                Some(NodeEvent::PathFound {
                    hops,
                    interface_index,
                    ..
                }) => {
                    assert_eq!(
                        interface_index, i,
                        "control events must arrive in order with none dropped"
                    );
                    assert_eq!(hops, (i % 256) as u8, "event payload must be intact");
                }
                other => panic!("expected PathFound #{i}, got {other:?}"),
            }
        }
    }

    /// The property emoore's unbounded channel broke: the DATA plane must stay
    /// bounded and drop under load rather than grow without limit. Emitting
    /// far more data events than the data capacity, with no concurrent drain,
    /// must leave at most `data_cap` buffered.
    #[tokio::test]
    async fn data_plane_stays_bounded_and_drops_under_load() {
        let data_cap = 8;
        let (mut sink, mut rx) = sink_and_receiver(16, data_cap);

        let burst = data_cap * 8;
        for i in 0..burst {
            sink.emit(NodeEvent::PacketReceived {
                destination: leviculum_core::DestinationHash::new([0x11; 16]),
                data: vec![i as u8],
                interface_index: i,
            });
        }

        let mut count = 0;
        while let Ok(ev) = rx.try_recv() {
            assert!(
                matches!(ev, NodeEvent::PacketReceived { .. }),
                "only data events expected"
            );
            count += 1;
        }
        assert_eq!(
            count, data_cap,
            "data plane must be bounded at its capacity (backpressure preserved)"
        );
    }

    /// Overflowing the bounded control channel must be VISIBLE: the dropped
    /// events are counted and surfaced as a single
    /// `ControlPlaneOverflow {{ dropped_count }}` once the channel has room.
    /// The marker itself is never lost, and the counter resets after delivery.
    #[tokio::test]
    async fn control_overflow_delivers_visible_marker() {
        let cap = 4;
        let (mut sink, mut rx) = sink_and_receiver(cap, 4);

        // Fill the control channel to capacity (all delivered)...
        for i in 0..cap {
            sink.emit(path_found(i));
        }
        // ...then emit three more that cannot fit: dropped and counted.
        let dropped = 3usize;
        for i in 0..dropped {
            sink.emit(path_found(100 + i));
        }

        // Drain everything currently buffered so the channel has headroom for
        // both the next real event and the overflow marker behind it.
        for _ in 0..cap {
            assert!(matches!(rx.try_recv(), Ok(NodeEvent::PathFound { .. })));
        }

        // One more control event lands AND carries the overflow marker behind
        // it (emit_control flushes the marker once an event proves there's room).
        sink.emit(path_found(200));
        assert!(
            matches!(
                rx.try_recv(),
                Ok(NodeEvent::PathFound {
                    interface_index: 200,
                    ..
                })
            ),
            "the real event is delivered first"
        );
        match rx.try_recv() {
            Ok(NodeEvent::ControlPlaneOverflow { dropped_count }) => {
                assert_eq!(
                    dropped_count, dropped as u64,
                    "marker must report exactly the number of dropped control events"
                );
            }
            other => panic!("expected ControlPlaneOverflow {{{dropped}}}, got {other:?}"),
        }

        // Counter reset: no spurious second marker.
        sink.emit(path_found(201));
        assert!(matches!(
            rx.try_recv(),
            Ok(NodeEvent::PathFound {
                interface_index: 201,
                ..
            })
        ));
        assert!(
            rx.try_recv().is_err(),
            "no second overflow marker after the count was reset"
        );
    }

    /// Strict priority: a backlog of data events must never delay a control
    /// event. With both planes non-empty, `recv` returns control first.
    #[tokio::test]
    async fn control_plane_drained_before_data_plane() {
        let (mut sink, mut rx) = sink_and_receiver(8, 8);
        // Queue data first, then a single control event.
        for i in 0..4 {
            sink.emit(NodeEvent::PacketReceived {
                destination: leviculum_core::DestinationHash::new([0x22; 16]),
                data: vec![i as u8],
                interface_index: i,
            });
        }
        sink.emit(path_found(7));

        // Despite arriving last, the control event is delivered first.
        match rx.recv().await {
            Some(NodeEvent::PathFound {
                interface_index: 7, ..
            }) => {}
            other => panic!("control event must come first, got {other:?}"),
        }
        // Then the data backlog follows.
        for i in 0..4 {
            match rx.recv().await {
                Some(NodeEvent::PacketReceived {
                    interface_index, ..
                }) => assert_eq!(interface_index, i),
                other => panic!("expected data #{i}, got {other:?}"),
            }
        }
    }

    /// Regression: the node's timer-driven event loop (`sleep_until`) and
    /// interface timers must work even when the *embedding* runtime was built
    /// without `enable_time()` — the PyO3/edge case that previously panicked
    /// the event-loop task on its first poll. The node owns its own
    /// time-enabled, single-worker runtime, so `start()` is independent of how
    /// the host configured its runtime.
    #[test]
    fn event_loop_survives_host_runtime_without_time_driver() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut node = ReticulumNodeBuilder::new()
            .enable_transport(true)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync");

        // Host runtime deliberately WITHOUT enable_time() (IO only) — mirrors an
        // embedder that built its runtime without timers.
        let host = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("host runtime");
        host.block_on(async {
            node.start().await.expect("start");
            // Let the event loop tick on the node's own runtime. OS sleep — the
            // host runtime has no timer to drive a tokio sleep.
            std::thread::sleep(std::time::Duration::from_millis(250));
            // Pre-fix the event loop panicked on its first `sleep_until` poll,
            // so its JoinHandle resolved to a JoinError and stop() returned Err.
            node.stop()
                .await
                .expect("stop — event loop must not have panicked");
        });
    }

    /// Regression for the runtime-cleanup-on-error path: when interface init
    /// fails *after* the node runtime is built, start() must return the error —
    /// not panic by blocking-dropping the Runtime inside the host's async
    /// context.
    #[test]
    fn start_surfaces_interface_init_error_without_panicking() {
        // Occupy a port so the node's TCP server bind fails during init.
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let busy: std::net::SocketAddr = occupied.local_addr().expect("local_addr");

        let td = tempfile::tempdir().expect("tempdir");
        let mut node = ReticulumNodeBuilder::new()
            .enable_transport(true)
            .add_tcp_server(busy)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync");

        let host = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("host runtime");
        host.block_on(async {
            // Pre-fix this panicked (blocking Runtime drop in async context);
            // post-fix it returns the bind error cleanly.
            let result = node.start().await;
            assert!(
                result.is_err(),
                "start() should surface the TCP bind failure, got {result:?}"
            );
        });
    }

    /// Regression for the `Drop` teardown path: dropping a started node from
    /// *inside* another runtime's async context must not panic. emoore's
    /// other tests exercise the `stop()` teardown; this one drops the node
    /// without calling `stop()`, so the node's owned `Runtime` is torn down by
    /// the `Drop` impl. A blocking `Runtime` drop inside an async context
    /// panics; the `Drop` impl uses `shutdown_background()` to avoid it.
    #[test]
    fn node_drops_cleanly_within_host_runtime_async_context() {
        let host = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("host runtime");
        host.block_on(async {
            let td = tempfile::tempdir().expect("tempdir");
            let mut node = ReticulumNodeBuilder::new()
                .enable_transport(true)
                .storage_path(td.path().to_path_buf())
                .build_sync()
                .expect("build_sync");
            node.start().await.expect("start");
            // Drop the live node (and its owned runtime) here, inside the host
            // runtime's async context. Pre-fix the blocking Runtime drop panicked;
            // post-fix `Drop`'s shutdown_background() returns without blocking.
            drop(node);
        });
    }

    /// Runtime attach/detach through the shared builder: `spawn_interface`
    /// brings up a UDP interface on a running node and `remove_interface` tears
    /// it back down, both reflected in `interface_stats`. Guards the universal
    /// build path and the remove-by-id teardown.
    #[test]
    fn spawn_and_remove_interface_round_trip() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut node = ReticulumNodeBuilder::new()
            .enable_transport(true)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync");

        let host = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("host runtime");
        host.block_on(async {
            node.start().await.expect("start");

            let cfg = crate::config::InterfaceConfig {
                interface_type: "UDPInterface".to_string(),
                enabled: true,
                listen_ip: Some("127.0.0.1".to_string()),
                listen_port: Some(0),
                forward_ip: Some("127.0.0.1".to_string()),
                forward_port: Some(37000),
                ..Default::default()
            };
            let ids = node.spawn_interface(cfg).expect("spawn_interface");
            assert_eq!(ids.len(), 1, "UDP is a single-handle interface");
            let name = format!("udp_{}", ids[0].0);

            // The node's own runtime drives registration; block the host thread.
            std::thread::sleep(std::time::Duration::from_millis(150));
            assert!(
                node.interface_stats().iter().any(|s| s.name == name),
                "attached interface must appear in interface_stats"
            );

            node.remove_interface(ids[0]).expect("remove_interface");
            std::thread::sleep(std::time::Duration::from_millis(150));
            assert!(
                node.interface_stats().iter().all(|s| s.name != name),
                "removed interface must be gone from interface_stats"
            );

            node.stop().await.expect("stop");
        });
    }

    #[test]
    fn test_reticulum_node_builder_creates_node() {
        let td = tempfile::tempdir().expect("tempdir");
        let node = ReticulumNodeBuilder::new()
            .enable_transport(true)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync failed");

        assert!(node.is_transport_enabled());
        assert!(!node.is_running());
        assert_eq!(node.path_count(), 0);

        let fake_hash = leviculum_core::DestinationHash::new([0xFF; 16]);
        assert!(!node.has_path(&fake_hash));
        assert!(node.hops_to(&fake_hash).is_none());
    }

    /// push_retry_with_warn inserts an entry into the `warned` set
    /// the first time queue depth reaches RETRY_QUEUE_DEPTH_WARN.
    /// Subsequent pushes beyond the threshold do NOT re-insert.
    #[test]
    fn push_retry_warns_once_when_crossing_warn_depth() {
        let mut q: VecDeque<Vec<u8>> = VecDeque::new();
        let mut warned: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut max_depth: BTreeMap<usize, usize> = BTreeMap::new();
        // Fill up to one below the warn threshold → never warns.
        for _ in 0..(RETRY_QUEUE_DEPTH_WARN - 1) {
            push_retry_with_warn(&mut q, 1, vec![0u8; 8], &mut warned, &mut max_depth);
        }
        assert!(
            !warned.contains(&1),
            "below-threshold depth must not trigger warn"
        );
        // Push one more → crosses threshold.
        push_retry_with_warn(&mut q, 1, vec![0u8; 8], &mut warned, &mut max_depth);
        assert!(
            warned.contains(&1),
            "reaching RETRY_QUEUE_DEPTH_WARN must trigger warn"
        );
        // Push past threshold → already warned, set membership unchanged (idempotent).
        push_retry_with_warn(&mut q, 1, vec![0u8; 8], &mut warned, &mut max_depth);
        assert!(warned.contains(&1));
        assert_eq!(warned.len(), 1, "no duplicate entries");
    }

    /// Clearing the warned flag (as dispatch_output does after the
    /// retain loop) allows a future re-crossing of the warn depth
    /// to re-emit.
    #[test]
    fn push_retry_rewarns_after_queue_drains_below_warn_depth() {
        let mut q: VecDeque<Vec<u8>> = VecDeque::new();
        let mut warned: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut max_depth: BTreeMap<usize, usize> = BTreeMap::new();
        for _ in 0..RETRY_QUEUE_DEPTH_WARN {
            push_retry_with_warn(&mut q, 2, vec![0u8; 8], &mut warned, &mut max_depth);
        }
        assert!(warned.contains(&2));
        // Drain below the warn threshold (simulate: clear queue,
        // clear warned per the retain-clause in dispatch_output).
        q.clear();
        warned.retain(|idx| {
            let _ = idx;
            // Mirror dispatch_output's clause:
            // keep only if queue.len() >= RETRY_QUEUE_DEPTH_WARN
            false // queue is empty now
        });
        assert!(!warned.contains(&2));
        // Rebuild to threshold → warn re-emitted.
        for _ in 0..RETRY_QUEUE_DEPTH_WARN {
            push_retry_with_warn(&mut q, 2, vec![0u8; 8], &mut warned, &mut max_depth);
        }
        assert!(warned.contains(&2));
    }

    /// max_depth is monotonic and tracks the high-watermark per
    /// interface index.
    #[test]
    fn push_retry_tracks_monotonic_max_depth() {
        let mut q: VecDeque<Vec<u8>> = VecDeque::new();
        let mut warned: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut max_depth: BTreeMap<usize, usize> = BTreeMap::new();
        for _ in 0..5 {
            push_retry_with_warn(&mut q, 3, vec![0u8; 4], &mut warned, &mut max_depth);
        }
        assert_eq!(max_depth.get(&3), Some(&5));
        // Drain the queue manually; max_depth must NOT regress.
        q.clear();
        // A single push after drain puts len=1, watermark stays at 5.
        push_retry_with_warn(&mut q, 3, vec![0u8; 4], &mut warned, &mut max_depth);
        assert_eq!(max_depth.get(&3), Some(&5), "watermark must be monotonic");
        // Re-fill past the old watermark → grows.
        for _ in 0..10 {
            push_retry_with_warn(&mut q, 3, vec![0u8; 4], &mut warned, &mut max_depth);
        }
        assert_eq!(max_depth.get(&3), Some(&11));
    }

    /// compute_retry_wake_deadline_ms returns `None` when every retry
    /// queue is empty, no wake needed.
    #[tokio::test(flavor = "current_thread")]
    async fn compute_retry_wake_none_when_queues_empty() {
        let registry = InterfaceRegistry::new();
        let retry_queues: BTreeMap<usize, VecDeque<Vec<u8>>> = BTreeMap::new();
        assert_eq!(
            compute_retry_wake_deadline_ms(&retry_queues, &registry, 1_000),
            None
        );
    }

    /// Queues with a ready head → return None so the caller doesn't
    /// sleep (drain would already happen on the next normal tick).
    #[tokio::test(flavor = "current_thread")]
    async fn compute_retry_wake_none_when_any_head_ready() {
        use crate::interfaces::airtime::AirtimeCredit;
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::transport::InterfaceId;

        let mut registry = InterfaceRegistry::new();
        let (_inc_tx, inc_rx) = tokio::sync::mpsc::channel(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(4);
        registry.register(InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(0),
                name: "ready".into(),
                hw_mtu: None,
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: inc_rx,
            outgoing: out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: None, // always-ready
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        });
        let mut retry_queues = BTreeMap::new();
        retry_queues
            .entry(0usize)
            .or_insert_with(VecDeque::new)
            .push_back(vec![1, 2, 3]);
        assert_eq!(
            compute_retry_wake_deadline_ms(&retry_queues, &registry, 1_000),
            None,
            "ready interface should short-circuit to None"
        );
        // Silence unused-import warning on non-LoRa path.
        let _ = AirtimeCredit::new(125_000, 10, 8, 18, 500);
    }

    /// When a queue head is NOT ready, return the MINIMUM over all
    /// not-ready heads' slot times.
    #[tokio::test(flavor = "current_thread")]
    async fn compute_retry_wake_returns_min_future_slot() {
        use crate::interfaces::airtime::AirtimeCredit;
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::transport::InterfaceId;

        let mut registry = InterfaceRegistry::new();
        let now_ms = 1_000;

        // Two LoRa handles with different saturation, both have
        // not-ready heads; the earlier slot should win.
        for (idx, payload_charge) in [(0usize, 500u32), (1usize, 100u32)] {
            let mut credit = AirtimeCredit::new(125_000, 10, 8, 18, 500);
            credit.try_charge(payload_charge, now_ms).unwrap();
            let (_inc_tx, inc_rx) = tokio::sync::mpsc::channel(4);
            let (out_tx, _out_rx) = tokio::sync::mpsc::channel(4);
            registry.register(InterfaceHandle {
                info: InterfaceInfo {
                    id: InterfaceId(idx),
                    name: format!("lora-{idx}"),
                    hw_mtu: None,
                    is_local_client: false,
                    bitrate: None,
                    tx_jitter_max_ms: None,
                    ifac: None,
                    mode: leviculum_core::traits::InterfaceMode::default(),
                    kind: leviculum_core::traits::InterfaceKind::Unknown,
                    ingress_control: None,
                },
                incoming: inc_rx,
                outgoing: out_tx,
                counters: Arc::new(InterfaceCounters::new()),
                credit: Some(Arc::new(Mutex::new(credit))),
                ready: crate::interfaces::ReadySignal::ready_immediate(),
            });
        }
        // Both queues carry a full-MTU packet, both heads are
        // definitely not-ready because the buckets were charged at
        // different magnitudes.
        let mut retry_queues = BTreeMap::new();
        retry_queues
            .entry(0usize)
            .or_insert_with(VecDeque::new)
            .push_back(vec![0u8; 500]);
        retry_queues
            .entry(1usize)
            .or_insert_with(VecDeque::new)
            .push_back(vec![0u8; 500]);

        let iface0_slot = {
            let handles = registry.handles();
            use leviculum_core::traits::Interface;
            handles[0].next_slot_ms(500, now_ms)
        };
        let iface1_slot = {
            let handles = registry.handles();
            use leviculum_core::traits::Interface;
            handles[1].next_slot_ms(500, now_ms)
        };
        let expected_min = iface0_slot.min(iface1_slot);
        assert!(expected_min > now_ms);

        assert_eq!(
            compute_retry_wake_deadline_ms(&retry_queues, &registry, now_ms),
            Some(expected_min)
        );
    }

    /// drain_retry_queues honors next_slot_ms. A ready interface
    /// drains its packet; a saturated interface leaves the packet at
    /// the queue front.
    #[tokio::test(flavor = "current_thread")]
    async fn drain_retry_queues_skips_saturated_and_drains_ready() {
        use crate::interfaces::airtime::AirtimeCredit;
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::transport::InterfaceId;

        let mut registry = InterfaceRegistry::new();

        // LoRa handle (iface_idx=1), saturated bucket.
        let mut saturated = AirtimeCredit::new(125_000, 10, 8, 18, 500);
        saturated.try_charge(500, 0).unwrap();
        let (_li, l_inc_rx) = tokio::sync::mpsc::channel(4);
        let (l_out_tx, mut l_out_rx) = tokio::sync::mpsc::channel(4);
        registry.register(InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(1),
                name: "lora".into(),
                hw_mtu: Some(500),
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: l_inc_rx,
            outgoing: l_out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: Some(Arc::new(Mutex::new(saturated))),
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        });

        // Plain handle (iface_idx=2), credit = None (always ready).
        let (_pi, p_inc_rx) = tokio::sync::mpsc::channel(4);
        let (p_out_tx, mut p_out_rx) = tokio::sync::mpsc::channel(4);
        registry.register(InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(2),
                name: "plain".into(),
                hw_mtu: None,
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: p_inc_rx,
            outgoing: p_out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: None,
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        });

        // Queue one packet on each interface.
        let mut retry_queues: BTreeMap<usize, VecDeque<Vec<u8>>> = BTreeMap::new();
        retry_queues
            .entry(1)
            .or_default()
            .push_back(vec![0xAA; 100]);
        retry_queues
            .entry(2)
            .or_default()
            .push_back(vec![0xBB; 100]);

        drain_retry_queues(&mut retry_queues, &mut registry, 0);

        // Saturated LoRa: packet still at front.
        assert_eq!(retry_queues.get(&1).map(|q| q.len()), Some(1));
        // Plain: packet drained.
        assert_eq!(retry_queues.get(&2).map(|q| q.len()), Some(0));
        // And the plain interface's outgoing channel received the packet.
        assert!(p_out_rx.try_recv().is_ok());
        // Saturated: nothing went to outgoing.
        assert!(l_out_rx.try_recv().is_err());
    }

    /// A ready interface (no credit) drains repeatedly across retries.
    #[tokio::test(flavor = "current_thread")]
    async fn drain_retry_queues_drains_all_ready_packets() {
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::transport::InterfaceId;

        let mut registry = InterfaceRegistry::new();
        let (_pi, p_inc_rx) = tokio::sync::mpsc::channel(4);
        let (p_out_tx, mut p_out_rx) = tokio::sync::mpsc::channel(4);
        registry.register(InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(0),
                name: "tcp".into(),
                hw_mtu: None,
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: p_inc_rx,
            outgoing: p_out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: None,
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        });
        let mut retry_queues: BTreeMap<usize, VecDeque<Vec<u8>>> = BTreeMap::new();
        let queue = retry_queues.entry(0).or_default();
        queue.push_back(vec![1, 2, 3]);
        queue.push_back(vec![4, 5, 6]);
        queue.push_back(vec![7, 8, 9]);

        drain_retry_queues(&mut retry_queues, &mut registry, 0);

        assert_eq!(retry_queues.get(&0).map(|q| q.len()), Some(0));
        let mut received = 0;
        while p_out_rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 3);
    }

    /// push_interface_state copies per-interface next_slot_ms into
    /// Transport's backchannel. Build a synthetic registry with one
    /// LoRa (saturated bucket → future slot) and one non-LoRa (default
    /// → now_ms), run the push, assert Transport reflects both.
    #[tokio::test(flavor = "current_thread")]
    async fn push_interface_state_mirrors_per_handle_values() {
        use crate::interfaces::airtime::AirtimeCredit;
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::transport::InterfaceId;
        use std::sync::atomic::Ordering;
        let _ = Ordering::Relaxed; // silences unused-import on minor builds

        // Minimal StdNodeCore in Arc<Mutex>.
        let tmp = std::env::temp_dir().join(format!("bug3-phase2a-c3-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let core: Arc<Mutex<StdNodeCore>> = {
            let node = leviculum_core::node::NodeCoreBuilder::new()
                .enable_transport(true)
                .build(
                    rand_core::OsRng,
                    SystemClock::new(),
                    crate::storage::Storage::new(&tmp).unwrap(),
                );
            Arc::new(Mutex::new(node))
        };

        // Construct two synthetic handles directly. Channel receivers
        // are dropped at end of test, that's fine since we don't call
        // try_send here, only next_slot_ms (which is &self).
        let mut registry = InterfaceRegistry::new();

        let (_lora_inc_tx, lora_inc_rx) = tokio::sync::mpsc::channel(4);
        let (lora_out_tx, _lora_out_rx) = tokio::sync::mpsc::channel(4);
        let mut lora_credit = AirtimeCredit::new(125_000, 10, 8, 18, 500);
        // Exhaust to guarantee earliest_fit_time > 0.
        lora_credit.try_charge(500, 0).unwrap();
        let lora_handle = InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(1),
                name: "lora-test".into(),
                hw_mtu: Some(500),
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: lora_inc_rx,
            outgoing: lora_out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: Some(Arc::new(Mutex::new(lora_credit))),
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        };

        let (_plain_inc_tx, plain_inc_rx) = tokio::sync::mpsc::channel(4);
        let (plain_out_tx, _plain_out_rx) = tokio::sync::mpsc::channel(4);
        let plain_handle = InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(2),
                name: "plain-test".into(),
                hw_mtu: None,
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: plain_inc_rx,
            outgoing: plain_out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: None,
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        };
        registry.register(lora_handle);
        registry.register(plain_handle);

        // Run the push.
        push_interface_state(&mut registry, &core);

        // LoRa (idx=1, saturated): slot must be in the future relative to now_ms.
        let now_ms = core.lock().unwrap().now_ms();
        let lora_slot = core.lock().unwrap().next_slot_ms_for_interface(1, now_ms);
        assert!(
            lora_slot > now_ms,
            "saturated LoRa should map to future slot, got {lora_slot} vs now {now_ms}"
        );
        // Plain (idx=2, no credit): not deferred — its slot is "now or already
        // past", never a future slot like the saturated LoRa above. We assert
        // `<= now_ms` rather than `== now_ms`: `push_interface_state` records the
        // slot against the `now_ms()` it read internally, which can be an
        // earlier millisecond than the one this line reads on a fast runner, so
        // exact equality raced. The invariant that matters is no future deferral.
        let plain_slot = core.lock().unwrap().next_slot_ms_for_interface(2, now_ms);
        assert!(
            plain_slot <= now_ms,
            "non-LoRa must not be deferred to a future slot, got {plain_slot} vs now {now_ms}"
        );
    }

    /// One LoRa-Serial handle at SF7 → Transport's
    /// announce_jitter_max_ms() reflects the SF7 airtime (which at
    /// 500 B is well below 167 ms, so the legacy 500 ms floor wins).
    /// Verifies the airtime push runs and the helper composes
    /// correctly. Use SF10 for a value the helper actually amplifies.
    #[tokio::test(flavor = "current_thread")]
    async fn push_interface_state_pushes_max_airtime_for_lora() {
        use crate::interfaces::airtime::AirtimeCredit;
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::transport::InterfaceId;

        let tmp = std::env::temp_dir().join(format!("bug19-a2-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let core: Arc<Mutex<StdNodeCore>> = {
            let node = leviculum_core::node::NodeCoreBuilder::new()
                .enable_transport(true)
                .build(
                    rand_core::OsRng,
                    SystemClock::new(),
                    crate::storage::Storage::new(&tmp).unwrap(),
                );
            Arc::new(Mutex::new(node))
        };

        let mut registry = InterfaceRegistry::new();
        let (_inc_tx, inc_rx) = tokio::sync::mpsc::channel(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(4);
        let credit = AirtimeCredit::new(125_000, 10, 8, 18, 500);
        let expected_airtime = credit.max_airtime_ms();
        let handle = InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(1),
                name: "lora-sf10".into(),
                hw_mtu: Some(500),
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: inc_rx,
            outgoing: out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: Some(Arc::new(Mutex::new(credit))),
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        };
        registry.register(handle);

        push_interface_state(&mut registry, &core);

        let jitter = core.lock().unwrap().announce_jitter_max_ms();
        assert_eq!(
            jitter,
            (3 * expected_airtime).max(500),
            "jitter window should track SF10 airtime"
        );
    }

    /// A non-LoRa registry leaves the airtime map empty; the helper
    /// returns the legacy floor.
    #[tokio::test(flavor = "current_thread")]
    async fn push_interface_state_skips_airtime_for_non_lora() {
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::transport::InterfaceId;

        let tmp = std::env::temp_dir().join(format!("bug19-a2-non-lora-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let core: Arc<Mutex<StdNodeCore>> = {
            let node = leviculum_core::node::NodeCoreBuilder::new()
                .enable_transport(true)
                .build(
                    rand_core::OsRng,
                    SystemClock::new(),
                    crate::storage::Storage::new(&tmp).unwrap(),
                );
            Arc::new(Mutex::new(node))
        };

        let mut registry = InterfaceRegistry::new();
        let (_inc_tx, inc_rx) = tokio::sync::mpsc::channel(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(4);
        let handle = InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(2),
                name: "tcp-test".into(),
                hw_mtu: None,
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: inc_rx,
            outgoing: out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: None,
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        };
        registry.register(handle);

        push_interface_state(&mut registry, &core);

        let jitter = core.lock().unwrap().announce_jitter_max_ms();
        assert_eq!(jitter, 500, "no LoRa interface ⇒ legacy floor");
    }

    /// Reconfiguring the bucket's radio params (SF7 → SF10) is picked
    /// up on the next push. Mirrors the live `send_radio_config` flow:
    /// the bucket's `update_radio_params` swaps params atomically; the
    /// next dispatch tick mirrors the new airtime into Transport.
    #[tokio::test(flavor = "current_thread")]
    async fn push_interface_state_picks_up_runtime_radio_reconfig() {
        use crate::interfaces::airtime::AirtimeCredit;
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        use leviculum_core::transport::InterfaceId;

        let tmp = std::env::temp_dir().join(format!("bug19-a2-reconfig-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let core: Arc<Mutex<StdNodeCore>> = {
            let node = leviculum_core::node::NodeCoreBuilder::new()
                .enable_transport(true)
                .build(
                    rand_core::OsRng,
                    SystemClock::new(),
                    crate::storage::Storage::new(&tmp).unwrap(),
                );
            Arc::new(Mutex::new(node))
        };

        let mut registry = InterfaceRegistry::new();
        let (_inc_tx, inc_rx) = tokio::sync::mpsc::channel(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(4);
        let credit = Arc::new(Mutex::new(AirtimeCredit::new(125_000, 7, 5, 24, 500)));
        let handle = InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(1),
                name: "lora-reconfig".into(),
                hw_mtu: Some(500),
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: inc_rx,
            outgoing: out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: Some(credit.clone()),
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        };
        registry.register(handle);

        push_interface_state(&mut registry, &core);
        let sf7_jitter = core.lock().unwrap().announce_jitter_max_ms();

        credit
            .lock()
            .unwrap()
            .update_radio_params(125_000, 10, 8, 18);
        push_interface_state(&mut registry, &core);
        let sf10_jitter = core.lock().unwrap().announce_jitter_max_ms();

        assert!(
            sf10_jitter > sf7_jitter,
            "SF10 jitter ({sf10_jitter}) must exceed SF7 ({sf7_jitter}) after reconfig"
        );
    }

    /// #126: `link_is_established` / `link_destination` across the link
    /// lifecycle, sans-I/O (never started, packets shuttled by hand):
    /// unknown id → false / None; PENDING link → false (while the raw
    /// presence probe `link_negotiated_mtu(..).is_some()` is already true —
    /// the distinction the accessor exists for) but destination already
    /// known; ACTIVE link → true / Some(dialed dest). The re-key alias leg
    /// (original id after a #66 establishment retry) is pinned by
    /// `rekey_alias_resolved_for_establishment_and_destination_reads` in
    /// leviculum-core, whose rig owns a warpable clock; the driver's
    /// `StdNodeCore` runs on the real-time `SystemClock`, which cannot reach
    /// the ≥12 s establishment timeout in a unit test.
    #[test]
    fn link_accessors_gate_on_established_and_expose_destination() {
        use leviculum_core::transport::{Action, InterfaceId, TickOutput};
        use leviculum_core::{
            Destination, DestinationType, Direction, Identity, LinkId, NoStorage, NodeCoreBuilder,
            ProofStrategy,
        };

        /// Single outbound packet of a tick; panics if not exactly one.
        fn one_packet(output: &TickOutput) -> Vec<u8> {
            let data: Vec<Vec<u8>> = output
                .actions
                .iter()
                .map(|a| match a {
                    Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => {
                        data.clone()
                    }
                })
                .collect();
            assert_eq!(
                data.len(),
                1,
                "expected exactly one outbound packet, got {}",
                data.len()
            );
            data.into_iter().next().unwrap()
        }

        // Responder: bare sans-I/O core owning a link-accepting destination.
        let identity = Identity::generate(&mut rand_core::OsRng);
        let signing_key = identity.ed25519_verifying().to_bytes();
        let mut responder =
            NodeCoreBuilder::new().build(rand_core::OsRng, SystemClock::new(), NoStorage);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "driverapp",
            &["accessors"],
        )
        .unwrap();
        dest.set_accepts_links(true);
        dest.set_proof_strategy(ProofStrategy::All);
        let dest_hash = *dest.hash();
        responder.register_destination(dest);

        // The driver under test: daemon-mode ReticulumNode, never started.
        let tmp = std::env::temp_dir().join(format!("link-accessors-126-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let core = NodeCoreBuilder::new().build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        let node = ReticulumNode::new(core, Vec::new(), None, false, 60, 4, 4);

        // 1. Unknown id: gate closed, no destination.
        let unknown = LinkId::new([0xEE; 16]);
        assert!(
            !node.link_is_established(&unknown),
            "unknown id must not read as established"
        );
        assert!(
            node.link_destination(&unknown).is_none(),
            "unknown id must have no destination"
        );

        // 2. PENDING link (request not yet answered): the presence probe is
        //    already true and the destination is already known, but the
        //    establishment gate must stay closed.
        let (link_id, _routed, out) = node
            .inner()
            .lock()
            .unwrap()
            .connect(dest_hash, &signing_key);
        assert!(
            node.link_negotiated_mtu(&link_id).is_some(),
            "presence probe must already be true for a pending link"
        );
        assert_eq!(
            node.link_destination(&link_id),
            Some(dest_hash),
            "pending link must already expose the dialed destination"
        );
        assert!(
            !node.link_is_established(&link_id),
            "pending link must not read as established"
        );

        // 3. Establish: request over, proof back.
        let request = one_packet(&out);
        let out = responder.handle_packet(InterfaceId(0), &request);
        let proof = one_packet(&out);
        let out = node
            .inner()
            .lock()
            .unwrap()
            .handle_packet(InterfaceId(0), &proof);
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, leviculum_core::NodeEvent::LinkEstablished { .. })),
            "proof must establish the link"
        );
        assert!(
            node.link_is_established(&link_id),
            "active link must read as established"
        );
        assert_eq!(
            node.link_destination(&link_id),
            Some(dest_hash),
            "active link must expose the dialed destination"
        );
    }

    // ---- Codeberg #196: the in-driver core processor seam --------------

    /// Test processor: records every event it is handed, and optionally
    /// answers with a canned `TickOutput`.
    #[derive(Default)]
    struct RecordingProcessor {
        seen: Arc<Mutex<Vec<String>>>,
        /// Emitted once, on the first event, as this processor's answer.
        answer: Option<TickOutput>,
        /// Wall-clock burn per event, to exercise the budget report.
        burn: Option<Duration>,
        tick_calls: Arc<AtomicUsize>,
        /// Panic on the first `on_event`, to exercise the unwind guard.
        panic_on_event: bool,
        /// Panic on the first `on_tick`, same.
        panic_on_tick: bool,
    }

    impl CoreProcessor for RecordingProcessor {
        fn on_event(&mut self, _core: &mut StdNodeCore, event: &NodeEvent) -> TickOutput {
            self.seen
                .lock_recover()
                .push(event.variant_name().to_string());
            if self.panic_on_event {
                panic!("consumer bug in on_event");
            }
            if let Some(burn) = self.burn {
                std::thread::sleep(burn);
            }
            self.answer.take().unwrap_or_else(TickOutput::empty)
        }

        fn on_tick(&mut self, _core: &mut StdNodeCore, _now_ms: u64) -> TickOutput {
            self.tick_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.panic_on_tick {
                panic!("consumer bug in on_tick");
            }
            self.answer.take().unwrap_or_else(TickOutput::empty)
        }
    }

    /// Collects `CORE_PROCESSOR_*` structured events off the tracing bus.
    ///
    /// Installed with `set_default`, so it is thread-local: a test that runs
    /// `run_event_tap` on another thread has to install it *there*.
    fn processor_event_probe() -> (
        Arc<Mutex<Vec<String>>>,
        impl tracing::Subscriber + Send + Sync,
    ) {
        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::SubscriberExt;

        struct Sink(Arc<Mutex<Vec<String>>>);
        impl Visit for Sink {
            fn record_debug(&mut self, _f: &Field, _v: &dyn std::fmt::Debug) {}
            fn record_str(&mut self, f: &Field, v: &str) {
                if f.name() == "event" {
                    self.0.lock_recover().push(v.to_string());
                }
            }
        }

        struct Layer(Arc<Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Layer {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                event.record(&mut Sink(Arc::clone(&self.0)));
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Layer(Arc::clone(&seen)));
        (seen, subscriber)
    }

    fn shared_test_core() -> (Arc<Mutex<StdNodeCore>>, tempfile::TempDir) {
        use leviculum_core::node::NodeCoreBuilder;
        let td = tempfile::tempdir().expect("tempdir");
        let core = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(td.path()).unwrap(),
        );
        (Arc::new(Mutex::new(core)), td)
    }

    fn packet_received(i: usize) -> NodeEvent {
        NodeEvent::PacketReceived {
            destination: leviculum_core::DestinationHash::new([0xCD; 16]),
            data: vec![i as u8],
            interface_index: i,
        }
    }

    /// Register an interface whose outgoing frames the test can read back.
    fn registry_with_readable_iface(
        registry: &mut InterfaceRegistry,
        id: usize,
    ) -> mpsc::Receiver<crate::interfaces::OutgoingPacket> {
        use crate::interfaces::{InterfaceCounters, InterfaceHandle, InterfaceInfo};
        let (_inc_tx, inc_rx) = mpsc::channel(4);
        let (out_tx, out_rx) = mpsc::channel(8);
        registry.register(InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(id),
                name: "test/readable".into(),
                hw_mtu: None,
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: inc_rx,
            outgoing: out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: None,
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        });
        out_rx
    }

    /// Constraint 2 of Codeberg #196, demonstrated rather than asserted from
    /// the code: the processor must observe an event that the lossy sink DROPS
    /// in the same run.
    ///
    /// `PacketReceived` is `EventClass::Data`, so a full data plane discards it
    /// silently — that is the designed backpressure (#71). For LXMF it is also
    /// how a message *arrives*, with nothing underneath to retransmit it. Three
    /// of them go into a data plane with room for one: the sink delivers one,
    /// and the tap sees all three.
    #[tokio::test(flavor = "current_thread")]
    async fn processor_tap_sees_the_data_event_the_lossy_sink_drops() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let processor = RecordingProcessor {
            seen: Arc::clone(&seen),
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        let mut output = TickOutput::empty();
        for i in 0..3 {
            output.events.push(packet_received(i));
        }

        // Data plane of capacity 1, never drained during dispatch.
        let (mut sink, mut rx) = sink_and_receiver(8, 1);
        dispatch_output(
            output,
            &mut registry,
            Some(&mut sink),
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        );

        let mut delivered = 0;
        while rx.try_recv().is_ok() {
            delivered += 1;
        }

        assert_eq!(
            delivered, 1,
            "the data plane has room for one; the sink must drop the other two"
        );
        assert_eq!(
            seen.lock_recover().len(),
            3,
            "the tap sits ahead of classification, so it must see all three — \
             including the two the sink dropped in this same run"
        );
    }

    /// The processor's `TickOutput` is transmitted on the driver's own send
    /// path, in the same `dispatch_output` that fed it the event. This is the
    /// mechanism behind constraint 3 (`PacketProofRequested`,
    /// `LinkProofRequested`, `ResourceAdvertised` cannot be deferred to a later
    /// tick); the end-to-end proof over a real interface is the
    /// `core_processor_seam` mvr.
    #[tokio::test(flavor = "current_thread")]
    async fn processor_answer_reaches_the_interface_in_the_same_tick() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();
        let mut out_rx = registry_with_readable_iface(&mut registry, 3);

        let mut answer = TickOutput::empty();
        answer
            .actions
            .push(leviculum_core::transport::Action::SendPacket {
                peer: None,
                iface: InterfaceId(3),
                data: vec![0xEE; 24],
            });
        let processor = RecordingProcessor {
            answer: Some(answer),
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        let mut output = TickOutput::empty();
        output.events.push(NodeEvent::PacketProofRequested {
            packet_hash: [0x11; 32],
            destination_hash: leviculum_core::DestinationHash::new([0xCD; 16]),
            interface_index: 3,
            peer: None,
        });

        dispatch_output(
            output,
            &mut registry,
            None,
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        );

        let frame = out_rx
            .try_recv()
            .expect("the answer must be on the interface before dispatch_output returns");
        assert_eq!(frame.data, vec![0xEE; 24]);
    }

    /// The recursion bound is one: a processor never sees the events of its own
    /// `TickOutput`. Unbounded here is a node hang, not a bug report.
    #[tokio::test(flavor = "current_thread")]
    async fn processor_does_not_observe_its_own_emitted_events() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();

        // The answer carries an event. If the seam re-entered the tap, the
        // processor would see it — and `LxmfNode::handle_event` emitting on
        // events would then never terminate.
        let mut answer = TickOutput::empty();
        answer.events.push(NodeEvent::InterfaceDown(9));

        let seen = Arc::new(Mutex::new(Vec::new()));
        let processor = RecordingProcessor {
            seen: Arc::clone(&seen),
            answer: Some(answer),
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        let mut output = TickOutput::empty();
        output.events.push(packet_received(0));

        let (mut sink, mut rx) = sink_and_receiver(8, 8);
        dispatch_output(
            output,
            &mut registry,
            Some(&mut sink),
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        );

        assert_eq!(
            *seen.lock_recover(),
            vec!["PacketReceived".to_string()],
            "the processor must see the core's event and NOT its own answer's"
        );

        // The answer's event still reaches the application: detaching the tap
        // on the recursive dispatch must not also silence the event sink.
        let mut forwarded = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            forwarded.push(ev.variant_name().to_string());
        }
        assert!(
            forwarded.contains(&"InterfaceDown".to_string()),
            "processor-produced events belong on the application stream: {forwarded:?}"
        );
    }

    /// An empty event list must not take the core lock at all — the tap runs on
    /// every dispatch, including the many that carry only actions.
    #[tokio::test(flavor = "current_thread")]
    async fn processor_is_not_consulted_when_the_tick_carried_no_events() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let processor = RecordingProcessor {
            seen: Arc::clone(&seen),
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        dispatch_output(
            TickOutput::empty(),
            &mut registry,
            None,
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        );

        assert!(seen.lock_recover().is_empty());
    }

    /// A processor that runs past `PROCESSOR_TICK_BUDGET` is reported. The
    /// bound is observational by necessity: a synchronous `fn` cannot be
    /// preempted, and moving it off-thread would take away the `&mut
    /// StdNodeCore` that is the whole seam.
    #[tokio::test(flavor = "current_thread")]
    async fn processor_over_the_tick_budget_is_reported() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tracing::field::{Field, Visit};

        #[derive(Default)]
        struct Seen(Arc<AtomicBool>);
        impl Visit for Seen {
            fn record_debug(&mut self, _f: &Field, _v: &dyn std::fmt::Debug) {}
            fn record_str(&mut self, f: &Field, v: &str) {
                if f.name() == "event" && v == "CORE_PROCESSOR_OVER_BUDGET" {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
        }

        struct Layer(Arc<AtomicBool>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Layer {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                event.record(&mut Seen(Arc::clone(&self.0)));
            }
        }

        let tripped = Arc::new(AtomicBool::new(false));
        let subscriber = {
            use tracing_subscriber::layer::SubscriberExt;
            tracing_subscriber::registry().with(Layer(Arc::clone(&tripped)))
        };
        let _guard = tracing::subscriber::set_default(subscriber);

        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();
        let processor = RecordingProcessor {
            burn: Some(PROCESSOR_TICK_BUDGET + Duration::from_millis(5)),
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        let mut output = TickOutput::empty();
        output.events.push(packet_received(0));

        dispatch_output(
            output,
            &mut registry,
            None,
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        );

        assert!(
            tripped.load(Ordering::Relaxed),
            "a processor over the {PROCESSOR_TICK_BUDGET:?} budget must emit \
             CORE_PROCESSOR_OVER_BUDGET"
        );
    }

    /// The scheduling half of the merge the split removed, pinned against the
    /// thing it replaced rather than argued.
    ///
    /// Before #196's A fix the timer branch called `TickOutput::merge` and read
    /// `next_deadline_ms` off the result. The merge had to go — it also fed the
    /// processor's events back into the tap, the `/status` responder and the
    /// discovery registry — but the deadline arithmetic was correct and had to
    /// survive verbatim. This drives both the old path and the new one over the
    /// same matrix and asserts they agree, including the boundary cases where
    /// `min()` and `or()` differ.
    #[test]
    fn merged_next_deadline_matches_what_tickoutput_merge_computed() {
        let cases = [None, Some(0_u64), Some(1), Some(500), Some(u64::MAX)];
        for core_ms in cases {
            for processor_ms in cases {
                // The old path, reconstructed exactly: merge, then read.
                let mut merged = TickOutput::empty();
                merged.next_deadline_ms = core_ms;
                let mut tick = TickOutput::empty();
                tick.next_deadline_ms = processor_ms;
                merged.merge(tick);

                assert_eq!(
                    merged_next_deadline(core_ms, processor_ms),
                    merged.next_deadline_ms,
                    "split scheduling diverges from the merge it replaced at \
                     core={core_ms:?} processor={processor_ms:?}"
                );
            }
        }
    }

    /// Finding H: `on_tick`'s `next_deadline_ms` was honoured by the timer
    /// branch while `on_event`'s was read by nobody, so the same field meant
    /// two different things depending on which hook filled it in.
    ///
    /// `dispatch_output` now hands the tap's deadline back as a delay for the
    /// caller to fold into `next_poll`.
    #[tokio::test(flavor = "current_thread")]
    async fn a_deadline_from_on_event_reaches_the_driver() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();

        let now_ms = core.lock_recover().now_ms();
        let mut answer = TickOutput::empty();
        answer.next_deadline_ms = Some(now_ms + 40);
        let processor = RecordingProcessor {
            answer: Some(answer),
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        let mut output = TickOutput::empty();
        output.events.push(packet_received(0));

        let delay = dispatch_output(
            output,
            &mut registry,
            None,
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        )
        .expect("the tap asked for a deadline; the driver must be told");

        // The clock the delay is measured against is read at the top of
        // dispatch_output, so the answer is 40 ms minus however long the call
        // itself took. Anything at or under 40 ms is the right side of the
        // boundary; a driver that ignored the request would have got `None`.
        assert!(
            delay <= Duration::from_millis(40),
            "delay {delay:?} must be the requested 40 ms, less time already spent"
        );
    }

    /// Finding G: `FramesDropped` is built inside `dispatch_output` and never
    /// passes through `handle_packet`, so the tap's "everything your actions
    /// cause comes back on a later tick" was false for exactly the #25 loss
    /// signal — the one a sender must see to re-send on a fresh link.
    ///
    /// A dead interface (receiver dropped) turns the processor's own
    /// `SendPacket` into a `Disconnected` dispatch error, and the notice must
    /// reach the tap in this same call.
    #[tokio::test(flavor = "current_thread")]
    async fn the_tap_sees_the_frames_dropped_a_send_on_a_dead_interface_caused() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();
        // Dropping the receiver is what an interface task dying looks like to
        // `try_send`: `Closed` → `InterfaceError::Disconnected`.
        let out_rx = registry_with_readable_iface(&mut registry, 5);
        drop(out_rx);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let processor = RecordingProcessor {
            seen: Arc::clone(&seen),
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        let mut output = TickOutput::empty();
        output
            .actions
            .push(leviculum_core::transport::Action::SendPacket {
                peer: None,
                iface: InterfaceId(5),
                data: vec![0x77; 16],
            });

        dispatch_output(
            output,
            &mut registry,
            None,
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        );

        assert_eq!(
            *seen.lock_recover(),
            vec!["FramesDropped".to_string()],
            "the loss signal #25 exists for must reach the processor that sent \
             the frame"
        );
    }

    /// Finding B: nothing wrapped the hooks, so a panic in third-party consumer
    /// code unwound through the tap's live guard, poisoned the core mutex, and
    /// killed the event loop — while the node kept its event-channel senders,
    /// so a consumer awaiting the stream never saw closure and waited forever.
    ///
    /// The unwind is now caught: the processor is detached for good, the
    /// consumer is told on the control plane, and the node carries on.
    #[tokio::test(flavor = "current_thread")]
    async fn a_panicking_on_event_detaches_the_processor_and_tells_the_consumer() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();

        let (logged, subscriber) = processor_event_probe();
        let _guard = tracing::subscriber::set_default(subscriber);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let processor = RecordingProcessor {
            seen: Arc::clone(&seen),
            panic_on_event: true,
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        let mut output = TickOutput::empty();
        output.events.push(packet_received(0));

        let (mut sink, mut rx) = sink_and_receiver(8, 8);
        // The default panic hook would print the backtrace of a panic the test
        // is provoking on purpose; silence it for the duration.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        dispatch_output(
            output,
            &mut registry,
            Some(&mut sink),
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        );
        std::panic::set_hook(previous_hook);

        assert!(
            !core.is_poisoned(),
            "the unwind must be caught inside the guard's scope, so the core \
             mutex is never poisoned"
        );
        assert!(
            logged
                .lock_recover()
                .contains(&"CORE_PROCESSOR_PANICKED".to_string()),
            "the panic must be reported on the structured event log: {:?}",
            logged.lock_recover()
        );

        let mut forwarded = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            forwarded.push(ev.variant_name().to_string());
        }
        assert!(
            forwarded.contains(&"CoreProcessorPanicked".to_string()),
            "the consumer learns on the stream it already reads: {forwarded:?}"
        );

        // Detached for good: a second dispatch must not call it again.
        seen.lock_recover().clear();
        let mut later = TickOutput::empty();
        later.events.push(packet_received(1));
        dispatch_output(
            later,
            &mut registry,
            None,
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &CompletionRegistry::new(),
            Some(&mut slot),
        );
        assert!(
            seen.lock_recover().is_empty(),
            "a processor that panicked once must never be called again"
        );
    }

    /// The same defence on the other hook. `on_tick` is called with a guard the
    /// timer branch holds, so an escaping unwind there poisons the core just as
    /// `on_event` did.
    #[test]
    fn a_panicking_on_tick_detaches_the_processor() {
        let (core, _td) = shared_test_core();

        let (logged, subscriber) = processor_event_probe();
        let _guard = tracing::subscriber::set_default(subscriber);

        let calls = Arc::new(AtomicUsize::new(0));
        let processor = RecordingProcessor {
            tick_calls: Arc::clone(&calls),
            panic_on_tick: true,
            ..Default::default()
        };
        let mut slot = processor::ProcessorSlot::new(Box::new(processor));

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let output = {
            let mut guard = core.lock_recover();
            let now_ms = guard.now_ms();
            processor::run_tick(&mut slot, &mut guard, now_ms)
        };
        std::panic::set_hook(previous_hook);

        assert!(
            !core.is_poisoned(),
            "the unwind must not escape the timer branch's guard"
        );
        assert!(
            output
                .events
                .iter()
                .any(|e| matches!(e, NodeEvent::CoreProcessorPanicked { hook: "on_tick" })),
            "the detach notice rides out on the tick's own output"
        );
        assert!(logged
            .lock_recover()
            .contains(&"CORE_PROCESSOR_PANICKED".to_string()));

        let before = calls.load(std::sync::atomic::Ordering::Relaxed);
        let _ = {
            let mut guard = core.lock_recover();
            let now_ms = guard.now_ms();
            processor::run_tick(&mut slot, &mut guard, now_ms)
        };
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            before,
            "a processor that panicked once must never be ticked again"
        );
    }

    /// Finding F: the budget clock started before `inner.lock_recover()`, so a
    /// 141 ms `send_resource` on another thread was charged to a processor that
    /// had not run yet — `CORE_PROCESSOR_OVER_BUDGET` against a hook that did
    /// nothing.
    ///
    /// Deterministic by construction: the lock is taken here *before* the tap
    /// thread is spawned, so the tap is guaranteed to wait for it.
    #[test]
    fn lock_wait_is_not_charged_to_the_processor() {
        let (core, _td) = shared_test_core();
        let hold = Duration::from_millis(20 * 5).max(PROCESSOR_TICK_BUDGET * 8);

        let guard = core.lock_recover();

        let tap_core = Arc::clone(&core);
        let tap = std::thread::spawn(move || {
            let (logged, subscriber) = processor_event_probe();
            // Thread-local: the tap runs here, so the probe has to live here.
            let _sub = tracing::subscriber::set_default(subscriber);
            let processor = RecordingProcessor::default();
            let mut slot = processor::ProcessorSlot::new(Box::new(processor));
            let event = packet_received(0);
            let _ = processor::run_event_tap(&mut slot, &tap_core, &[&event]);
            let out = logged.lock_recover().clone();
            out
        });

        std::thread::sleep(hold);
        drop(guard);

        let logged = tap.join().expect("tap thread");
        assert!(
            !logged.contains(&"CORE_PROCESSOR_OVER_BUDGET".to_string()),
            "a processor that did nothing must not be blamed for {hold:?} of \
             lock contention: {logged:?}"
        );
    }

    // ── Off-lock periodic storage flush (leviculum#44) ──

    /// Gate for the blocking flush write: the hook reports each entry on an
    /// async channel, then std-blocks the blocking-pool thread until the test
    /// releases it (only the first `block_first_n` calls block). The test body
    /// itself never std-blocks, so the current_thread flavor works. Dropping
    /// the gate unblocks the hook (a closed release channel returns Err).
    struct FlushGate {
        entered_rx: mpsc::UnboundedReceiver<usize>,
        release_tx: std::sync::mpsc::Sender<()>,
    }

    fn gated_flush_hook(block_first_n: usize) -> (crate::storage::FlushIoHook, FlushGate) {
        let (entered_tx, entered_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let hook: crate::storage::FlushIoHook = Arc::new(move || {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let _ = entered_tx.send(n);
            if n <= block_first_n {
                let _ = release_rx.lock().unwrap().recv();
            }
        });
        (
            hook,
            FlushGate {
                entered_rx,
                release_tx,
            },
        )
    }

    fn dirty_identity(core: &Arc<Mutex<StdNodeCore>>, tag: u8) {
        let id = leviculum_core::Identity::generate(&mut rand_core::OsRng);
        let mut guard = core.lock_recover();
        StorageTrait::set_identity(guard.storage_mut(), [tag; TRUNCATED_HASHBYTES], id);
    }

    /// A full `run_event_loop` with one readable interface, everything
    /// optional disabled, and the senders held open for the test's duration.
    struct FlushLoopHarness {
        action_tx: mpsc::Sender<TickOutput>,
        shutdown_tx: watch::Sender<bool>,
        loop_handle: tokio::task::JoinHandle<()>,
        out_rx: mpsc::Receiver<crate::interfaces::OutgoingPacket>,
        _in_tx: mpsc::Sender<IncomingPacket>,
        _new_iface_tx: mpsc::Sender<InterfaceHandle>,
        _reconnect_tx: mpsc::Sender<InterfaceId>,
        _tunnel_tx: mpsc::Sender<InterfaceId>,
        _remove_tx: mpsc::Sender<InterfaceId>,
    }

    fn spawn_flush_loop(
        core: Arc<Mutex<StdNodeCore>>,
        flush_interval_secs: u64,
    ) -> FlushLoopHarness {
        use crate::interfaces::{InterfaceCounters, InterfaceInfo};
        let mut registry = InterfaceRegistry::new();
        let (in_tx, in_rx) = mpsc::channel(4);
        let (out_tx, out_rx) = mpsc::channel(8);
        registry.register(InterfaceHandle {
            info: InterfaceInfo {
                id: InterfaceId(0),
                name: "test/flush-loop".into(),
                hw_mtu: None,
                is_local_client: false,
                bitrate: None,
                tx_jitter_max_ms: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Unknown,
                ingress_control: None,
            },
            incoming: in_rx,
            outgoing: out_tx,
            counters: Arc::new(InterfaceCounters::new()),
            credit: None,
            ready: crate::interfaces::ReadySignal::ready_immediate(),
        });

        let (action_tx, action_rx) = mpsc::channel(8);
        let (new_iface_tx, new_iface_rx) = mpsc::channel(1);
        let (reconnect_tx, reconnect_rx) = mpsc::channel(1);
        let (tunnel_tx, tunnel_rx) = mpsc::channel(1);
        let (remove_tx, remove_rx) = mpsc::channel(1);
        let (_peer_event_tx, peer_event_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let loop_handle = tokio::spawn(run_event_loop(
            core,
            registry,
            EventLoopChannels {
                event_sink: None,
                action_dispatch_rx: action_rx,
                new_interface_rx: new_iface_rx,
                reconnect_rx,
                tunnel_notify_rx: tunnel_rx,
                remove_iface_rx: remove_rx,
                peer_event_rx,
                shutdown: shutdown_rx,
            },
            Arc::new(Mutex::new(BTreeMap::new())),
            crate::interfaces::inventory::InterfaceInventory::shared(),
            Arc::new(AtomicUsize::new(0)),
            flush_interval_secs,
            None,
            None,
            None,
            AutoConnectWiring {
                max: 0,
                new_iface_tx: new_iface_tx.clone(),
                reconnect_tx: reconnect_tx.clone(),
                next_id: Arc::new(AtomicUsize::new(1)),
                corrupt_every: None,
                outbound_socket_hook: None,
            },
            None,
            None,
            CompletionRegistry::new(),
        ));

        FlushLoopHarness {
            action_tx,
            shutdown_tx,
            loop_handle,
            out_rx,
            _in_tx: in_tx,
            _new_iface_tx: new_iface_tx,
            _reconnect_tx: reconnect_tx,
            _tunnel_tx: tunnel_tx,
            _remove_tx: remove_tx,
        }
    }

    /// C1's core claim for this lane: while the flush write is on the
    /// blocking pool, the node lock is free — free enough to dirty the
    /// storage mid-write, which the generation guard then keeps dirty.
    #[tokio::test(flavor = "current_thread")]
    async fn flush_write_runs_off_the_node_lock() {
        use crate::known_destinations::{decode_known_destinations, KNOWN_DESTINATIONS_FILE};

        let (core, td) = shared_test_core();
        let (hook, mut gate) = gated_flush_hook(1);
        core.lock_recover().storage_mut().set_flush_io_hook(hook);
        dirty_identity(&core, 0x0A);

        let handle = begin_flush(&core).expect("dirty storage must begin a flush");
        gate.entered_rx.recv().await.expect("hook must be entered");

        {
            let mut guard = core
                .try_lock()
                .expect("the node lock must be free while the flush write runs");
            let id = leviculum_core::Identity::generate(&mut rand_core::OsRng);
            StorageTrait::set_identity(guard.storage_mut(), [0x0B; TRUNCATED_HASHBYTES], id);
        }

        gate.release_tx.send(()).expect("blocked hook receives");
        let joined = handle.await;
        settle_flush(&core, joined);

        assert!(
            core.lock_recover()
                .storage_mut()
                .take_flush_snapshot()
                .is_some(),
            "the identity added mid-write must stay dirty for the next interval"
        );

        let bytes = std::fs::read(td.path().join(KNOWN_DESTINATIONS_FILE)).unwrap();
        let entries = decode_known_destinations(&bytes).unwrap();
        assert!(
            entries.contains_key(&[0x0A; TRUNCATED_HASHBYTES]),
            "the pre-gate identity must be on disk"
        );
    }

    /// The hourly deaf window this lane removes: a SendPacket pushed while
    /// the flush write is stalled must still reach the interface.
    #[tokio::test(flavor = "current_thread")]
    async fn loop_stays_responsive_during_slow_flush() {
        let (core, _td) = shared_test_core();
        let (hook, mut gate) = gated_flush_hook(1);
        core.lock_recover().storage_mut().set_flush_io_hook(hook);
        dirty_identity(&core, 0x1A);

        let mut harness = spawn_flush_loop(Arc::clone(&core), 1);
        gate.entered_rx.recv().await.expect("first flush begins");

        let mut output = TickOutput::empty();
        output
            .actions
            .push(leviculum_core::transport::Action::SendPacket {
                peer: None,
                iface: InterfaceId(0),
                data: vec![0xAB; 32],
            });
        harness.action_tx.send(output).await.expect("loop is live");

        let frame = tokio::time::timeout(Duration::from_secs(2), harness.out_rx.recv())
            .await
            .expect("the loop must dispatch while the flush write is in flight")
            .expect("interface channel open");
        assert_eq!(frame.data, vec![0xAB; 32]);

        gate.release_tx.send(()).expect("blocked hook receives");
        harness.shutdown_tx.send(true).expect("loop is live");
        harness.loop_handle.await.expect("loop exits cleanly");
    }

    /// The inbound half of the deaf window: a live announce arriving from an
    /// interface while the flush write is stalled must still be processed
    /// under the (free) node lock — observed as the announced identity
    /// landing in storage before the write is released.
    #[tokio::test(flavor = "current_thread")]
    async fn inbound_processing_proceeds_during_slow_flush() {
        use leviculum_core::{DestinationType, Direction};

        let (core, _td) = shared_test_core();
        let (hook, mut gate) = gated_flush_hook(1);
        core.lock_recover().storage_mut().set_flush_io_hook(hook);
        dirty_identity(&core, 0x4A);

        let harness = spawn_flush_loop(Arc::clone(&core), 1);
        gate.entered_rx.recv().await.expect("flush begins");

        // A real announce from a throwaway peer, fed through the registered
        // interface's incoming channel while the write is gated.
        let peer = leviculum_core::Identity::generate(&mut rand_core::OsRng);
        let mut dest = Destination::new(
            Some(peer),
            Direction::In,
            DestinationType::Single,
            "test",
            &["flushgate"],
        )
        .unwrap();
        let dest_hash = dest.hash().into_bytes();
        let announce = dest
            .announce(None, &mut rand_core::OsRng, 12_000, 1_700_000_000)
            .unwrap();
        let mut buf = [0u8; 500];
        let len = announce.pack(&mut buf).unwrap();
        harness
            ._in_tx
            .send(crate::interfaces::IncomingPacket {
                data: buf[..len].to_vec(),
            })
            .await
            .expect("loop is live");

        // The identity must land while the flush write is still in flight.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if core
                    .lock_recover()
                    .storage()
                    .get_identity(&dest_hash)
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the announce must be processed while the flush write is in flight");

        gate.release_tx.send(()).expect("blocked hook receives");
        harness.shutdown_tx.send(true).expect("loop is live");
        harness.loop_handle.await.expect("loop exits cleanly");
    }

    /// The JoinHandle is the overlap guard: timer fires during an in-flight
    /// write re-arm and do nothing, and the interval after the write settles
    /// retries whatever was dirtied mid-write.
    #[tokio::test(flavor = "current_thread")]
    async fn flush_intervals_never_overlap_and_retry_next_interval() {
        let (core, _td) = shared_test_core();
        let (hook, mut gate) = gated_flush_hook(1);
        core.lock_recover().storage_mut().set_flush_io_hook(hook);
        dirty_identity(&core, 0x2A);

        let harness = spawn_flush_loop(Arc::clone(&core), 1);
        let first = gate.entered_rx.recv().await.expect("first flush begins");
        assert_eq!(first, 1);

        // More than two timer fires pass while the write is gated; each must
        // re-arm without starting a second write.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            gate.entered_rx.try_recv().is_err(),
            "a timer fire during an in-flight write must not begin another"
        );

        // Dirtied mid-gate: the settle keeps the flag set, so the next
        // interval retries.
        dirty_identity(&core, 0x2B);
        gate.release_tx.send(()).expect("blocked hook receives");

        let second = tokio::time::timeout(Duration::from_secs(5), gate.entered_rx.recv())
            .await
            .expect("the mid-gate dirtying must be retried on the next interval")
            .expect("hook channel open");
        assert_eq!(second, 2);

        harness.shutdown_tx.send(true).expect("loop is live");
        harness.loop_handle.await.expect("loop exits cleanly");
    }

    /// Shutdown joins the in-flight write before the loop exits, so stop()'s
    /// synchronous flush can never be clobbered by a stale background rename.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_joins_the_in_flight_flush_write() {
        use crate::known_destinations::{decode_known_destinations, KNOWN_DESTINATIONS_FILE};

        let (core, td) = shared_test_core();
        let (hook, mut gate) = gated_flush_hook(1);
        core.lock_recover().storage_mut().set_flush_io_hook(hook);
        dirty_identity(&core, 0x3A);

        let mut harness = spawn_flush_loop(Arc::clone(&core), 1);
        gate.entered_rx.recv().await.expect("flush begins");

        harness.shutdown_tx.send(true).expect("loop is live");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut harness.loop_handle)
                .await
                .is_err(),
            "the loop must wait for the in-flight write before exiting"
        );

        gate.release_tx.send(()).expect("blocked hook receives");
        harness.loop_handle.await.expect("loop exits cleanly");

        let bytes = std::fs::read(td.path().join(KNOWN_DESTINATIONS_FILE)).unwrap();
        let entries = decode_known_destinations(&bytes).unwrap();
        assert!(
            entries.contains_key(&[0x3A; TRUNCATED_HASHBYTES]),
            "the joined write must have landed the snapshot on disk"
        );
    }

    // ---- leviculum#42: completion futures at the dispatch layer ---------

    /// Poll a completion exactly once with a no-op waker.
    fn poll_completion<T>(
        fut: &mut completions::Completion<T>,
    ) -> Poll<std::result::Result<T, CompletionError>> {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        std::future::Future::poll(std::pin::Pin::new(fut), &mut cx)
    }

    /// C2 pin: the completion hook OBSERVES at the dispatch layer — the
    /// registered waiter resolves AND the primary `EventReceiver` still gets
    /// the same event. An observer that consumed would starve the stream the
    /// application already reads.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_output_resolves_registered_link_waiter_and_still_forwards_event() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();
        let completions = CompletionRegistry::new();

        let link_id = LinkId::new([0x42; 16]);
        let mut fut = completions.register_link_established(link_id);
        assert!(poll_completion(&mut fut).is_pending());

        let mut output = TickOutput::empty();
        output.events.push(NodeEvent::LinkEstablished {
            link_id,
            is_initiator: true,
            destination_hash: leviculum_core::DestinationHash::new([0xAA; 16]),
        });

        let (mut sink, mut rx) = sink_and_receiver(8, 8);
        dispatch_output(
            output,
            &mut registry,
            Some(&mut sink),
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &completions,
            None,
        );

        assert_eq!(fut.await, Ok(()));
        let ev = rx
            .recv()
            .await
            .expect("the primary receiver must still get the event");
        assert!(
            matches!(ev, NodeEvent::LinkEstablished { .. }),
            "expected LinkEstablished, got {ev:?}"
        );
    }

    /// Daemon mode (`without_events()`, sink None) must still resolve
    /// waiters: the hook sits ahead of event forwarding, on the raw
    /// `TickOutput` the NRF daemon path simply drops.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_output_resolves_waiters_in_daemon_mode() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();
        let completions = CompletionRegistry::new();

        let link_id = LinkId::new([0x43; 16]);
        let fut = completions.register_link_established(link_id);

        let mut output = TickOutput::empty();
        output.events.push(NodeEvent::LinkEstablished {
            link_id,
            is_initiator: true,
            destination_hash: leviculum_core::DestinationHash::new([0xAB; 16]),
        });

        dispatch_output(
            output,
            &mut registry,
            None,
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &completions,
            None,
        );

        assert_eq!(fut.await, Ok(()));
    }

    /// leviculum#42 end-to-end, sans-I/O (the #126 two-core harness): an
    /// `await_link_established` registered while the link is Pending resolves
    /// once the proof-bearing `TickOutput` passes through `dispatch_output`
    /// with the node's own registry; a second await after establishment
    /// resolves immediately via the mirror.
    #[tokio::test(flavor = "current_thread")]
    async fn await_link_established_resolves_through_dispatch_on_never_started_node() {
        use leviculum_core::transport::{Action, InterfaceId, TickOutput};
        use leviculum_core::{
            Destination, DestinationType, Direction, Identity, NoStorage, NodeCoreBuilder,
            ProofStrategy,
        };

        fn one_packet(output: &TickOutput) -> Vec<u8> {
            let data: Vec<Vec<u8>> = output
                .actions
                .iter()
                .map(|a| match a {
                    Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => {
                        data.clone()
                    }
                })
                .collect();
            assert_eq!(data.len(), 1, "expected exactly one outbound packet");
            data.into_iter().next().unwrap()
        }

        // Responder: bare sans-I/O core owning a link-accepting destination.
        let identity = Identity::generate(&mut rand_core::OsRng);
        let signing_key = identity.ed25519_verifying().to_bytes();
        let mut responder =
            NodeCoreBuilder::new().build(rand_core::OsRng, SystemClock::new(), NoStorage);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "driverapp",
            &["completions"],
        )
        .unwrap();
        dest.set_accepts_links(true);
        dest.set_proof_strategy(ProofStrategy::All);
        let dest_hash = *dest.hash();
        responder.register_destination(dest);

        // The driver under test: daemon-mode ReticulumNode, never started.
        let td = tempfile::tempdir().expect("tempdir");
        let core = NodeCoreBuilder::new().build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(td.path()).unwrap(),
        );
        let node = ReticulumNode::new(core, Vec::new(), None, false, 60, 4, 4);

        let (link_id, _routed, out) = node
            .inner()
            .lock()
            .unwrap()
            .connect(dest_hash, &signing_key);

        // Registered while Pending: nothing to resolve yet.
        let mut fut = node.await_link_established(&link_id);
        assert!(poll_completion(&mut fut).is_pending());

        // Shuttle by hand: request over, proof back; the proof's TickOutput
        // goes through the dispatch layer exactly as the live loop would.
        let request = one_packet(&out);
        let out = responder.handle_packet(InterfaceId(0), &request);
        let proof = one_packet(&out);
        let out = node
            .inner()
            .lock()
            .unwrap()
            .handle_packet(InterfaceId(0), &proof);

        let inner = node.inner();
        let mut registry = InterfaceRegistry::new();
        dispatch_output(
            out,
            &mut registry,
            None,
            &inner,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &node.completions,
            None,
        );

        assert_eq!(fut.await, Ok(()));

        // Immediate-resolve path: the mirror answers a second await without
        // any event in flight.
        assert_eq!(node.await_link_established(&link_id).await, Ok(()));
    }

    /// Surface (b), C2 pin: a tap subscriber receives the event AND the
    /// primary `EventReceiver` still gets it — the tap is fed clones at the
    /// dispatch layer, it never consumes from the two-plane channels.
    #[tokio::test(flavor = "current_thread")]
    async fn tap_receives_events_without_consuming_primary() {
        let (core, _td) = shared_test_core();
        let mut registry = InterfaceRegistry::new();
        let completions = CompletionRegistry::new();
        let mut tap = completions.subscribe();

        let mut output = TickOutput::empty();
        output.events.push(NodeEvent::LinkEstablished {
            link_id: LinkId::new([0x44; 16]),
            is_initiator: false,
            destination_hash: leviculum_core::DestinationHash::new([0xAC; 16]),
        });

        let (mut sink, mut rx) = sink_and_receiver(8, 8);
        dispatch_output(
            output,
            &mut registry,
            Some(&mut sink),
            &core,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &completions,
            None,
        );

        match tap.try_recv() {
            Some(TapEvent::Event(NodeEvent::LinkEstablished { link_id, .. })) => {
                assert_eq!(link_id, LinkId::new([0x44; 16]));
            }
            other => panic!("the tap must see the event, got {other:?}"),
        }
        let ev = rx
            .recv()
            .await
            .expect("the primary receiver must be unaffected by the tap");
        assert!(
            matches!(ev, NodeEvent::LinkEstablished { .. }),
            "expected LinkEstablished, got {ev:?}"
        );
    }

    /// merge/pr253 review finding: the remote-management `/status` responder
    /// commits its response Resource through `core.send_response_resource`
    /// directly, so it must note the send like every driver send path does —
    /// an un-noted transfer's terminal event under-counts `pending_sends` and
    /// its link-scoped sweep resolves a waiter belonging to the NEXT send on
    /// the link: the stale-sweep class 3d3d74f closed for app sends, reopened
    /// through the mgmt side door.
    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "rpc")]
    async fn mgmt_response_resource_is_noted_and_does_not_sweep_the_next_sends_waiter() {
        use leviculum_core::traits::InterfaceMode;
        use leviculum_core::transport::{Action, InterfaceId, TickOutput};
        use leviculum_core::{
            Destination, DestinationType, Direction, Identity, NoStorage, NodeCoreBuilder,
            ProofStrategy, RequestPolicy,
        };

        fn packets(out: &TickOutput) -> Vec<Vec<u8>> {
            out.actions
                .iter()
                .map(|a| match a {
                    Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => {
                        data.clone()
                    }
                })
                .collect()
        }

        // The driver under test, owning the destination the mgmt client calls.
        let td = tempfile::tempdir().expect("tempdir");
        let core = NodeCoreBuilder::new().build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(td.path()).unwrap(),
        );
        let node = ReticulumNode::new(core, Vec::new(), None, false, 60, 4, 4);

        let identity = Identity::generate(&mut rand_core::OsRng);
        let signing_key = identity.ed25519_verifying().to_bytes();
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "driverapp",
            &["mgmtnote"],
        )
        .unwrap();
        dest.set_accepts_links(true);
        dest.set_proof_strategy(ProofStrategy::All);
        let dest_hash = *dest.hash();
        {
            let inner = node.inner();
            let mut core = inner.lock().unwrap();
            core.register_destination(dest);
            core.register_request_handler(dest_hash, "/status", RequestPolicy::AllowAll);
        }

        // The mgmt-client role: a bare sans-I/O core initiating the link.
        let mut peer =
            NodeCoreBuilder::new().build(rand_core::OsRng, SystemClock::new(), NoStorage);
        let (peer_link_id, _routed, out) = peer.connect(dest_hash, &signing_key);

        // Hand-shuttle until both sides go quiet. Observation stays manual:
        // nothing reaches the completion registry until the test dispatches
        // it, which is what lets the stale window be constructed exactly.
        let mut node_outputs: Vec<TickOutput> = Vec::new();
        let mut to_node: Vec<Vec<u8>> = packets(&out);
        type PeerCore = leviculum_core::node::NodeCore<rand_core::OsRng, SystemClock, NoStorage>;
        let pump = |peer: &mut PeerCore,
                    to_node: &mut Vec<Vec<u8>>,
                    node_outputs: &mut Vec<TickOutput>| {
            for _ in 0..16 {
                let mut to_peer: Vec<Vec<u8>> = Vec::new();
                for pkt in to_node.drain(..) {
                    let out = node
                        .inner()
                        .lock()
                        .unwrap()
                        .handle_packet(InterfaceId(0), &pkt);
                    to_peer.extend(packets(&out));
                    node_outputs.push(out);
                }
                if to_peer.is_empty() {
                    break;
                }
                for pkt in to_peer.drain(..) {
                    let out = peer.handle_packet(InterfaceId(0), &pkt);
                    to_node.extend(packets(&out));
                }
                if to_node.is_empty() {
                    break;
                }
            }
        };
        pump(&mut peer, &mut to_node, &mut node_outputs);

        // The peer requests /status over the established link.
        let (_req_id, out) = peer
            .send_request(&peer_link_id, "/status", None, None)
            .expect("request over the established link");
        let mut to_node = packets(&out);
        pump(&mut peer, &mut to_node, &mut node_outputs);

        let (link_id, request_id) = node_outputs
            .iter()
            .find_map(|o| {
                o.events.iter().find_map(|e| match e {
                    NodeEvent::RequestReceived {
                        link_id,
                        request_id,
                        ..
                    } => Some((*link_id, *request_id)),
                    _ => None,
                })
            })
            .expect("the /status request must reach the node");

        // A responder whose inventory inflates the response past the link
        // MDU, forcing the response-Resource branch (the branch under test).
        let stats_map: crate::interfaces::InterfaceStatsMap =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let inventory = crate::interfaces::inventory::InterfaceInventory::shared();
        {
            let mut inv = inventory.lock_recover();
            for i in 0..16usize {
                inv.add_listener(
                    1000 + i,
                    crate::interfaces::inventory::ListenerRow {
                        identity: crate::interfaces::inventory::InterfaceIdentity {
                            name: format!(
                                "TCPServerInterface[padding-{}/198.51.100.{}:4242]",
                                "x".repeat(64),
                                i
                            ),
                            short_name: format!("listener-{i}"),
                            type_name: "TCPServerInterface",
                            parent: None,
                        },
                        bitrate: 10_000_000,
                        hw_mtu: 262_144,
                        mode: InterfaceMode::Full,
                        announce_rate: (None, None, None),
                        ifac_size_bits: None,
                        departed_rxb: 0,
                        departed_txb: 0,
                        bound_addr: None,
                    },
                );
            }
        }
        let responder = RemoteMgmtResponder::new(
            stats_map,
            inventory,
            std::time::Instant::now(),
            AutoPeerCount::default(),
        );
        let resp = responder
            .handle_request(
                &node.inner(),
                &link_id,
                &request_id,
                "/status",
                &[],
                &node.completions,
            )
            .expect("a /status request gets a response");

        // Shuttle the response Resource to completion; the proof-bearing
        // terminal TickOutput is HELD, not dispatched — the stale window
        // (transfer complete in core, terminal event not yet observed).
        let mut to_node: Vec<Vec<u8>> = Vec::new();
        for pkt in packets(&resp) {
            let out = peer.handle_packet(InterfaceId(0), &pkt);
            to_node.extend(packets(&out));
        }
        pump(&mut peer, &mut to_node, &mut node_outputs);
        let term_idx = node_outputs
            .iter()
            .position(|o| {
                o.events.iter().any(|e| {
                    matches!(
                        e,
                        NodeEvent::ResourceCompleted {
                            is_sender: true,
                            ..
                        }
                    )
                })
            })
            .expect("the response resource must complete sender-side");
        let term = node_outputs.swap_remove(term_idx);

        // The NEXT send on the same link: note + register, exactly the pair
        // `send_resource_awaited` performs before dispatching.
        node.completions.note_send_began(link_id);
        let mut fut = node.completions.register_resource_sent([0xB1; 32], link_id);
        assert!(poll_completion(&mut fut).is_pending());

        // Only now does the mgmt transfer's terminal event dispatch.
        let inner = node.inner();
        let mut registry = InterfaceRegistry::new();
        dispatch_output(
            term,
            &mut registry,
            None,
            &inner,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &node.completions,
            None,
        );
        assert!(
            poll_completion(&mut fut).is_pending(),
            "the mgmt response's terminal event must not sweep the next send's waiter"
        );

        // The newer send's own terminal event is what resolves it.
        let mut own = TickOutput::empty();
        own.events.push(NodeEvent::ResourceCompleted {
            link_id,
            resource_hash: [0xB1; 32],
            data: Vec::new(),
            metadata: None,
            is_sender: true,
            segment_index: 1,
            total_segments: 1,
        });
        dispatch_output(
            own,
            &mut registry,
            None,
            &inner,
            &mut BTreeMap::new(),
            &mut std::collections::BTreeSet::new(),
            &mut BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
            None,
            &mut BTreeMap::new(),
            &node.completions,
            None,
        );
        assert!(matches!(poll_completion(&mut fut), Poll::Ready(Ok(_))));
    }
}
