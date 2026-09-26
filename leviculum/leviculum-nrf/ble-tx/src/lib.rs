//! Outbound BLE notification flow control (Codeberg #264).
//!
//! A Reticulum packet larger than one BLE fragment is handed to the
//! SoftDevice one `sd_ble_gatts_hvx` call at a time, and the SoftDevice
//! accepts at most `hvn_tx_queue_size` notifications per connection
//! before it starts refusing with `NRF_ERROR_RESOURCES`. On S140 that
//! queue is **one** entry deep by default
//! (`BLE_GATTS_HVN_TX_QUEUE_SIZE_DEFAULT = 1`), so from the second
//! fragment onwards *every* call is refused until the first one has
//! actually gone out over the air. A loop that ignores the return value
//! therefore delivers fragment 0 and silently discards 1..N — the peer
//! waits forever for an assembly that will never complete, and nothing
//! on our side knows a packet was lost.
//!
//! The decision that fixes this — retry the same fragment after the
//! queue drains, abort and report on anything else, never re-send a
//! fragment that already went out — is a state machine over notify
//! results and SoftDevice events. Nothing in it needs a radio, so it
//! lives here and is exercised on the host, next to
//! [`leviculum-sd-policy`] and [`leviculum-telemetry-policy`] and for
//! the same reason: a state machine that needs hardware to be exercised
//! is a state machine that is never exercised.
//!
//! The firmware side stays a thin driver: perform the [`Action`] the
//! machine asks for, feed back the [`Event`] it produced, repeat. Each
//! action is performed **exactly once**, which is what makes
//! "every fragment goes out in order and exactly once" a structural
//! property rather than a hope — the machine is the only source of
//! actions, and it only ever emits [`Action::Send`] for a fragment it
//! has not yet seen acknowledged.
//!
//! [`leviculum-sd-policy`]: https://codeberg.org/Lew_Palm/leviculum
//! [`leviculum-telemetry-policy`]: https://codeberg.org/Lew_Palm/leviculum

#![cfg_attr(not(test), no_std)]

pub mod adv;
pub mod drain;
pub mod gap;
pub mod hold;
pub mod peer;
pub mod registry;
pub mod window;

pub use adv::{
    ad_structure_len, manufacturer_data, ADV_BYTES_USED, CAP_PERIPHERAL_ONLY, COMPANY_ID,
    LEGACY_AD_CAPACITY, MANUFACTURER_DATA_LEN, PROTOCOL_VERSION,
};
pub use drain::{DrainRouter, DrainSlot, NO_CONN_HANDLE};
pub use gap::{effective_tx_gap_ms, TxGap, DEFAULT_TX_GAP_MS};
pub use hold::TxHold;
pub use peer::{
    addr_value, parse_peer_advertisement, should_initiate, ConnectDecision, PeerAdvertisement,
    ScanMode,
};
pub use registry::{
    judge_duplicate, plan_fanout, Duplicate, LinkUp, Origin, PeerRegistry, TxFanout,
    LINK_TIMEOUT_MS,
};
pub use window::{
    CandidateTable, SCAN_FALLBACK_AFTER_MS, SCAN_WINDOW_COLLECT_MS, WINDOW_CANDIDATES,
};

/// Upper bound on a single wait for the SoftDevice's
/// `BLE_GATTS_EVT_HVN_TX_COMPLETE`, in milliseconds.
///
/// The queue drains at most one notification per connection event, i.e.
/// once per connection interval. Centrals that matter for a Reticulum
/// link — Columba on Android, BlueZ, iOS — negotiate intervals in the
/// 7.5 ms to 100 ms range while the link is active, and our own
/// `conn_gap.event_length` of 24 (× 1.25 ms = 30 ms) is sized for that
/// regime. 2 s is therefore at least 20 connection events even at the
/// slow end of it: a healthy link never reaches this bound.
///
/// The upper side is bounded by the link-supervision timeout. Once the
/// peer stops responding the stack tears the connection down on its own
/// and the next `notify_value` returns `Disconnected`. Waiting longer
/// than a couple of seconds would just race that teardown and replace a
/// precise "the queue stalled" diagnosis with a generic disconnect. 2 s
/// sits below the multi-second supervision timeouts in common use, so a
/// stall is reported as a stall.
///
/// Residual: a spec-legal but exotic central negotiating an interval
/// above 2 s spends extra waits from the budget on every fragment
/// beyond the first. Since a timed-out wait re-offers the fragment
/// rather than aborting (#255 — one timeout used to tear the packet
/// mid-stream), such a peer sees delay, not loss; only a queue that
/// stays wedged for the whole budget aborts, and that abort is
/// *visible* (event + counter) rather than silent.
pub const DRAIN_WAIT_MS: u64 = 2_000;

/// How many drain waits one packet may spend before it is abandoned.
///
/// With a one-deep queue the expected cost is one wait per fragment
/// after the first, and each wait ends with a drain event that means a
/// fragment genuinely left the device. `2 * fragments + 4` leaves room
/// for a stale drain edge left over from an earlier packet and for a
/// deeper queue, while still bounding a pathological peer that keeps
/// signalling drains without ever making room. Hitting the budget is
/// itself a diagnosis, which is why it aborts with its own reason
/// instead of looping.
#[must_use]
pub fn drain_wait_budget(fragment_count: usize) -> u32 {
    let n = u32::try_from(fragment_count).unwrap_or(u32::MAX / 4);
    n.saturating_mul(2).saturating_add(4)
}

