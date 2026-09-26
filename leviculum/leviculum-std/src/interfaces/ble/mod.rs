//! Columba BLE interface (`ble-reticulum` protocol v2.2 + v0.3.0) over
//! BlueZ — the PC side of the mesh the LNode firmware and Columba phones
//! already speak.
//!
//! One configured `BLEInterface` section is **one Reticulum interface and
//! one broadcast domain**: every outbound packet fans out to all live BLE
//! links, every live link's inbound packets feed the same interface. The
//! interface runs both GATT roles at once — it advertises + serves the
//! Columba GATT layout (peripheral) and scans + dials peers the shared
//! connection-direction rule tells it to (central).
//!
//! # Layering
//!
//! - [`links`] is the pure half: admission, per-link framing state,
//!   keepalive/expiry policy, fan-out planning. Host-tested, no BlueZ.
//! - [`bluez`] is the carrier binding: bluer (the official BlueZ Rust
//!   binding, over D-Bus) performs advertising, the GATT server, scanning
//!   and the central-role connections, and reports everything as
//!   [`Ev`] events into the orchestrator task below.
//! - The orchestrator owns the [`links::LinkTable`] and the two mpsc
//!   channels of the [`InterfaceHandle`] contract; it is the only writer
//!   of interface state, so there is no lock.
//!
//! Wire logic is reused, not restated: fragmentation from
//! `leviculum_core::framing::ble`, advertisement parsing and the
//! connection decision from `leviculum-ble-tx` (the firmware's own
//! host-tested crate), both driven from [`links`].

pub(crate) mod bluez;
pub(crate) mod links;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use bluer::gatt::local::CharacteristicNotifier;
use bluer::Address;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::{
    IncomingPacket, InterfaceCounters, InterfaceHandle, InterfaceInfo, OutgoingPacket, PeerEvent,
    ReadySignal,
};
use leviculum_core::traits::{InterfaceKind, InterfaceMode};
use leviculum_core::transport::InterfaceId;
use links::{
    Addr, Admission, DialQueue, Displaced, IdentityHash, Inbound, LinkTable, Role, ScanScheduler,
};

/// Outbound channel depth, matching the other interfaces' default.
const BLE_BUFFER_SIZE: usize = 256;

/// Per-central-link packet-unit channel depth (one message = one
/// packet's fragments, #376). A full channel drops the packet on that
/// link (counted + logged), it never blocks the domain.
const LINK_QUEUE_DEPTH: usize = 32;

/// Reconnect/backoff after a failed or ended central session, the
/// firmware's `SESSION_BACKOFF`.
const SESSION_BACKOFF_MS: u64 = 5_000;

/// How long an address that presented a duplicate or our own identity is
/// not redialled, the firmware's `DEAD_END_TTL`.
const DUPLICATE_ADDR_TTL_MS: u64 = 120_000;

/// Delay before retrying BlueZ session/adapter setup after a failure.
const ADAPTER_RETRY: Duration = Duration::from_secs(10);

/// The reference's `BITRATE_GUESS` for this medium (`ble-reticulum@07d94130` `BLEInterface.py`, `BITRATE_GUESS`).
const BLE_BITRATE_GUESS: u32 = 700_000;

/// Everything the builder resolves from the config section.
#[derive(Debug, Clone)]
pub(crate) struct BleOptions {
    /// BlueZ adapter name (`hci0`); `None` = the default adapter.
    pub adapter: Option<String>,
    /// Simultaneous link cap, both roles counted together.
    pub max_connections: usize,
    /// Sightings weaker than this are not dialled (reference `min_rssi`).
    pub min_rssi: i16,
    /// Pause between scan windows (reference `discovery_interval`).
    pub discovery_interval: Duration,
    /// Run the scanning + dialling half.
    pub enable_central: bool,
    /// Run the advertising + GATT-server half.
    pub enable_peripheral: bool,
}

impl Default for BleOptions {
    fn default() -> Self {
        Self {
            adapter: None,
            max_connections: links::DEFAULT_MAX_LINKS,
            min_rssi: -85,
            discovery_interval: Duration::from_secs(5),
            enable_central: true,
            enable_peripheral: true,
        }
    }
}