/// Length in bytes of the advertised device name: `LN-` + 8 hex digits.
///
/// Sized against the scan response, the tighter of the two surfaces the
/// name occupies: a legacy scan-response PDU carries 31 bytes of AD
/// structures, a Complete Local Name structure costs 2 bytes of
/// overhead (length + AD type), and our scan response is name-only, so
/// up to 29 name bytes would fit. 11 keeps 18 bytes in reserve for
/// anything the scan response carries later.
///
/// The GAP device-name attribute has a second, softer ceiling of its
/// own: `BLE_GAP_DEVNAME_DEFAULT_LEN` (31). The name is reserved in the
/// SoftDevice's attribute table (`p_value` NULL under
/// `BLE_GATTS_VLOC_STACK`, see `leviculum_nrf::ble::init`), and the
/// bindings note that going past 31 there "the attribute table size must
/// be increased to have room for the longer device name". Nothing on the
/// board would say so — the config would simply fail to apply — so the
/// bound is asserted below instead.
pub const DEVICE_NAME_LEN: usize = 11;

/// `BLE_GAP_DEVNAME_DEFAULT_LEN`: the longest GAP device name the
/// SoftDevice's default attribute table has room for.
///
/// Mirrored from the `nrf-softdevice-s140` bindings rather than imported:
/// this crate is the pure, host-tested half and does not link the
/// SoftDevice. See [`DEVICE_NAME_LEN`].
pub const GAP_DEVNAME_DEFAULT_LEN: usize = 31;

/// The drift guard for [`leviculum_core::node_name::BLE_NAME_MAX_LEN`],
/// which mirrors [`DEVICE_NAME_LEN`] so a host tool can shorten a name
/// exactly as this crate does. A `const` block, so the two parting
/// company fails the build rather than one test run.
const _: () = assert!(DEVICE_NAME_LEN == leviculum_core::node_name::BLE_NAME_MAX_LEN);

/// The node's individual BLE name: `LN-<hex8>` (#255).
///
/// `<hex8>` is the leading 4 bytes of the identity hash — the same
/// bytes the LXMF announce's `app_data` display name `LNode-<hex8>`
/// (`leviculum-nrf/src/telemetry.rs`, `announce_app_data`) is built
/// from, so a BLE scanner listing and a Columba contact list show the
/// same hex for the same node. The full identity hash is also what the
/// GATT identity characteristic publishes; the name is a readable
/// prefix of it. The LXMF *destination* hash is deliberately not the
/// source: no name Columba ever displays is derived from it.
///
/// The output is ASCII by construction, so it is always valid UTF-8.
#[must_use]
pub fn device_name(identity_hash: &[u8; 16]) -> [u8; DEVICE_NAME_LEN] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = *b"LN-00000000";
    for (i, byte) in identity_hash[..4].iter().enumerate() {
        name[3 + 2 * i] = HEX[usize::from(byte >> 4)];
        name[4 + 2 * i] = HEX[usize::from(byte & 0x0F)];
    }
    name
}

/// The BLE name this node advertises: the operator's name if one is set,
/// the derived [`device_name`] otherwise (#235).
///
/// Returns the buffer and how much of it is in use — the derived name is
/// always [`DEVICE_NAME_LEN`] bytes, an operator's name is whatever fits.
///
/// **One name, both surfaces.** The value handed in here is the same one
/// the LXMF announce carries as its display name, so a board listed as
/// `Balkon-Nord` in Columba is listed as `Balkon-Nord` in a phone's
/// Bluetooth settings too. Where the mesh name does not fit
/// [`DEVICE_NAME_LEN`], BLE gets a **visibly shortened** prefix rather
/// than a different name: see
/// [`leviculum_core::node_name::truncate_on_char_boundary`] for the two
/// rules that shortening obeys.
///
/// The derived default is deliberately *not* run through the truncation:
/// `LN-<hex8>` is exactly `DEVICE_NAME_LEN` bytes by construction, and
/// it is a different string from the mesh default `LNode-<hex8>` rather
/// than a prefix of it, because 11 bytes of `LNode-<hex8>` would drop
/// half the hex that makes the board identifiable.
#[must_use]
pub fn gap_name(
    identity_hash: &[u8; 16],
    configured: Option<&str>,
) -> ([u8; DEVICE_NAME_LEN], usize) {
    let mut buf = [0u8; DEVICE_NAME_LEN];
    match configured {
        None => (device_name(identity_hash), DEVICE_NAME_LEN),
        Some(name) => {
            let fitted =
                leviculum_core::node_name::truncate_on_char_boundary(name, DEVICE_NAME_LEN);
            buf[..fitted.len()].copy_from_slice(fitted.as_bytes());
            (buf, fitted.len())
        }
    }
}

/// The `BLE_TX_PKT` structured event: one line per multi-fragment
/// packet handed to a link, whatever became of it (#373).
///
/// The desk log that motivated it had five relayed two-fragment packets
/// forwarded to the BLE hop and only three arrive, with **no** line on
/// our side for the other two — `BLE_TX_DROP` fires only when the
/// driver abandons a packet, so a packet whose every fragment was
/// accepted by the stack leaves no trace. This line closes that gap:
/// `sent < frags` without a drop line is now a contradiction a capture
/// can show, and `sent == frags` moves the search past this node.
///
/// Single-fragment packets and keepalives stay unlogged: they are the
/// bulk of the traffic, every line also rewrites the 2 KiB post-crash
/// tail, and the failure mode this measures needs at least two
/// fragments to exist.
///
/// Formatted here, in the host-tested crate, so the exact line is
/// pinned by a test rather than transcribed into one; the firmware's
/// two TX paths (`notify.rs` pump, `columba.rs` central write loop)
/// both render this one type. The trailing `t=` comes from the
/// firmware log formatter, never from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxPktLine {
    /// SoftDevice connection handle of the link the packet targeted.
    pub conn: u16,
    /// The whole Reticulum packet's length in bytes.
    pub len: usize,
    /// Fragments the packet was split into.
    pub frags: usize,
    /// Fragments the stack accepted; `sent < frags` is a loss on this
    /// node and is accompanied by a drop line naming the reason.
    pub sent: usize,
}

impl core::fmt::Display for TxPktLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "BLE_TX_PKT conn={} len={} frags={} sent={}",
            self.conn, self.len, self.frags, self.sent
        )
    }
}

/// The result of one `sd_ble_gatts_hvx` call, as the driver saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// The SoftDevice took the fragment into its notification queue.
    Sent,
    /// `NRF_ERROR_RESOURCES`: the per-connection HVN queue is full. The
    /// fragment was **not** queued and must be offered again.
    QueueFull,
    /// The connection is gone; nothing more will go out on it.
    Disconnected,
    /// Any other SoftDevice error, carrying its raw code so the report
    /// names it. `NRF_ERROR_DATA_SIZE` (a fragment larger than the
    /// negotiated ATT MTU allows) is the one to expect here.
    Failed(u32),
}

/// Something that happened after the driver performed an [`Action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The outcome of the [`Action::Send`] the driver just performed.
    Notify(NotifyOutcome),
    /// The SoftDevice raised `BLE_GATTS_EVT_HVN_TX_COMPLETE`: at least
    /// one queue slot is free again.
    Drained,
    /// [`Action::AwaitDrain`] hit [`DRAIN_WAIT_MS`] without a drain.
    WaitTimedOut,
}

/// Why a packet was abandoned part-way through its fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    /// Every drain wait timed out until the [`drain_wait_budget`] was
    /// spent — the queue never made room and never signalled. A single
    /// timed-out wait only re-offers the fragment (#255: one timeout
    /// must not tear the packet mid-stream).
    Stalled,
    /// The connection dropped mid-packet.
    Disconnected,
    /// [`drain_wait_budget`] exhausted.
    BudgetExhausted,
    /// A SoftDevice error other than `NRF_ERROR_RESOURCES`.
    SoftDevice(u32),
}

impl AbortReason {
    /// Stable token for the `reason=` field of the structured log event.
    /// Whitespace-free and `=`-free, as the event-log grammar requires.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AbortReason::Stalled => "stalled",
            AbortReason::Disconnected => "disconnected",
            AbortReason::BudgetExhausted => "budget",
            AbortReason::SoftDevice(_) => "sd_error",
        }
    }

    /// The raw SoftDevice code for the `code=` field, 0 when the reason
    /// did not come from a syscall.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            AbortReason::SoftDevice(code) => code,
            _ => 0,
        }
    }
}

/// What the driver must do next. Perform it exactly once, then feed the
/// resulting [`Event`] back through [`PacketTx::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Hand fragment `index` to `notify_value`.
    Send { index: usize },
    /// Wait for [`Event::Drained`], bounded by [`DRAIN_WAIT_MS`]; on
    /// expiry feed [`Event::WaitTimedOut`]. `index` is the fragment the
    /// wait is for, for the log line only.
    AwaitDrain { index: usize },
    /// Every fragment was queued, in order, exactly once.
    Done,
    /// Give up on this packet. `index` is the fragment that failed —
    /// fragments before it did go out, the ones from it on did not.
    Abort { index: usize, reason: AbortReason },
    /// The event did not apply to the outstanding action. The driver
    /// must not act on it; it still owes the event for the action it
    /// performed. Reachable only from a caller that feeds an event it
    /// was not asked for, and defined explicitly so that such a caller
    /// cannot provoke a duplicate `Send`.
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Fragment `.0` was handed to `notify_value`; its result is owed.
    Sending(usize),
    /// Fragment `.0` was refused; a drain (or a timeout) is owed.
    Waiting(usize),
    Done,
    Aborted,
}

/// Drives one packet's fragments through the SoftDevice notification
/// queue. Construct with [`PacketTx::start`], then loop on
/// [`PacketTx::step`].
#[derive(Debug)]
pub struct PacketTx {
    total: usize,
    phase: Phase,
    sent: usize,
    waits: u32,
    budget: u32,
}