/// Everything the carrier tasks report back to the orchestrator.
pub(crate) enum Ev {
    /// A central wrote to our RX characteristic.
    PeriphWrite {
        addr: Address,
        mtu: usize,
        data: Vec<u8>,
    },
    /// A central subscribed to our TX characteristic.
    PeriphNotify(CharacteristicNotifier),
    /// One scanner sighting of a device offering (or not) the service.
    Scan {
        addr: Address,
        rssi: Option<i16>,
        offers_service: bool,
        /// Payload of the CID 0xFFFF manufacturer record, if present.
        record: Option<Vec<u8>>,
    },
    /// A central-role connection read the peer's identity and asks to be
    /// admitted before it handshakes. `true` on `ack` = proceed.
    ///
    /// `frames` carries one PACKET per message — all its fragments as a
    /// unit — so the link task can pace packet-to-packet (#376) and a
    /// full queue costs whole packets, never a torn fragment stream. A
    /// keepalive rides as a single sub-header-size frame.
    CentralIdentity {
        addr: Address,
        identity: IdentityHash,
        mtu: usize,
        frames: mpsc::Sender<Vec<Vec<u8>>>,
        ack: oneshot::Sender<bool>,
    },
    /// A notification arrived on a central-role link.
    CentralFrame { addr: Address, data: Vec<u8> },
    /// A central-role task ended (connect failure, session end, or the
    /// admission said no). Always the task's last event.
    CentralGone { addr: Address },
}

/// Spawn the Columba BLE interface. Never fails at build time: BlueZ
/// availability is a runtime concern (the adapter can appear, power up,
/// or restart while lnsd runs), handled by the task's retry loop.
pub(crate) fn spawn_ble_interface(
    id: InterfaceId,
    name: String,
    opts: BleOptions,
    identity_hash: IdentityHash,
    peer_event_tx: mpsc::Sender<(InterfaceId, PeerEvent)>,
) -> InterfaceHandle {
    let (incoming_tx, incoming_rx) = mpsc::channel(BLE_BUFFER_SIZE);
    let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingPacket>(BLE_BUFFER_SIZE);
    let counters = Arc::new(InterfaceCounters::new());
    let ready = ReadySignal::new();

    let info = InterfaceInfo {
        id,
        name: name.clone(),
        // Reassembled packets carry the base protocol MTU; no link-MTU
        // upgrade is signalled (the reference's HW_MTU is the same 500).
        hw_mtu: None,
        is_local_client: false,
        bitrate: Some(BLE_BITRATE_GUESS),
        tx_jitter_max_ms: None,
        ifac: None,
        mode: InterfaceMode::default(),
        kind: InterfaceKind::Ble,
        ingress_control: None,
    };

    let task = BleTask {
        id,
        name,
        opts,
        identity_hash,
        incoming_tx,
        peer_event_tx,
        counters: Arc::clone(&counters),
        ready: Arc::clone(&ready),
    };
    tokio::spawn(task.run(outgoing_rx));

    InterfaceHandle {
        info,
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        ready,
    }
}

struct BleTask {
    id: InterfaceId,
    name: String,
    opts: BleOptions,
    identity_hash: IdentityHash,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    /// Peer transitions toward the driver loop (Codeberg #365): `Lost`
    /// when an identity's LAST link on this broadcast domain died (the
    /// loop culls the paths via that peer), `Up` when an identity
    /// gained its FIRST link (the loop counts it into the peer mirror
    /// and runs the opt-in peer-up pull).
    /// One ordered channel for both — see [`PeerEvent`].
    peer_event_tx: mpsc::Sender<(InterfaceId, PeerEvent)>,
    counters: Arc<InterfaceCounters>,
    ready: Arc<ReadySignal>,
}

/// Orchestrator-side state for one central-role link's TX pipe. One
/// message = one packet's fragments (see [`Ev::CentralIdentity`]).
struct CentralPipe {
    frames: mpsc::Sender<Vec<Vec<u8>>>,
}

impl BleTask {
    async fn run(self, mut outgoing_rx: mpsc::Receiver<OutgoingPacket>) {
        loop {
            match self.session(&mut outgoing_rx).await {
                Ok(()) => return, // interface detached
                Err(e) => {
                    tracing::warn!(
                        "BLE {}: BlueZ session failed ({e}); retrying in {:?}",
                        self.name,
                        ADAPTER_RETRY
                    );
                    tokio::time::sleep(ADAPTER_RETRY).await;
                }
            }
        }
    }