impl PacketTx {
    /// Begin a packet of `fragment_count` fragments, returning the first
    /// action. A zero-fragment packet is [`Action::Done`] at once —
    /// there is nothing to send and nothing to report.
    #[must_use]
    pub fn start(fragment_count: usize) -> (Self, Action) {
        let (phase, action) = if fragment_count == 0 {
            (Phase::Done, Action::Done)
        } else {
            (Phase::Sending(0), Action::Send { index: 0 })
        };
        let tx = Self {
            total: fragment_count,
            phase,
            sent: 0,
            waits: 0,
            budget: drain_wait_budget(fragment_count),
        };
        (tx, action)
    }

    /// Feed the event produced by the last action; get the next action.
    pub fn step(&mut self, event: Event) -> Action {
        match (self.phase, event) {
            (Phase::Sending(index), Event::Notify(NotifyOutcome::Sent)) => {
                self.sent += 1;
                let next = index + 1;
                if next >= self.total {
                    self.phase = Phase::Done;
                    Action::Done
                } else {
                    self.phase = Phase::Sending(next);
                    Action::Send { index: next }
                }
            }
            (Phase::Sending(index), Event::Notify(NotifyOutcome::QueueFull)) => {
                if self.waits >= self.budget {
                    self.abort(index, AbortReason::BudgetExhausted)
                } else {
                    self.phase = Phase::Waiting(index);
                    Action::AwaitDrain { index }
                }
            }
            (Phase::Sending(index), Event::Notify(NotifyOutcome::Disconnected)) => {
                self.abort(index, AbortReason::Disconnected)
            }
            (Phase::Sending(index), Event::Notify(NotifyOutcome::Failed(code))) => {
                self.abort(index, AbortReason::SoftDevice(code))
            }
            (Phase::Waiting(index), Event::Drained) => {
                self.waits += 1;
                self.phase = Phase::Sending(index);
                Action::Send { index }
            }
            (Phase::Waiting(index), Event::WaitTimedOut) => {
                // A timed-out wait is NOT license to tear the packet
                // (#255): fragments already accepted are on the air, and
                // a peer's reassembler holds them as the head of this
                // packet. Abandoning the rest turns the *next* packet's
                // tail into the completion of this one — the Columba
                // reassembler glued a torn 186 B announce head onto the
                // following report's END fragment and rejected the
                // result as a 211/259 B announce with an invalid
                // signature. Re-offer the fragment instead: if the
                // drain edge was merely lost, the queue has room now;
                // if not, QueueFull leads back into the wait. The waits
                // budget still bounds the total, so a genuinely wedged
                // queue aborts (and the driver then resyncs the peer by
                // dropping the connection) instead of looping forever.
                self.waits += 1;
                if self.waits >= self.budget {
                    self.abort(index, AbortReason::Stalled)
                } else {
                    self.phase = Phase::Sending(index);
                    Action::Send { index }
                }
            }
            _ => Action::Nothing,
        }
    }

    fn abort(&mut self, index: usize, reason: AbortReason) -> Action {
        self.phase = Phase::Aborted;
        Action::Abort { index, reason }
    }

    /// Fragments the SoftDevice accepted so far.
    #[must_use]
    pub fn fragments_sent(&self) -> usize {
        self.sent
    }

    /// Drain waits this packet has completed, for the `waits=` field.
    #[must_use]
    pub fn drain_waits(&self) -> u32 {
        self.waits
    }

    /// The packet's drain-wait budget, i.e. [`drain_wait_budget`] of its
    /// fragment count.
    #[must_use]
    pub fn budget(&self) -> u32 {
        self.budget
    }

    /// Whether an abort left the fragment stream torn: at least one
    /// fragment of this packet was accepted before the rest was
    /// abandoned.
    ///
    /// A torn stream is worse than a lost packet. The wire protocol has
    /// no abort marker, so the peer's reassembler keeps the accepted
    /// head for its full reassembly window and completes it with the
    /// **next** packet's tail — which then parses as a packet of this
    /// one's type and fails validation (#255: a torn announce head plus
    /// a report END fragment arrived at Columba as a 211/259 B announce
    /// with an invalid signature, deterministically). The only in-band
    /// reset of the peer's per-connection reassembly state is dropping
    /// the connection; the driver must do exactly that when this is
    /// `true`.
    #[must_use]
    pub fn torn(&self) -> bool {
        matches!(self.phase, Phase::Aborted) && self.sent > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NRF_ERROR_DATA_SIZE`, the error a notification larger than the
    /// negotiated ATT MTU produces. Used as the "hard error" stand-in.
    const NRF_ERROR_DATA_SIZE: u32 = 12;

    /// A scripted stand-in for the SoftDevice's per-connection HVN
    /// queue: it accepts up to `depth` notifications, refuses the rest
    /// with `QueueFull`, and only makes room when something drains it.
    struct Sink {
        depth: usize,
        in_flight: usize,
        /// Fragment indices accepted, in the order they were accepted.
        accepted: Vec<usize>,
        /// Fail the call for this fragment index instead of queueing it.
        fail_at: Option<(usize, NotifyOutcome)>,
        /// Refuse to ever drain (models a peer that stopped listening).
        never_drains: bool,
        /// Signal a drain without actually making room (models a
        /// pathological drain storm).
        lying_drains: bool,
    }

    impl Sink {
        fn new(depth: usize) -> Self {
            Self {
                depth,
                in_flight: 0,
                accepted: Vec::new(),
                fail_at: None,
                never_drains: false,
                lying_drains: false,
            }
        }

        fn notify(&mut self, index: usize) -> NotifyOutcome {
            if let Some((at, outcome)) = self.fail_at {
                if at == index {
                    return outcome;
                }
            }
            if self.in_flight >= self.depth {
                return NotifyOutcome::QueueFull;
            }
            self.in_flight += 1;
            self.accepted.push(index);
            NotifyOutcome::Sent
        }

        /// The event the driver observes while waiting for a drain.
        fn wait(&mut self) -> Event {
            if self.lying_drains {
                return Event::Drained;
            }
            if self.never_drains || self.in_flight == 0 {
                return Event::WaitTimedOut;
            }
            self.in_flight -= 1;
            Event::Drained
        }
    }

    /// The firmware driver, in the shape `ble.rs` uses it.
    fn drive(sink: &mut Sink, fragments: usize) -> (Action, PacketTx) {
        let (mut tx, mut action) = PacketTx::start(fragments);
        for _ in 0..10_000 {
            match action {
                Action::Send { index } => {
                    let outcome = sink.notify(index);
                    action = tx.step(Event::Notify(outcome));
                }
                Action::AwaitDrain { .. } => {
                    let event = sink.wait();
                    action = tx.step(event);
                }
                terminal => return (terminal, tx),
            }
        }
        panic!("driver did not terminate");
    }

    /// Positive control for the sink itself, and the red state this
    /// batch fixes: the pre-#264 loop pushed every fragment without
    /// looking at the result, so exactly one of three reached the queue.
    #[test]
    fn control_ignoring_the_result_delivers_only_the_first_fragment() {
        let mut sink = Sink::new(1);
        for index in 0..3 {
            let _ignored = sink.notify(index);
        }
        assert_eq!(sink.accepted, vec![0], "the sink must actually refuse");
    }

    #[test]
    fn every_fragment_goes_out_in_order_exactly_once_on_a_one_deep_queue() {
        let mut sink = Sink::new(1);
        let (action, tx) = drive(&mut sink, 3);
        assert_eq!(action, Action::Done);
        assert_eq!(sink.accepted, vec![0, 1, 2]);
        assert_eq!(tx.fragments_sent(), 3);
    }

    #[test]
    fn a_queue_that_refuses_after_k_fragments_then_drains_still_delivers_all() {
        // depth 2: fragments 0 and 1 go straight in, 2 and 3 each wait.
        let mut sink = Sink::new(2);
        let (action, tx) = drive(&mut sink, 4);
        assert_eq!(action, Action::Done);
        assert_eq!(sink.accepted, vec![0, 1, 2, 3]);
        assert_eq!(tx.drain_waits(), 2);
    }

    #[test]
    fn a_queue_deep_enough_never_waits() {
        let mut sink = Sink::new(8);
        let (action, tx) = drive(&mut sink, 3);
        assert_eq!(action, Action::Done);
        assert_eq!(sink.accepted, vec![0, 1, 2]);
        assert_eq!(tx.drain_waits(), 0);
    }

    #[test]
    fn success_after_n_retries_counts_one_wait_per_refused_fragment() {
        let mut sink = Sink::new(1);
        let (action, tx) = drive(&mut sink, 5);
        assert_eq!(action, Action::Done);
        assert_eq!(sink.accepted, vec![0, 1, 2, 3, 4]);
        assert_eq!(tx.drain_waits(), 4);
    }

    #[test]
    fn a_peer_that_stops_draining_aborts_at_the_stalled_fragment() {
        let mut sink = Sink::new(1);
        sink.never_drains = true;
        let (action, tx) = drive(&mut sink, 3);
        assert_eq!(
            action,
            Action::Abort {
                index: 1,
                reason: AbortReason::Stalled
            }
        );
        assert_eq!(sink.accepted, vec![0], "fragment 1 was never queued");
        assert_eq!(tx.fragments_sent(), 1);
        // The whole budget was spent waiting before giving up: a stall
        // abort is a last resort, not a first response (#255).
        assert_eq!(tx.drain_waits(), tx.budget());
        // Fragment 0 is on the air, so the peer holds a torn head — the
        // driver must reset it by dropping the connection.
        assert!(tx.torn());
    }

    /// The #255 mechanism, minimal: a TRANSIENT drain stall (the drain
    /// arrives, but later than `DRAIN_WAIT_MS`) must not tear the packet
    /// mid-stream. Red while one `WaitTimedOut` aborted the packet: the
    /// peer's reassembler then held fragment 0 as a torn head and
    /// completed it with the next packet's END fragment — Columba logged
    /// the glue as a 211/259 B announce with an invalid signature.
    #[test]
    fn a_transient_stall_retries_and_delivers_instead_of_tearing() {
        let mut sink = Sink::new(1);
        let (mut tx, mut action) = PacketTx::start(2);
        let mut timeouts_left = 1;
        for _ in 0..100 {
            match action {
                Action::Send { index } => {
                    action = tx.step(Event::Notify(sink.notify(index)));
                }
                Action::AwaitDrain { .. } => {
                    let event = if timeouts_left > 0 {
                        timeouts_left -= 1;
                        Event::WaitTimedOut
                    } else {
                        sink.wait()
                    };
                    action = tx.step(event);
                }
                terminal => {
                    assert_eq!(
                        terminal,
                        Action::Done,
                        "one late drain must not tear the packet"
                    );
                    assert_eq!(sink.accepted, vec![0, 1]);
                    assert!(!tx.torn());
                    return;
                }
            }
        }
        panic!("driver did not terminate");
    }

    /// `torn()` is precisely "aborted after at least one accepted
    /// fragment" — the condition under which the driver must reset the
    /// peer's reassembler by dropping the connection (#255).
    #[test]
    fn torn_is_abort_after_an_accepted_fragment_and_nothing_else() {
        // Abort at fragment 0: nothing on the air, not torn.
        let (mut tx, _) = PacketTx::start(2);
        assert!(matches!(
            tx.step(Event::Notify(NotifyOutcome::Failed(NRF_ERROR_DATA_SIZE))),
            Action::Abort { index: 0, .. }
        ));
        assert!(!tx.torn());

        // Abort at fragment 1: fragment 0 accepted, torn.
        let (mut tx, _) = PacketTx::start(2);
        assert_eq!(
            tx.step(Event::Notify(NotifyOutcome::Sent)),
            Action::Send { index: 1 }
        );
        assert!(matches!(
            tx.step(Event::Notify(NotifyOutcome::Disconnected)),
            Action::Abort { index: 1, .. }
        ));
        assert!(tx.torn());

        // A completed packet is never torn.
        let (mut tx, _) = PacketTx::start(1);
        assert_eq!(tx.step(Event::Notify(NotifyOutcome::Sent)), Action::Done);
        assert!(!tx.torn());
    }

    #[test]
    fn a_hard_error_mid_packet_aborts_and_carries_the_softdevice_code() {
        let mut sink = Sink::new(8);
        sink.fail_at = Some((1, NotifyOutcome::Failed(NRF_ERROR_DATA_SIZE)));
        let (action, _tx) = drive(&mut sink, 3);
        assert_eq!(
            action,
            Action::Abort {
                index: 1,
                reason: AbortReason::SoftDevice(NRF_ERROR_DATA_SIZE)
            }
        );
        assert_eq!(sink.accepted, vec![0]);
        let Action::Abort { reason, .. } = action else {
            unreachable!()
        };
        assert_eq!(reason.as_str(), "sd_error");
        assert_eq!(reason.code(), NRF_ERROR_DATA_SIZE);
    }

    #[test]
    fn a_disconnect_mid_packet_aborts_without_a_softdevice_code() {
        let mut sink = Sink::new(8);
        sink.fail_at = Some((2, NotifyOutcome::Disconnected));
        let (action, _tx) = drive(&mut sink, 4);
        assert_eq!(
            action,
            Action::Abort {
                index: 2,
                reason: AbortReason::Disconnected
            }
        );
        assert_eq!(sink.accepted, vec![0, 1]);
        assert_eq!(AbortReason::Disconnected.as_str(), "disconnected");
        assert_eq!(AbortReason::Disconnected.code(), 0);
    }

    #[test]
    fn drain_events_that_never_make_room_exhaust_the_budget_instead_of_looping() {
        let mut sink = Sink::new(1);
        sink.lying_drains = true;
        // depth 1 with a lying drain: fragment 0 goes in, fragment 1 is
        // refused forever while the "drain" keeps firing.
        let (action, tx) = drive(&mut sink, 2);
        assert_eq!(
            action,
            Action::Abort {
                index: 1,
                reason: AbortReason::BudgetExhausted
            }
        );
        assert_eq!(tx.drain_waits(), tx.budget());
        assert_eq!(tx.budget(), drain_wait_budget(2));
    }

    #[test]
    fn an_event_the_machine_did_not_ask_for_cannot_provoke_a_second_send() {
        // Phase Sending(0): a stray drain must not re-issue Send{0}.
        let (mut tx, first) = PacketTx::start(2);
        assert_eq!(first, Action::Send { index: 0 });
        assert_eq!(tx.step(Event::Drained), Action::Nothing);
        assert_eq!(tx.step(Event::WaitTimedOut), Action::Nothing);
        // The outstanding result still advances the packet normally.
        assert_eq!(
            tx.step(Event::Notify(NotifyOutcome::Sent)),
            Action::Send { index: 1 }
        );

        // Phase Waiting(1): a stray notify result must not advance it.
        assert_eq!(
            tx.step(Event::Notify(NotifyOutcome::QueueFull)),
            Action::AwaitDrain { index: 1 }
        );
        assert_eq!(tx.step(Event::Notify(NotifyOutcome::Sent)), Action::Nothing);
        assert_eq!(tx.fragments_sent(), 1);
    }

    #[test]
    fn a_terminated_packet_stays_terminated() {
        let (mut tx, _) = PacketTx::start(1);
        assert_eq!(tx.step(Event::Notify(NotifyOutcome::Sent)), Action::Done);
        assert_eq!(tx.step(Event::Notify(NotifyOutcome::Sent)), Action::Nothing);
        assert_eq!(tx.step(Event::Drained), Action::Nothing);

        let (mut tx, _) = PacketTx::start(1);
        assert!(matches!(
            tx.step(Event::Notify(NotifyOutcome::Disconnected)),
            Action::Abort { .. }
        ));
        assert_eq!(tx.step(Event::Drained), Action::Nothing);
        assert_eq!(tx.step(Event::Notify(NotifyOutcome::Sent)), Action::Nothing);
    }

    #[test]
    fn an_empty_packet_is_done_immediately() {
        let (tx, action) = PacketTx::start(0);
        assert_eq!(action, Action::Done);
        assert_eq!(tx.fragments_sent(), 0);
    }

    #[test]
    fn the_budget_grows_with_the_fragment_count_and_never_overflows() {
        assert_eq!(drain_wait_budget(0), 4);
        assert_eq!(drain_wait_budget(1), 6);
        assert_eq!(drain_wait_budget(3), 10);
        assert!(drain_wait_budget(usize::MAX) > 0);
    }

    /// The #373 first hypothesis, transmitter side: two multi-fragment
    /// packets queued back-to-back to ONE link must reach the stack as
    /// START..END, START..END — never interleaved. The firmware holds
    /// this structurally: each link has exactly one outbound future
    /// (`columba.rs`), which runs one `notify_fragments` (one
    /// [`PacketTx`]) to a terminal action before taking the next packet
    /// from the link's queue. This test is that loop shape against a
    /// refusing, draining queue; the control below shows the harness
    /// would catch the interleave if the shape were broken.
    #[test]
    fn back_to_back_packets_on_one_link_never_interleave_their_fragments() {
        // (packet, fragment) pairs in stack-acceptance order.
        let mut stream: Vec<(char, usize)> = Vec::new();
        let mut in_flight = 0usize; // one-deep HVN queue, drains on wait
        for id in ['A', 'B'] {
            // One notify_fragments call: PacketTx runs to terminal
            // before the loop takes the next packet.
            let (mut tx, mut action) = PacketTx::start(2);
            loop {
                match action {
                    Action::Send { index } => {
                        let outcome = if in_flight >= 1 {
                            NotifyOutcome::QueueFull
                        } else {
                            in_flight += 1;
                            stream.push((id, index));
                            NotifyOutcome::Sent
                        };
                        action = tx.step(Event::Notify(outcome));
                    }
                    Action::AwaitDrain { .. } => {
                        in_flight -= 1;
                        action = tx.step(Event::Drained);
                    }
                    Action::Done => break,
                    other => panic!("unexpected terminal {:?}", other),
                }
            }
        }
        assert_eq!(
            stream,
            vec![('A', 0), ('A', 1), ('B', 0), ('B', 1)],
            "START..END, START..END, in order"
        );
    }

    /// Positive control for the test above: the serialisation is a
    /// property of the one-pump-per-link loop, NOT of [`PacketTx`] —
    /// two machines advanced concurrently against the same queue DO
    /// interleave, and the stream detector sees it. This is the exact
    /// shape a second writer to the same link would produce, which is
    /// why the fan-out queue is a link's only packet source.
    #[test]
    fn control_two_concurrent_machines_do_interleave_and_the_harness_sees_it() {
        let mut stream: Vec<(char, usize)> = Vec::new();
        let mut in_flight = 0usize; // two-deep queue, drains on refusal
        let (mut tx_a, mut act_a) = PacketTx::start(2);
        let (mut tx_b, mut act_b) = PacketTx::start(2);
        // Alternate the two machines, as two tasks racing one link would.
        for _ in 0..16 {
            for (id, tx, action) in [('A', &mut tx_a, &mut act_a), ('B', &mut tx_b, &mut act_b)] {
                match *action {
                    Action::Send { index } => {
                        let outcome = if in_flight >= 2 {
                            in_flight -= 1; // a drain happens between turns
                            NotifyOutcome::QueueFull
                        } else {
                            in_flight += 1;
                            stream.push((id, index));
                            NotifyOutcome::Sent
                        };
                        *action = tx.step(Event::Notify(outcome));
                    }
                    Action::AwaitDrain { .. } => {
                        *action = tx.step(Event::Drained);
                    }
                    _ => {}
                }
            }
        }
        let a_span: Vec<usize> = stream
            .iter()
            .enumerate()
            .filter(|(_, (id, _))| *id == 'A')
            .map(|(i, _)| i)
            .collect();
        let b_span: Vec<usize> = stream
            .iter()
            .enumerate()
            .filter(|(_, (id, _))| *id == 'B')
            .map(|(i, _)| i)
            .collect();
        assert!(
            b_span.first() < a_span.last() && a_span.first() < b_span.last(),
            "the control must actually interleave, stream: {:?}",
            stream
        );
    }

    /// The `BLE_TX_PKT` line, verbatim (#373): what a healthy relayed
    /// two-fragment packet writes, and what a partial hand-over writes.
    /// The firmware appends ` t=<ms>`; everything before it is this.
    #[test]
    fn the_tx_pkt_line_is_the_documented_grammar_verbatim() {
        let line = TxPktLine {
            conn: 1,
            len: 291,
            frags: 2,
            sent: 2,
        };
        assert_eq!(line.to_string(), "BLE_TX_PKT conn=1 len=291 frags=2 sent=2");

        let torn = TxPktLine {
            conn: 1,
            len: 291,
            frags: 2,
            sent: 1,
        };
        assert_eq!(torn.to_string(), "BLE_TX_PKT conn=1 len=291 frags=2 sent=1");
    }

    #[test]
    fn the_device_name_is_ln_dash_plus_the_leading_hex_of_the_hash() {
        let hash = [
            0xa1, 0xb2, 0xc3, 0xd4, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66,
            0x55, 0x44,
        ];
        let name = device_name(&hash);
        assert_eq!(&name, b"LN-a1b2c3d4");
        assert_eq!(name.len(), DEVICE_NAME_LEN);
    }

    #[test]
    fn the_device_name_fits_a_name_only_scan_response() {
        // 31-byte legacy scan-response AD budget, minus the 2-byte
        // length + AD-type overhead of a Complete Local Name structure.
        // Constant on both sides, so a const block: it fails the build.
        const { assert!(DEVICE_NAME_LEN <= 31 - 2) };
    }

    #[test]
    fn the_device_name_fits_the_default_gap_attribute_table() {
        // Past BLE_GAP_DEVNAME_DEFAULT_LEN the `gatts_attr_tab_size`
        // config has to grow with it, and the only symptom of forgetting
        // is a SoftDevice that refuses the name config at enable — on a
        // board, before USB comes up. Constant on both sides, so a const
        // block: the build fails rather than one test run.
        const { assert!(DEVICE_NAME_LEN <= GAP_DEVNAME_DEFAULT_LEN) };
    }

    #[test]
    fn the_device_name_is_ascii_for_every_hash_byte() {
        for byte in 0..=255u8 {
            let name = device_name(&[byte; 16]);
            assert!(name.iter().all(|c| c.is_ascii_graphic()));
            assert!(core::str::from_utf8(&name).is_ok());
        }
    }

    #[test]
    fn an_unnamed_board_advertises_its_derived_name() {
        // The property that makes this feature invisible to a board
        // nobody names: the derived default must stay exactly what it is
        // today, hex and all.
        let hash = [
            0xa1, 0xb2, 0xc3, 0xd4, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66,
            0x55, 0x44,
        ];
        let (name, len) = gap_name(&hash, None);
        assert_eq!(&name[..len], b"LN-a1b2c3d4");
        assert_eq!(len, DEVICE_NAME_LEN);
    }

    #[test]
    fn a_short_name_is_advertised_whole() {
        let (name, len) = gap_name(&[0u8; 16], Some("Balkon"));
        assert_eq!(&name[..len], b"Balkon");
        assert_eq!(len, 6);
    }

    #[test]
    fn a_long_name_is_shortened_visibly_rather_than_replaced() {
        // The mesh keeps the whole name; BLE gets a prefix an operator
        // can recognise as the same board, not a different string.
        let (name, len) = gap_name(&[0u8; 16], Some("Balkon-Nord-Solarknoten"));
        assert_eq!(core::str::from_utf8(&name[..len]).unwrap(), "Balkon-Nord");
        assert_eq!(len, DEVICE_NAME_LEN);
    }

    #[test]
    fn a_multibyte_name_is_cut_on_a_codepoint_boundary() {
        // `Küchenschrank` is 14 bytes; a byte-wise cut at 11 would land
        // mid-character only if the multibyte one straddled the bound, so
        // the interesting fixture puts one there deliberately.
        let (name, len) = gap_name(&[0u8; 16], Some("Kücheeeeeeüberall"));
        let text = core::str::from_utf8(&name[..len]).expect("still valid UTF-8");
        assert!(len <= DEVICE_NAME_LEN);
        assert_eq!(text, "Kücheeeeee");
        // The rule, stated as a property over every prefix length: no
        // truncation may produce something that is not a &str.
        for max in 0..=20 {
            let cut =
                leviculum_core::node_name::truncate_on_char_boundary("Kücheeeeeeüberall", max);
            assert!(cut.len() <= max, "{max}");
            assert!("Kücheeeeeeüberall".starts_with(cut), "{max}");
        }
    }

    #[test]
    fn a_shortened_name_never_ends_on_a_space() {
        // A trailing space is invisible: the operator would compare the
        // two names by eye, find them equal, and be wrong.
        let (name, len) = gap_name(&[0u8; 16], Some("Hallo Welt Nord"));
        assert_eq!(core::str::from_utf8(&name[..len]).unwrap(), "Hallo Welt");
        assert_eq!(
            leviculum_core::node_name::truncate_on_char_boundary("ab   cd", 5),
            "ab"
        );
    }

    #[test]
    fn a_name_that_exactly_fits_is_not_shortened() {
        let exact = "0123456789A";
        assert_eq!(exact.len(), DEVICE_NAME_LEN);
        let (name, len) = gap_name(&[0u8; 16], Some(exact));
        assert_eq!(core::str::from_utf8(&name[..len]).unwrap(), exact);
        assert_eq!(len, DEVICE_NAME_LEN);
    }
}