    /// One BlueZ session: set up both roles, then run the event loop
    /// until the daemon drops the interface (`Ok`) or the adapter goes
    /// away (`Err` → retried by [`run`]).
    async fn session(&self, outgoing_rx: &mut mpsc::Receiver<OutgoingPacket>) -> bluer::Result<()> {
        let session = bluer::Session::new().await?;
        let adapter = match &self.opts.adapter {
            Some(name) => session.adapter(name)?,
            None => session.default_adapter().await?,
        };
        adapter.set_powered(true).await?;
        let local_addr: Addr = adapter.address().await?.0;

        let (ev_tx, mut ev_rx) = mpsc::channel::<Ev>(64);

        // Peripheral half. The GATT application lives for the session —
        // live peripheral links keep using it — while the advertisement
        // is gated on table occupancy below (`should_advertise`, the
        // firmware's ADV_LOCK policy at ead0bce): deregistered when the
        // table fills, re-registered when a slot frees. Both handles
        // deregister on drop.
        let _app = if self.opts.enable_peripheral {
            Some(bluez::serve_gatt(&adapter, self.identity_hash, ev_tx.clone()).await?)
        } else {
            None
        };
        let mut adv = if self.opts.enable_peripheral {
            Some(bluez::register_advertisement(&adapter, self.identity_hash, &self.name).await?)
        } else {
            None
        };

        // Central half: windowed scanning.
        let scan_guard = if self.opts.enable_central {
            let scan = bluez::ScanTask {
                adapter: adapter.clone(),
                ev_tx: ev_tx.clone(),
                interval: self.opts.discovery_interval,
                iface: self.name.clone(),
            };
            Some(tokio::spawn(scan.run()))
        } else {
            None
        };
        // Abort the scanner with the session, not at some later drop.
        let _scan_abort = scan_guard.map(AbortOnDrop);

        tracing::info!(
            "BLE {}: up on {} addr {} name LN-… (peripheral={}, central={}, max_links={})",
            self.name,
            adapter.name(),
            Address(local_addr),
            self.opts.enable_peripheral,
            self.opts.enable_central,
            self.opts.max_connections,
        );
        self.ready.signal_ready();

        let start = Instant::now();
        let now_ms = |i: Instant| i.duration_since(start).as_millis() as u64;

        let mut table = LinkTable::new(self.identity_hash, self.opts.max_connections);
        // The peripheral notify pipe's inter-packet gap (#376). One
        // pacer for the pipe IS per-link pacing here: a notification
        // reaches every subscribed central at once, so all peripheral
        // links share one schedule. The central links pace themselves,
        // each in its own task (`bluez::CentralTask::session_loop`).
        let mut notify_pacer = links::LinkPacer::new();
        // The firmware central task's fallback clock and collection
        // window (#375 part 2, item 3), driven from the event loop:
        // sightings feed it, ticks close its windows.
        let mut scheduler = ScanScheduler::new(0);
        let mut notifiers: Vec<CharacteristicNotifier> = Vec::new();
        let mut central_pipes: HashMap<Addr, CentralPipe> = HashMap::new();
        // One connection setup in flight per adapter, jittered (#49
        // part 3). Seeded from entropy; tests seed the queue directly.
        let mut dial_queue = DialQueue::new(rand_core::RngCore::next_u64(&mut rand_core::OsRng));
        let mut backoff_until: HashMap<Addr, u64> = HashMap::new();
        // BLE_SCAN_DECISION is logged once per (address, decision) change,
        // not per sighting — a waiting peer advertises several times a
        // second (same rule as the firmware).
        let mut last_decision: HashMap<Addr, links::ScanDecision> = HashMap::new();

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // The head dial's start time, re-read each pass: any event
            // may arm, consume or postpone it. `None` parks the arm.
            let dial_at = dial_queue
                .next_deadline_ms()
                .map(|ms| start + Duration::from_millis(ms));
            tokio::select! {
                outgoing = outgoing_rx.recv() => {
                    let Some(packet) = outgoing else {
                        // Daemon dropped the handle: interface detached.
                        return Ok(());
                    };
                    self.send_packet(
                        &table,
                        &central_pipes,
                        &mut notifiers,
                        &mut notify_pacer,
                        start,
                        &packet.data,
                        packet.peer,
                    )
                    .await;
                }
                ev = ev_rx.recv() => {
                    let Some(ev) = ev else { return Ok(()) };
                    self.handle_event(
                        ev,
                        &adapter,
                        &local_addr,
                        &mut table,
                        &mut scheduler,
                        &mut notifiers,
                        &mut central_pipes,
                        &mut dial_queue,
                        &mut backoff_until,
                        &mut last_decision,
                        &ev_tx,
                        now_ms(Instant::now()),
                    )
                    .await;
                    self.reconcile_advertising(&adapter, &table, &mut adv).await;
                }
                _ = async {
                    match dial_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.pump_dials(
                        &mut dial_queue,
                        &table,
                        &adapter,
                        &backoff_until,
                        &ev_tx,
                        now_ms(Instant::now()),
                    );
                }
                _ = tick.tick() => {
                    let now = now_ms(Instant::now());
                    notifiers.retain(|n| !n.is_stopped());
                    let ka = table.keepalives_due(now);
                    if ka.notify {
                        send_via_notifiers(
                            &mut notifiers,
                            vec![leviculum_core::framing::ble::KEEPALIVE_BYTE],
                        )
                        .await;
                    }
                    for addr in ka.central {
                        if let Some(pipe) = central_pipes.get(&addr) {
                            let _ = pipe
                                .frames
                                .try_send(vec![vec![leviculum_core::framing::ble::KEEPALIVE_BYTE]]);
                        }
                    }
                    let expired = table.expire(now);
                    if !(expired.links.is_empty() && expired.pending.is_empty()) {
                        // Connections ended: a fresh strict phase, as
                        // the firmware resets its clock at teardown.
                        scheduler.note_reset(now);
                    }
                    for (identity, addr, role) in expired.links {
                        self.log_link_down(&identity, role, "timeout");
                        central_pipes.remove(&addr);
                        disconnect_quietly(&adapter, addr).await;
                        backoff_until.insert(addr, now + SESSION_BACKOFF_MS);
                        self.report_peer_lost(&table, identity).await;
                    }
                    for addr in expired.pending {
                        tracing::debug!(
                            "BLE {}: disconnecting {} — no identity handshake within timeout",
                            self.name, Address(addr)
                        );
                        disconnect_quietly(&adapter, addr).await;
                    }
                    backoff_until.retain(|_, until| *until > now);
                    // A collection window whose bound passed without a
                    // further sighting closes on the tick.
                    self.dial_window_choice(
                        &mut scheduler,
                        &table,
                        &adapter,
                        &mut dial_queue,
                        &backoff_until,
                        &ev_tx,
                        now,
                    );
                    // Expiry can free a slot, and a failed re-register
                    // gets its retry here.
                    self.reconcile_advertising(&adapter, &table, &mut adv).await;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_event(
        &self,
        ev: Ev,
        adapter: &bluer::Adapter,
        local_addr: &Addr,
        table: &mut LinkTable,
        scheduler: &mut ScanScheduler,
        notifiers: &mut Vec<CharacteristicNotifier>,
        central_pipes: &mut HashMap<Addr, CentralPipe>,
        dial_queue: &mut DialQueue,
        backoff_until: &mut HashMap<Addr, u64>,
        last_decision: &mut HashMap<Addr, links::ScanDecision>,
        ev_tx: &mpsc::Sender<Ev>,
        now: u64,
    ) {
        match ev {
            Ev::PeriphNotify(notifier) => {
                notifiers.push(notifier);
            }
            Ev::PeriphWrite { addr, mtu, data } => {
                self.counters
                    .rx_bytes
                    .fetch_add(data.len() as usize, Ordering::Relaxed);
                match table.peripheral_frame(addr.0, mtu, &data, now) {
                    Inbound::Packet(packet) => self.deliver(packet).await,
                    Inbound::HandshakeComplete {
                        identity,
                        displaced,
                    } => {
                        // A connection event in either role restarts
                        // the strict phase (#375 §0).
                        scheduler.note_reset(now);
                        let first_link = displaced.is_none();
                        if let Some(old) = displaced {
                            self.log_link_dup_displace(&identity, &addr.0, Role::Peripheral, &old);
                            self.log_link_down(&old.identity, old.role, "displaced");
                            central_pipes.remove(&old.addr);
                            disconnect_quietly(adapter, old.addr).await;
                        }
                        self.log_link_up(&identity, &addr.0, Role::Peripheral, mtu);
                        if first_link {
                            self.report_peer_up(identity).await;
                        }
                    }
                    Inbound::HandshakeRejected(admission) => {
                        self.log_rejection(admission, Role::Peripheral, &data, &addr.0);
                        backoff_until.insert(addr.0, now + DUPLICATE_ADDR_TTL_MS);
                        disconnect_quietly(adapter, addr.0).await;
                    }
                    Inbound::Error => {
                        tracing::debug!("BLE {}: bad fragment from {} dropped", self.name, addr);
                    }
                    Inbound::Keepalive | Inbound::NeedMore | Inbound::NotHandshaked => {}
                }
                self.log_abandon_reports(table);
            }
            Ev::Scan {
                addr,
                rssi,
                offers_service,
                record,
            } => {
                // A sighting with no RSSI is a BlueZ cache entry, not a
                // device on the air right now.
                let Some(rssi) = rssi else { return };
                // The fallback clock's busy input (#375 part 3, the
                // eager spec): only a dial of ours in flight or queued
                // — either way its outcome is about to reset the clock
                // — or a handshake still pending holds the clock at
                // zero. A handshaked link does not — its address is
                // kept out of the window by knows_addr below, and the
                // clock must keep running so a linked-but-losing-the-
                // sort board can still fallback-dial a third party.
                let busy = table.has_pending_handshakes() || !dial_queue.is_idle();
                let (mode, announce) = scheduler.mode(busy, now);
                if let Some(after_ms) = announce {
                    tracing::info!(
                        event = "BLE_SCAN_FALLBACK",
                        iface = %self.name,
                        after_ms = after_ms,
                    );
                }
                let Some(decision) = links::decide_from_scan(
                    local_addr,
                    &addr.0,
                    offers_service,
                    record.as_deref(),
                    mode,
                ) else {
                    return;
                };
                if last_decision.insert(addr.0, decision) != Some(decision) {
                    tracing::info!(
                        event = "BLE_SCAN_DECISION",
                        addr = %hex12(&addr.0),
                        caps = %format_args!("{:#04x}", decision.caps),
                        caps_record = u8::from(decision.caps_record),
                        initiate = u8::from(decision.decision.initiate()),
                        rule = decision.decision.as_str(),
                    );
                }
                if !decision.decision.initiate()
                    || rssi < self.opts.min_rssi
                    || table.is_full()
                    || table.knows_addr(&addr.0)
                    || dial_queue.knows(&addr.0)
                    || backoff_until.get(&addr.0).is_some_and(|until| *until > now)
                {
                    return;
                }
                // Eligible: into the collection window instead of an
                // immediate dial — the firmware's window, the same
                // CandidateTable, the same lowest-eligible choice.
                scheduler.offer(addr.0, decision.decision, now);
                self.dial_window_choice(
                    scheduler,
                    table,
                    adapter,
                    dial_queue,
                    backoff_until,
                    ev_tx,
                    now,
                );
            }
            Ev::CentralIdentity {
                addr,
                identity,
                mtu,
                frames,
                ack,
            } => {
                let (admission, displaced) = table.admit(identity, addr.0, Role::Central, mtu, now);
                match admission {
                    Admission::Accept => {
                        scheduler.note_reset(now);
                        // The dial landed: the handshake completing ends
                        // the setup, so the queue's one-in-flight slot
                        // frees here (the eager spec's clock keeps
                        // running while links are live). CentralGone
                        // still fires at the session's end; its release
                        // is then a no-op.
                        dial_queue.release(&addr.0, now);
                        let first_link = displaced.is_none();
                        if let Some(old) = displaced {
                            self.log_link_dup_displace(&identity, &addr.0, Role::Central, &old);
                            self.log_link_down(&old.identity, old.role, "displaced");
                            central_pipes.remove(&old.addr);
                            disconnect_quietly(adapter, old.addr).await;
                        }
                        central_pipes.insert(addr.0, CentralPipe { frames });
                        self.log_link_up(&identity, &addr.0, Role::Central, mtu);
                        if first_link {
                            self.report_peer_up(identity).await;
                        }
                        let _ = ack.send(true);
                    }
                    other => {
                        self.log_rejection(other, Role::Central, &identity, &addr.0);
                        backoff_until.insert(
                            addr.0,
                            now + match other {
                                Admission::RejectFull => SESSION_BACKOFF_MS,
                                _ => DUPLICATE_ADDR_TTL_MS,
                            },
                        );
                        let _ = ack.send(false);
                    }
                }
            }
            Ev::CentralFrame { addr, data } => {
                self.counters
                    .rx_bytes
                    .fetch_add(data.len() as usize, Ordering::Relaxed);
                match table.central_frame(addr.0, &data, now) {
                    Inbound::Packet(packet) => self.deliver(packet).await,
                    Inbound::Error => {
                        tracing::debug!("BLE {}: bad fragment from {} dropped", self.name, addr);
                    }
                    _ => {}
                }
                self.log_abandon_reports(table);
            }
            Ev::CentralGone { addr } => {
                // A dial or link ended either way: fresh strict phase,
                // as the firmware resets its clock at teardown. A setup
                // that timed out or failed frees the queue's slot here
                // (a completed one already freed it at admission).
                scheduler.note_reset(now);
                dial_queue.release(&addr.0, now);
                central_pipes.remove(&addr.0);
                if let Some((identity, _, role)) = table.remove_by_addr(&addr.0) {
                    self.log_link_down(&identity, role, "disconnected");
                    self.report_peer_lost(table, identity).await;
                }
                let until = backoff_until.entry(addr.0).or_insert(0);
                *until = (*until).max(now + SESSION_BACKOFF_MS);
            }
        }
    }

    /// Close the scheduler's collection window if its bound has passed
    /// and defer its choice into the dial queue — the lowest eligible
    /// address, strict verdicts first, elected by the same
    /// [`CandidateTable`] the firmware and the #375 simulation use. Logs
    /// `BLE_SCAN_WINDOW` once per window that ends in a dial, like the
    /// firmware, and `BLE_DIAL_QUEUE` for the deferred dial (#49 part 3:
    /// every dial defers — at least its pre-connect jitter, plus any
    /// setup already in flight).
    ///
    /// The eligibility filters ran when each candidate was collected;
    /// they run again here because the window is seconds long and the
    /// world moves — the chosen peer may have connected to us in the
    /// meantime, the table may have filled, a failure may have imposed
    /// a backoff. A window whose choice fails the re-check simply ends
    /// without a dial; the peer's next advertisement opens a new one.
    #[allow(clippy::too_many_arguments)]
    fn dial_window_choice(
        &self,
        scheduler: &mut ScanScheduler,
        table: &LinkTable,
        adapter: &bluer::Adapter,
        dial_queue: &mut DialQueue,
        backoff_until: &HashMap<Addr, u64>,
        ev_tx: &mpsc::Sender<Ev>,
        now: u64,
    ) {
        if let Some((addr, decision, seen)) = scheduler.poll(now) {
            if !(table.is_full()
                || table.knows_addr(&addr)
                || dial_queue.knows(&addr)
                || backoff_until.get(&addr).is_some_and(|until| *until > now))
            {
                tracing::info!(
                    event = "BLE_SCAN_WINDOW",
                    iface = %self.name,
                    seen = seen,
                    chosen = %hex12(&addr),
                    rule = decision.as_str(),
                );
                if let Some(queued) = dial_queue.enqueue(addr, decision, now) {
                    tracing::info!(
                        event = "BLE_DIAL_QUEUE",
                        iface = %self.name,
                        depth = queued.depth,
                        wait_ms = queued.wait_ms,
                    );
                }
            }
        }
        self.pump_dials(dial_queue, table, adapter, backoff_until, ev_tx, now);
    }

    /// Start the dial the queue hands out, if any: one connection setup
    /// in flight per adapter (#49 part 3). A popped dial whose
    /// eligibility re-check fails — the world moved while it waited out
    /// its jitter or a setup ahead of it — is dropped, its release
    /// arming the next queued dial.
    fn pump_dials(
        &self,
        dial_queue: &mut DialQueue,
        table: &LinkTable,
        adapter: &bluer::Adapter,
        backoff_until: &HashMap<Addr, u64>,
        ev_tx: &mpsc::Sender<Ev>,
        now: u64,
    ) {
        while let Some((addr, _decision)) = dial_queue.pop_ready(now) {
            if table.is_full()
                || table.knows_addr(&addr)
                || backoff_until.get(&addr).is_some_and(|until| *until > now)
            {
                tracing::debug!(
                    "BLE {}: queued dial to {} no longer eligible, dropped",
                    self.name,
                    Address(addr)
                );
                dial_queue.release(&addr, now);
                continue;
            }
            let central = bluez::CentralTask {
                adapter: adapter.clone(),
                addr: Address(addr),
                own_identity: self.identity_hash,
                ev_tx: ev_tx.clone(),
                iface: self.name.clone(),
            };
            tokio::spawn(central.run());
        }
    }

    /// Apply the table's advertising verdict (#49 item 1, the firmware's
    /// ADV_LOCK policy): a full table takes the advertisement off the
    /// air — dropping the handle deregisters it from BlueZ — and a freed
    /// slot puts it back. The GATT application is untouched either way;
    /// live peripheral sessions keep running while dark, exactly as the
    /// firmware keeps serving its sessions when nothing advertises.
    /// Called after every event and on every tick, so a failed
    /// re-registration is retried within a second.
    async fn reconcile_advertising(
        &self,
        adapter: &bluer::Adapter,
        table: &LinkTable,
        adv: &mut Option<bluer::adv::AdvertisementHandle>,
    ) {
        if !self.opts.enable_peripheral {
            return;
        }
        if table.should_advertise() {
            if adv.is_none() {
                match bluez::register_advertisement(adapter, self.identity_hash, &self.name).await {
                    Ok(handle) => {
                        *adv = Some(handle);
                        tracing::info!(
                            event = "BLE_ADV_GATE",
                            iface = %self.name,
                            state = "on",
                        );
                    }
                    Err(e) => tracing::warn!(
                        "BLE {}: re-registering advertisement failed ({e}); retrying",
                        self.name
                    ),
                }
            }
        } else if adv.take().is_some() {
            tracing::info!(
                event = "BLE_ADV_GATE",
                iface = %self.name,
                state = "off",
                reason = "full",
            );
        }
    }

    /// Deliver one Reticulum packet: to the peer the core addressed it
    /// to, or — with no addressee — to every live link.
    ///
    /// `peer` is the core's #376 delivery hint, the identity a path entry
    /// carries as `via_peer`. With it, this interface stops copying a
    /// routed packet onto links it was never meant for; without it (an
    /// announce, a path request) the fan-out is unchanged. See
    /// [`LinkTable::plan_tx_to`] for the mapping and for the
    /// peer-with-no-live-link decision.
    ///
    /// The notify pipe is paced here (#376): the orchestrator is the
    /// pipe's only writer, so the inter-packet gap is served inline
    /// before the packet's first fragment. That parks the event loop
    /// for up to the gap — deliberate: the gap IS the pipe's throughput
    /// ceiling, and inbound events queue in their channels meanwhile.
    /// Central links get their packet as one queued unit and pace
    /// themselves in their own task.
    #[allow(clippy::too_many_arguments)]
    async fn send_packet(
        &self,
        table: &LinkTable,
        central_pipes: &HashMap<Addr, CentralPipe>,
        notifiers: &mut Vec<CharacteristicNotifier>,
        notify_pacer: &mut links::LinkPacer,
        start: Instant,
        packet: &[u8],
        peer: Option<IdentityHash>,
    ) {
        let plan = table.plan_tx_to(packet, peer.as_ref());
        match plan.route {
            // One line per outbound packet, like the sibling
            // `BLE_TX_GAP`: the bench recipe on #376 reads the routing
            // decision off the same capture as the pacing.
            links::TxRoute::Flood => tracing::info!(
                event = "BLE_TX_FLOOD",
                iface = %self.name,
                links = table.live_links(),
                len = packet.len(),
            ),
            links::TxRoute::Routed { peer, role } => tracing::info!(
                event = "BLE_TX_ROUTE",
                iface = %self.name,
                peer = %hex8(&peer),
                conn = %role.as_str(),
                len = packet.len(),
            ),
            links::TxRoute::NoLink { peer } => {
                self.counters.tx_queue_drops.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .tx_dropped_bytes
                    .fetch_add(packet.len() as usize, Ordering::Relaxed);
                tracing::warn!(
                    event = "BLE_TX_ROUTE_MISS",
                    iface = %self.name,
                    peer = %hex8(&peer),
                    len = packet.len(),
                );
                return;
            }
        }
        let mut delivered = false;
        notifiers.retain(|n| !n.is_stopped());
        if !plan.notify_fragments.is_empty() && !notifiers.is_empty() {
            let now_ms = Instant::now().duration_since(start).as_millis() as u64;
            let wait = notify_pacer.wait_ms(now_ms);
            if wait > 0 {
                tracing::info!(
                    event = "BLE_TX_GAP",
                    iface = %self.name,
                    link = "notify",
                    waited_ms = wait,
                );
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            let mut ok = true;
            for frag in plan.notify_fragments {
                if !send_via_notifiers(notifiers, frag).await {
                    ok = false;
                    break;
                }
            }
            notify_pacer.packet_done(Instant::now().duration_since(start).as_millis() as u64);
            delivered |= ok;
        }
        for (addr, frags) in plan.central {
            let Some(pipe) = central_pipes.get(&addr) else {
                continue;
            };
            let frag_count = frags.len();
            // One try_send per packet: it lands whole or not at all, so
            // a full queue can no longer tear the peer's reassembly
            // with a partial fragment stream.
            if pipe.frames.try_send(frags).is_ok() {
                delivered = true;
            } else {
                self.counters.tx_queue_drops.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .tx_dropped_bytes
                    .fetch_add(packet.len() as usize, Ordering::Relaxed);
                let peer = table
                    .link_by_addr(&addr)
                    .map(|l| hex8(&l.identity))
                    .unwrap_or_default();
                tracing::warn!(
                    event = "BLE_TX_FANOUT_DROP",
                    iface = %self.name,
                    peer = %peer,
                    len = packet.len(),
                    depth = frag_count,
                );
            }
        }
        if delivered {
            self.counters
                .tx_bytes
                .fetch_add(packet.len() as usize, Ordering::Relaxed);
        }
    }

    async fn deliver(&self, packet: Vec<u8>) {
        let _ = self.incoming_tx.send(IncomingPacket { data: packet }).await;
    }

    /// Report a peer loss to the driver loop, unless the identity still
    /// owns another live link — `LinkTable::knows_identity` is the
    /// single decision point (Codeberg #365). Called at the timeout and
    /// disconnect removal sites; the displacement sites are exempt by
    /// construction (a displaced link is replaced by a live link with
    /// the SAME identity, so the peer was never lost).
    async fn report_peer_lost(&self, table: &LinkTable, identity: IdentityHash) {
        if table.knows_identity(&identity) {
            return;
        }
        tracing::info!(
            event = "BLE_PEER_LOST",
            iface = %self.name,
            peer = %hex8(&identity),
        );
        let _ = self
            .peer_event_tx
            .send((self.id, PeerEvent::Lost(identity)))
            .await;
    }

    /// Report a peer's arrival to the driver loop (Codeberg #365) — the
    /// mirror of [`report_peer_lost`](Self::report_peer_lost). Called at
    /// the two link-up sites (peripheral handshake completion, central
    /// admission) exactly when the admission carried no displacement:
    /// `admit` displaces same-identity links only, so `displaced == None`
    /// on an Accept means the identity gained its FIRST link — the moment
    /// `knows_identity` starts returning true. A displacement relink (the
    /// phone's ~60 s random-address rotation) is churn, not an arrival,
    /// and must not spray pull requests.
    async fn report_peer_up(&self, identity: IdentityHash) {
        tracing::info!(
            event = "BLE_PEER_UP",
            iface = %self.name,
            peer = %hex8(&identity),
        );
        let _ = self
            .peer_event_tx
            .send((self.id, PeerEvent::Up(identity)))
            .await;
    }

    /// One `BLE_RX_ABANDON` line per reassembly this receiver discarded
    /// before completion (#373): a torn or interleaved fragment stream
    /// from the peer cost `lost=` whole Reticulum packets; `total=` is
    /// the link's running count. Mirrors the firmware's line of the
    /// same name (slot-keyed there, identity-keyed here), so a merged
    /// bench timeline shows the receiver side of a `BLE_TX_PKT
    /// sent=<all>` that never became a delivery.
    fn log_abandon_reports(&self, table: &mut LinkTable) {
        for (identity, lost, total) in table.take_abandon_reports() {
            tracing::warn!(
                event = "BLE_RX_ABANDON",
                iface = %self.name,
                peer = %hex8(&identity),
                lost = lost,
                total = total,
            );
        }
    }

    fn log_link_up(&self, identity: &IdentityHash, addr: &Addr, role: Role, mtu: usize) {
        tracing::info!(
            event = "BLE_LINK_UP",
            iface = %self.name,
            peer = %hex8(identity),
            addr = %hex12(addr),
            role = role.as_str(),
            mtu = mtu,
        );
    }

    fn log_link_down(&self, identity: &IdentityHash, role: Role, reason: &'static str) {
        tracing::info!(
            event = "BLE_LINK_DOWN",
            iface = %self.name,
            peer = %hex8(identity),
            role = role.as_str(),
            reason = reason,
        );
    }

    /// A duplicate identity we resolved by displacing the old link
    /// (#376/#382). The firmware's `BLE_LINK_DUP … action=displace`,
    /// key for key, so one grep reads a mixed capture: `origin=` names
    /// who opened the connection that won, `old_silence_ms=` how long
    /// the loser had delivered nothing at all.
    fn log_link_dup_displace(
        &self,
        identity: &IdentityHash,
        addr: &Addr,
        role: Role,
        old: &Displaced,
    ) {
        tracing::info!(
            event = "BLE_LINK_DUP",
            iface = %self.name,
            peer = %hex8(identity),
            addr = %hex12(addr),
            action = "displace",
            origin = role.origin_as_str(),
            old_role = old.role.as_str(),
            old_silence_ms = old.silence_ms,
        );
    }

    fn log_rejection(&self, admission: Admission, role: Role, identity: &[u8], addr: &Addr) {
        match admission {
            Admission::RejectSelf => tracing::warn!(
                event = "BLE_LINK_SELF",
                addr = %hex12(addr),
                action = "disconnect",
            ),
            Admission::RejectDuplicate { old_silence_ms } => tracing::info!(
                event = "BLE_LINK_DUP",
                iface = %self.name,
                peer = %hex8(identity),
                addr = %hex12(addr),
                action = "refuse",
                origin = role.origin_as_str(),
                old_silence_ms = old_silence_ms,
            ),
            Admission::RejectFull => tracing::info!(
                "BLE {}: link limit reached, rejecting {}",
                self.name,
                hex12(addr)
            ),
            Admission::Accept => {}
        }
    }
}

/// Emit one frame as a GATT notification. BlueZ fans a notification out
/// to every subscribed central, so exactly one live notifier is used per
/// frame — calling more than one would duplicate the frame at every
/// subscriber.
async fn send_via_notifiers(notifiers: &mut Vec<CharacteristicNotifier>, frame: Vec<u8>) -> bool {
    notifiers.retain(|n| !n.is_stopped());
    for notifier in notifiers.iter_mut() {
        if notifier.notify(frame.clone()).await.is_ok() {
            return true;
        }
    }
    false
}

async fn disconnect_quietly(adapter: &bluer::Adapter, addr: Addr) {
    if let Ok(device) = adapter.device(Address(addr)) {
        let _ = device.disconnect().await;
    }
}

/// `peer=` value: leading 4 identity-hash bytes as hex, the same hex the
/// firmware logs and the LN-<hex8> name carries.
fn hex8(identity: &[u8]) -> String {
    identity
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `addr=` value: the 48-bit address as 12 hex digits in display order,
/// matching the firmware's `addr={:012x}` (which prints the numeric
/// value, i.e. the displayed byte order).
fn hex12(addr: &Addr) -> String {
    addr.iter().map(|b| format!("{b:02x}")).collect()
}

/// Aborts a spawned task when dropped, tying its lifetime to the session.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
