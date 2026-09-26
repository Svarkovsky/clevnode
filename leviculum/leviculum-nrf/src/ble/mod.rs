//! BLE for the nRF52840 + S140, split along the protocol-reuse seam.
//!
//! # The seam (#255 phase A)
//!
//! Everything the board does over Bluetooth today speaks **one** wire
//! protocol — Columba v2.2 — but almost none of the machinery under it
//! is specific to that protocol. The roadmap's next BLE carrier
//! (`ble_leviculum`: BT5 extended-advertising broadcast on Coded PHY,
//! `docs/ble5-broadcast-protocol3-spike.md`) reuses the machinery and
//! replaces the protocol, so the two are separated here rather than
//! after the fact:
//!
//! - [`columba`] — **the protocol.** The GATT service layout and its
//!   UUIDs (`37145b00-…`), the 16-byte identity handshake, the 1-byte
//!   keepalive, the advertisement contents including the v0.3.0
//!   capability record, and (since phase B) the scanner, the MAC-sorting
//!   connection rule with its v0.3.0 override, and the GATT-client
//!   central path that mirrors the peripheral one.
//! - this module and [`notify`] — **protocol-neutral.** SoftDevice
//!   bring-up and its RAM-floor guard, the `Irqs` binding, the SoC-event
//!   task, the packet channels and the [`Interface`] implementation, the
//!   per-connection HVN drain table (whose claim index doubles as the
//!   link identity), the per-link outbound queues and the fan-out that
//!   places each outbound packet on the link of the peer the core
//!   addressed it to — or, for a broadcast, on every live link (#376) —
//!   and the fragment pump that walks [`leviculum_ble_tx::PacketTx`]
//!   over a GATT notify handle.
//!
//! The acceptance test for the split is that a sibling `ble_leviculum`
//! could be added without touching [`columba`]. What such a sibling
//! would still have to reach across the seam for is recorded honestly:
//!
//! 1. [`init`] spawns the Columba tasks by name (via `columba::spawn`,
//!    which since phase B spawns both the peripheral and the central
//!    half behind the one entry point). A second carrier means a second
//!    spawn here — a one-line edit in the neutral module, not a change
//!    to the protocol one.
//! 2. `on_notify_tx_complete` is a method on the `Server` trait, so it
//!    is implemented on whatever concrete GATT server the protocol
//!    defines. The *routing* it performs is neutral ([`HVN_DRAIN`]); the
//!    obligation to call it belongs to each protocol's server.
//! 3. `leviculum_core::framing::ble` is filed as neutral shared framing,
//!    but its fragment header and `KEEPALIVE_BYTE` are Columba wire
//!    specifics. A broadcast carrier with its own framing would not
//!    reuse it, and would not need to: nothing in this module or in
//!    [`notify`] refers to it.
//! 4. The advertisement is built with `LegacyAdvertisementBuilder`,
//!    which is legacy-PDU-only. Extended advertising is a different
//!    nrf-softdevice API and a different `adv_set_count`; the config
//!    below is where that lands.
//!
//! # Architecture notes (carried over from the trouble-host migration)
//!
//! - One task per link: each of the [`PERIPH_LINKS`] peripheral tasks'
//!   `peripheral::advertise_connectable` produces a Connection (the
//!   tasks serialize on one advertising lock — the SoftDevice runs a
//!   single advertising set — so exactly one free task advertises at a
//!   time and a connect hands the lock to the next free one, #372),
//!   then `gatt_server::run(&conn, &server, |evt| { ... })` drives a
//!   callback closure for incoming writes. Outgoing notifications use
//!   `gatt_server::notify_value(conn, handle, &data)` sync, one fragment
//!   at a time, flow-controlled against the SoftDevice's per-connection
//!   HVN queue (see [`notify`]). The central task (phase B) holds the
//!   same shape with the GATT roles mirrored. Concurrent inbound +
//!   outbound is via embassy_futures::select inside the connection
//!   lifetime; each task carries at most one connection, which is what
//!   holds `conn_count` = [`CONN_COUNT`] structurally.
//! - SoftDevice owns RADIO/TIMER0/RTC0/etc.; we don't bind those.
//!   USB VBUS detect goes via `SoftwareVbusDetect` fed by SoC events.
//!
//! [`Interface`]: leviculum_core::traits::Interface

extern crate alloc;

pub mod columba;
pub mod notify;

use alloc::vec::Vec;
use core::mem;
use core::ptr;
use embassy_executor::Spawner;
use embassy_nrf::peripherals;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{bind_interrupts, Peri};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_sync::signal::Signal;
use leviculum_ble_tx::{DrainRouter, DEVICE_NAME_LEN};
use leviculum_core::traits::{Interface, InterfaceError};
use leviculum_core::InterfaceId;
use nrf_softdevice::{raw, SocEvent, Softdevice};

pub use notify::{BLE_TX_DRAIN_UNROUTED, BLE_TX_DRAIN_WAITS, BLE_TX_DROPPED, BLE_TX_PACKETS};

// USBD only. SoftDevice owns the rest of the IRQs we used to bind.
bind_interrupts!(pub struct Irqs {
    USBD => embassy_nrf::usb::InterruptHandler<peripherals::USBD>;
});

/// Incoming (peripheral-role) BLE links accepted concurrently
/// (Codeberg #372).
///
/// Three, on both bins: with one slot, two boards beside a phone pair
/// with each other first and the phone can only reach the board whose
/// single slot is still free — the 2026-09-07/08 field failure. The RAM
/// bill is paid in `memory.x` (both bins share it and the SoftDevice
/// cost is board-independent), and the measured stack floor leaves
/// ~74 KiB of headroom after it, so nothing forces a smaller Pocket
/// value. One advertising set serves all slots: the peripheral tasks
/// serialize on [`columba`]'s advertising lock, so a free slot keeps
/// the board connectable while the busy ones run their sessions.
pub(crate) const PERIPH_LINKS: usize = 3;

/// Outgoing (central-role) links this node initiates. Stays at one
/// (#372 step 4): a board dials at most one neighbour at a time, the
/// central task's sequential scan→connect→session loop holds that
/// structurally, and no field topology has yet needed more — raising it
/// is a budget question (`memory.x`) before it is a code change.
const CENTRAL_LINKS: usize = 1;

/// Concurrent BLE connections the SoftDevice is configured for:
/// every incoming slot plus the initiated one. The boot-time
/// `SD_RAM_FLOOR` check judges this configuration against the linked
/// ceiling on every boot.
const CONN_COUNT: u8 = (PERIPH_LINKS + CENTRAL_LINKS) as u8;

/// Slots in the per-connection HVN drain table ([`HVN_DRAIN`]).
///
/// The #255 design headroom of 4 that this table was pre-sized to is
/// now spent: #372 raised [`CONN_COUNT`] to meet it, so every slot is
/// claimable. Growing further means growing both together (and paying
/// the `memory.x` bill first).
pub const MAX_LINKS: usize = 4;

// A table smaller than the SoftDevice's connection count would refuse a
// legitimate connection's claim — quietly, on a board.
const _: () = assert!(MAX_LINKS >= CONN_COUNT as usize);

/// Where `BLE_GATTS_EVT_HVN_TX_COMPLETE` goes.
///
/// The HVN queue is per connection, so its drain edge is too. Routing it
/// by connection handle is what keeps link A's drain out of link B's
/// fragment pump; see [`leviculum_ble_tx::drain`] for the failure mode
/// and its host tests.
pub static HVN_DRAIN: DrainRouter<MAX_LINKS> = DrainRouter::new();

/// The channel depth every BLE packet queue uses.
const QUEUE_DEPTH: usize = 4;

/// One packet queue, as both the node-facing channels and the per-link
/// queues use it.
type PacketQueue = Channel<CriticalSectionRawMutex, Vec<u8>, QUEUE_DEPTH>;

/// An inbound packet with the peer link it arrived through (Codeberg
/// #365): the identity hash the peer presented in the handshake — the
/// same 16 bytes the peer events report — or `None` for bytes that
/// arrived before the handshake completed. The main loop passes it to
/// `NodeCore::handle_packet_from_peer` so path entries learned over
/// this link are attributable when the link dies, even when the
/// announce identity differs from the link identity (Columba).
type InboundQueue = Channel<CriticalSectionRawMutex, (Option<[u8; 16]>, Vec<u8>), QUEUE_DEPTH>;

/// An outbound packet with the core's #376 delivery hint: the 16-byte
/// identity of the peer these bytes are for, or `None` for a broadcast
/// (an announce, a path request, anything the core did not address at a
/// named peer). [`tx_fanout_task`] turns the hint into links.
type OutboundQueue = Channel<CriticalSectionRawMutex, (Option<[u8; 16]>, Vec<u8>), QUEUE_DEPTH>;

// Channels between the BLE tasks and the binaries' main loop.
static BLE_INCOMING: InboundQueue = Channel::new();
static BLE_OUTGOING: OutboundQueue = Channel::new();

/// A peer-link transition report toward the main loop (Codeberg #365).
/// Only the interface knows a single peer inside its broadcast domain
/// came or went while the interface itself stayed up; the identity hash
/// is the value the Columba handshake exchanges.
#[derive(Clone, Copy)]
pub enum PeerEvent {
    /// The identity's FIRST link on this interface is up. The main loop
    /// hands it to `NodeCore::handle_interface_peer_up`, which pulls the
    /// peer's delivery path over the new link — Columba answers a path
    /// request but announces on neither connect nor reconnect, so
    /// without the pull a relay that lost its direct entry stays blind
    /// until the peer's periodic announce.
    Up([u8; 16]),
    /// The identity's LAST link on this interface died. The main loop
    /// hands it to `NodeCore::handle_interface_peer_lost`, which culls
    /// the path entries whose next hop is that peer — without it a
    /// stale 1-hop BLE path keeps swallowing traffic after the peer
    /// walked out of range.
    Lost([u8; 16]),
}

/// Peer-event reports toward the main loop (Codeberg #365). Fed by
/// [`columba`]'s link registry (both GATT roles); protocol-neutral, so
/// a sibling carrier reports here too. `Up` and `Lost` share this ONE
/// ordered channel on purpose: a link flap is a `Lost` followed by an
/// `Up`, and the cull must land before the pull decision looks at the
/// path table — on separate channels the `Up` could win the race, see
/// the still-standing direct entry, skip the pull, and then have the
/// `Lost` cull re-arm exactly the trap the pull exists to clear.
/// Depth 8: a carrier-off teardown drops every live link at once, and
/// with [`CONN_COUNT`] = 4 that is up to four `Lost` reports in one
/// burst before the main loop runs — twice that leaves room for the
/// relink `Up`s that follow (#372).
static BLE_PEER_EVENTS: Channel<CriticalSectionRawMutex, PeerEvent, 8> = Channel::new();

/// Report a peer transition (see [`BLE_PEER_EVENTS`]). `try_send`: with
/// the 8-deep queue full the oldest pending report wins and this one is
/// dropped — the affected node then falls back to the pre-#365
/// behaviour (a dropped `Lost` ages the paths out via ordinary expiry,
/// a dropped `Up` waits for the peer's periodic announce) rather than
/// blocking a connection task.
pub(crate) fn report_peer_event(event: PeerEvent) {
    if BLE_PEER_EVENTS.try_send(event).is_err() {
        let (kind, identity) = match event {
            PeerEvent::Up(id) => ("up", id),
            PeerEvent::Lost(id) => ("lost", id),
        };
        crate::log::log_fmt(
            "[BLE ] ",
            format_args!(
                "BLE_PEER_EVENT_DROPPED kind={} peer={:02x}{:02x}{:02x}{:02x}",
                kind, identity[0], identity[1], identity[2], identity[3]
            ),
        );
    }
}

/// The runtime half of the media profile's BLE switch — the boot half
/// is [`init`]'s `columba_enabled`, which decides whether the protocol
/// tasks exist at all.
///
/// A runtime `ble=off` used to be a mute: [`BleInterface::try_send`]
/// dropped outbound packets and the binaries' RX arms dropped inbound
/// ones, but the SoftDevice links stayed connected and the
/// advertisement kept going — a phone still showed the board as
/// connected, and no `PeerEvent::Lost` ever fired, so the #365 cull
/// never ran. Off now means off on the air: each protocol task holds
/// one of these latches, [`note_media_changed`] pokes them all, and the
/// woken task re-reads [`crate::media::ble_active`] — a live session
/// disconnects its link (through the same teardown a peer walking out
/// of range takes, so the `Lost` report and the cull are identical),
/// and the advertise/scan futures are dropped and not re-entered until
/// the carrier reads on again.
///
/// One latch per waiter, not one shared: `Signal::wait` consumes the
/// latch, so a shared one would wake whichever task polled first and
/// starve the others. Within a waiter only one of [`carrier_on`] /
/// [`carrier_off`] is ever awaited at a time, and both re-check the
/// media state after every wake, so a stale latched wake (a LoRa-only
/// profile change, a flip-and-back while the task was busy) is a no-op.
///
/// The waiters, one latch each: the [`PERIPH_LINKS`] peripheral
/// advertise/accept tasks, the central scan task, and one per drain
/// slot for the live sessions (a session's waiter is keyed by its
/// unique slot, so peripheral and central sessions cannot collide).
static CARRIER_WAKES: [Signal<CriticalSectionRawMutex, ()>; PERIPH_LINKS + 1 + MAX_LINKS] =
    [const { Signal::new() }; PERIPH_LINKS + 1 + MAX_LINKS];

/// A protocol task's handle on its carrier-wake latch (see
/// [`CARRIER_WAKES`]).
#[derive(Clone, Copy)]
pub(crate) enum CarrierWaiter {
    /// Peripheral advertise/accept task `i` (`0..PERIPH_LINKS`).
    Peripheral(usize),
    /// The central scan/initiate task.
    Central,
    /// A live session, keyed by its drain-table slot.
    Session(usize),
}

impl CarrierWaiter {
    fn index(self) -> usize {
        match self {
            CarrierWaiter::Peripheral(i) => i,
            CarrierWaiter::Central => PERIPH_LINKS,
            CarrierWaiter::Session(slot) => PERIPH_LINKS + 1 + slot,
        }
    }
}

/// Wake every BLE protocol task to re-read the media profile. Called by
/// [`crate::media::apply`] after the new profile is already in force,
/// so a woken task cannot read the old state. Before the tasks exist —
/// or on a boot that never spawned them — this just latches the wakes,
/// which is the required no-op: with `ble=off` at boot no link, no
/// advertisement and no scan exist to stop.
pub fn note_media_changed() {
    for wake in &CARRIER_WAKES {
        wake.signal(());
    }
}

/// The operator's inter-packet-gap override (#376, `TYPE_BLE_TX_GAP`),
/// in milliseconds. With no override — the boot state, and what every
/// reset restores — the pumps serve the compiled
/// [`leviculum_ble_tx::DEFAULT_TX_GAP_MS`] (100 ms, the measured desk
/// value; the justification lives on that constant). The knob stays a
/// measurement instrument: any set value overrides the default, `0`
/// disables the gap entirely, and nothing is persisted, exactly like
/// the LoRa transmit spacing (#345): a bench must not be able to leave
/// a board silently mispaced after the session that paced it.
///
/// `u16::MAX` is the no-override sentinel; the control envelope refuses
/// anything above `BLE_TX_GAP_MAX_MS`, so no operator value can collide
/// with it (asserted below).
const TX_GAP_OVERRIDE_NONE: u16 = u16::MAX;
const _: () = assert!(leviculum_core::envelope::BLE_TX_GAP_MAX_MS < TX_GAP_OVERRIDE_NONE);
static TX_GAP_OVERRIDE_MS: core::sync::atomic::AtomicU16 =
    core::sync::atomic::AtomicU16::new(TX_GAP_OVERRIDE_NONE);

/// Set the inter-packet gap override. Called from the serial control
/// task; the per-connection pumps read it at each packet, so it takes
/// effect from the next packet on every live link without touching the
/// links.
pub fn set_tx_gap_ms(gap_ms: u16) {
    TX_GAP_OVERRIDE_MS.store(gap_ms, core::sync::atomic::Ordering::Relaxed);
    crate::log::log_fmt("[BLE ] ", format_args!("tx_gap_ms={}", gap_ms));
}

/// The gap the pumps serve right now: the override if one was set this
/// boot, [`leviculum_ble_tx::DEFAULT_TX_GAP_MS`] otherwise.
pub(crate) fn tx_gap_ms() -> u16 {
    let raw = TX_GAP_OVERRIDE_MS.load(core::sync::atomic::Ordering::Relaxed);
    leviculum_ble_tx::effective_tx_gap_ms((raw != TX_GAP_OVERRIDE_NONE).then_some(raw))
}

/// Resolve when the BLE carrier reads off. Selected against a live
/// session or an advertise/scan future; never resolves while the
/// carrier stays on.
pub(crate) async fn carrier_off(waiter: CarrierWaiter) {
    let wake = &CARRIER_WAKES[waiter.index()];
    while crate::media::ble_active() {
        wake.wait().await;
    }
}

/// Resolve when the BLE carrier reads on — the gate at the top of each
/// protocol task's loop, so a switched-off carrier neither advertises
/// nor scans nor accepts.
pub(crate) async fn carrier_on(waiter: CarrierWaiter) {
    let wake = &CARRIER_WAKES[waiter.index()];
    while !crate::media::ble_active() {
        wake.wait().await;
    }
}

/// Per-link outbound queues, indexed by the link's [`HVN_DRAIN`] slot.
///
/// A Reticulum interface is a broadcast domain: one BROADCAST `try_send`
/// from the node core must reach **every** peer on the medium, exactly
/// as one LoRa transmission reaches every listener. With two live links,
/// two connection tasks receiving from the single [`BLE_OUTGOING`]
/// channel would round-robin it instead — each announce reaching one
/// peer and not the other — so [`tx_fanout_task`] is the only consumer
/// of [`BLE_OUTGOING`]. Which links are live is read from
/// [`HVN_DRAIN`]'s claims: a connection claims its slot for its whole
/// lifetime, so the drain table is also the link table, and a second
/// registry that could disagree with it is never built.
///
/// A ROUTED packet is a different question, and since #376 the core
/// answers it: the packet carries the peer it is for, and the fan-out
/// queues it on that peer's link alone. The broadcast domain is
/// unchanged — an announce still reaches every link — but a report
/// addressed to the phone no longer also travels to the board beside
/// it, which used to forward it back to the phone.
static LINK_OUT: [PacketQueue; MAX_LINKS] = [const { Channel::new() }; MAX_LINKS];

/// This link's private outbound queue (see [`LINK_OUT`]).
///
/// Handed to a connection task along with its drain-slot index. Stale
/// packets from a previous tenancy of the slot are the caller's to
/// drain at claim time, exactly as [`BLE_OUTGOING`] was drained per
/// connection before phase B.
pub(crate) fn link_out(slot_index: usize) -> &'static PacketQueue {
    &LINK_OUT[slot_index]
}

/// Deliver one outbound packet: to the link of the peer the core
/// addressed it to, or — with no addressee — to every live link (see
/// [`LINK_OUT`]).
///
/// The hint is the core's `via_peer` (Codeberg #376), the same 16 bytes
/// this interface reports on peer-up/peer-lost, and
/// [`leviculum_ble_tx::plan_fanout`] maps it onto a drain slot. Before
/// it, a routed packet was copied onto every link: a telemetry report
/// addressed to the phone also went to the other board, which forwarded
/// it back to the phone — double airtime per report, `TRANSPORT dup=` on
/// both boards, and relayed copies racing the direct ones (the desk
/// timeline on #376). A broadcast still reaches every peer, which is
/// what a Reticulum interface owes it.
///
/// `try_send`, never `send`: a link whose queue is full — a peer that
/// stopped draining — costs that link the packet and is told so in the
/// log, but must not stall delivery to the healthy links or wedge the
/// fan-out. With no live link at all a flooded packet is dropped
/// silently; that is today's behaviour for an unconnected board, just
/// moved from the connect-time drain to the moment of sending. A ROUTED
/// packet whose peer has no live link is dropped loudly
/// (`BLE_TX_ROUTE_MISS`) rather than flooded — see
/// [`leviculum_ble_tx::TxFanout::NoLink`] for why.
#[embassy_executor::task]
async fn tx_fanout_task() -> ! {
    loop {
        let (peer, packet) = BLE_OUTGOING.receive().await;
        match columba::plan_fanout(peer.as_ref()) {
            // `Route` and `NoLink` are only reachable with a hint, so
            // the peer is Some in both arms.
            leviculum_ble_tx::TxFanout::Route(slot) => {
                let hint = peer.unwrap_or_default();
                let Some(handle) = HVN_DRAIN.handle_at(slot) else {
                    // The registry named a slot the drain table no longer
                    // claims: the link died between the two reads. Same
                    // decision as NoLink.
                    log_route_miss(&hint, packet.len());
                    continue;
                };
                crate::log::log_fmt(
                    "[BLE ] ",
                    format_args!(
                        "BLE_TX_ROUTE peer={:02x}{:02x}{:02x}{:02x} conn={} slot={} len={}",
                        hint[0],
                        hint[1],
                        hint[2],
                        hint[3],
                        handle,
                        slot,
                        packet.len()
                    ),
                );
                queue_on_link(slot, packet);
            }
            leviculum_ble_tx::TxFanout::NoLink => {
                log_route_miss(&peer.unwrap_or_default(), packet.len())
            }
            leviculum_ble_tx::TxFanout::Flood => {
                let mut links = 0usize;
                for (index, _) in LINK_OUT.iter().enumerate() {
                    if HVN_DRAIN.handle_at(index).is_none() {
                        continue;
                    }
                    links += 1;
                    queue_on_link(index, packet.clone());
                }
                crate::log::log_fmt(
                    "[BLE ] ",
                    format_args!("BLE_TX_FLOOD links={} len={}", links, packet.len()),
                );
            }
        }
    }
}

/// Put one packet in a link's outbound queue, reporting a full queue.
fn queue_on_link(slot: usize, packet: Vec<u8>) {
    let len = packet.len();
    if LINK_OUT[slot].try_send(packet).is_err() {
        crate::log::log_fmt(
            "[BLE ] ",
            format_args!(
                "BLE_TX_FANOUT_DROP slot={} len={} depth={}",
                slot, len, QUEUE_DEPTH
            ),
        );
    }
}

/// One line per routed packet whose peer holds no live link here.
fn log_route_miss(peer: &[u8; 16], len: usize) {
    BLE_TX_ROUTE_MISSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    crate::log::log_fmt(
        "[BLE ] ",
        format_args!(
            "BLE_TX_ROUTE_MISS peer={:02x}{:02x}{:02x}{:02x} len={}",
            peer[0], peer[1], peer[2], peer[3], len
        ),
    );
}

/// Routed packets dropped because the addressed peer held no live link
/// (Codeberg #376), for the `BLE_COUNTERS` line.
pub static BLE_TX_ROUTE_MISSES: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

pub struct BleChannels {
    pub incoming_rx: Receiver<'static, CriticalSectionRawMutex, (Option<[u8; 16]>, Vec<u8>), 4>,
    /// The outbound side carries the #376 delivery hint alongside the
    /// bytes; see [`OutboundQueue`].
    pub outgoing_tx: Sender<'static, CriticalSectionRawMutex, (Option<[u8; 16]>, Vec<u8>), 4>,
    /// Peer transitions (Codeberg #365); the main loop feeds `Lost` to
    /// `handle_interface_peer_lost` and `Up` to
    /// `handle_interface_peer_up`. See [`BLE_PEER_EVENTS`] for why both
    /// ride one ordered channel.
    pub peer_event_rx: Receiver<'static, CriticalSectionRawMutex, PeerEvent, 8>,
}

pub fn channels() -> BleChannels {
    BleChannels {
        incoming_rx: BLE_INCOMING.receiver(),
        outgoing_tx: BLE_OUTGOING.sender(),
        peer_event_rx: BLE_PEER_EVENTS.receiver(),
    }
}

pub struct BleInterface {
    sender: Sender<'static, CriticalSectionRawMutex, (Option<[u8; 16]>, Vec<u8>), 4>,
    /// The carrier-off drop run — see the LoRa interface for why these
    /// are counted rather than logged one line per packet.
    drops: leviculum_media_state::DropRun,
}

impl BleInterface {
    pub fn new(
        sender: Sender<'static, CriticalSectionRawMutex, (Option<[u8; 16]>, Vec<u8>), 4>,
    ) -> Self {
        Self {
            sender,
            drops: leviculum_media_state::DropRun::new(),
        }
    }

    /// Distinct live peer identities behind this interface (Codeberg
    /// #365), for the main loop's `set_interface_peer_count` mirror —
    /// the sibling of the `is_online` mirror. Zero with the carrier
    /// off: links torn down by a media switch cannot be asked anything,
    /// whatever the registry still holds mid-teardown.
    pub fn peer_count(&self) -> usize {
        if crate::media::ble_active() {
            columba::live_peer_count()
        } else {
            0
        }
    }
}

impl Interface for BleInterface {
    fn id(&self) -> InterfaceId {
        InterfaceId(2)
    }
    fn name(&self) -> &str {
        "ble"
    }
    fn mtu(&self) -> usize {
        564
    }
    fn is_online(&self) -> bool {
        // Online means "a frame handed to this interface can reach a
        // peer": the carrier must be on AND at least one link must be
        // live — with no link the fan-out drops every packet silently
        // (see `tx_fanout_task`). The main loop mirrors this into the
        // core, where a path over an offline interface is no path
        // (Codeberg #365).
        crate::media::ble_active() && HVN_DRAIN.claimed() > 0
    }
    fn try_send(&mut self, data: &[u8]) -> Result<(), InterfaceError> {
        self.try_send_to_peer(data, None, false)
    }
    /// The #376 delivery hint, carried to [`tx_fanout_task`]: this
    /// interface holds several point-to-point links, so a routed packet
    /// belongs on the addressed peer's link alone. `None` keeps the
    /// broadcast fan-out.
    fn try_send_to_peer(
        &mut self,
        data: &[u8],
        peer: Option<&[u8; 16]>,
        _high_priority: bool,
    ) -> Result<(), InterfaceError> {
        // The media profile, applied at the interface — see the LoRa
        // interface for why this is `Ok` and not `BufferFull`.
        if !crate::media::ble_active() {
            if let Some(run) = self.drops.dropped(data.len()) {
                crate::media::log_tx_drop(self.name(), run);
            }
            return Ok(());
        }
        if let Some(run) = self.drops.resumed() {
            crate::media::log_tx_resumed(self.name(), run);
        }
        self.sender
            .try_send((peer.copied(), data.to_vec()))
            .map_err(|_| {
                // Codeberg #344: same silence as the other two. A phone that
                // stops draining the notify path fills this queue, and the board
                // could not tell that from a mesh with nothing to say.
                crate::log::log_fmt(
                    "[IFACE_FULL] ",
                    format_args!(
                        "iface={} depth={} len={}",
                        self.name(),
                        self.sender.capacity(),
                        data.len()
                    ),
                );
                InterfaceError::BufferFull
            })
    }
}

#[embassy_executor::task]
async fn softdevice_task(sd: &'static Softdevice, vbus: &'static SoftwareVbusDetect) -> ! {
    sd.run_with_callback(|evt| match evt {
        SocEvent::PowerUsbDetected => vbus.detected(true),
        SocEvent::PowerUsbRemoved => vbus.detected(false),
        SocEvent::PowerUsbPowerReady => vbus.ready(),
        _ => {}
    })
    .await
}

/// The GAP connection configuration, as one value so [`sd_config`] and
/// [`required_app_ram_base`] cannot drift apart.
const CONN_GAP: raw::ble_gap_conn_cfg_t = raw::ble_gap_conn_cfg_t {
    conn_count: CONN_COUNT,
    event_length: 24,
};

/// A ceiling, not an answer. `ble_gatt_conn_cfg_t::att_mtu` is the
/// "maximum size of ATT packet the SoftDevice can send or receive" for
/// connections opened on this conn_cfg tag; the S140 does not answer an
/// Exchange MTU Request on its own, it raises
/// BLE_GATTS_EVT_EXCHANGE_MTU_REQUEST and waits for
/// `sd_ble_gatts_exchange_mtu_reply`, whose server_rx_mtu "maximum
/// value is ble_gatt_conn_cfg_t::att_mtu in the connection
/// configuration used for this connection". nrf-softdevice makes that
/// reply for us with min(peer's request, this value)
/// (nrf-softdevice/src/ble/gatt_server.rs:447-463), and the connection
/// is opened on the tag this value was set under: the crate sets every
/// conn_cfg under APP_CONN_CFG_TAG = 1 (softdevice.rs:68) and starts
/// advertising with the same tag (peripheral.rs:277).
///
/// What this does NOT do is keep events inside the
/// `nrf-softdevice/evt-max-size-512` buffer selected in Cargo.toml.
/// That bound is the 251-byte width of rx/tx (see
/// [`columba::ReticulumService`]): both values live in the SoftDevice's
/// attribute table with max_len 251 and no write authorization, so a
/// longer write is rejected by the SoftDevice with an ATT error and
/// never becomes an event. Widening 251 is what forces a recheck of
/// `evt-max-size-*`; moving this number does not.
const CONN_GATT: raw::ble_gatt_conn_cfg_t = raw::ble_gatt_conn_cfg_t { att_mtu: 256 };

const ATTR_TAB_SIZE: raw::ble_gatts_cfg_attr_tab_size_t = raw::ble_gatts_cfg_attr_tab_size_t {
    attr_tab_size: raw::BLE_GATTS_ATTR_TAB_SIZE_DEFAULT,
};

/// Role counts, fed from [`PERIPH_LINKS`] / [`CENTRAL_LINKS`] so the
/// SoftDevice grant and the task structure cannot drift: one peripheral
/// task per incoming slot (#372), one sequential central task
/// (`central_role_count: 1` was the phase-B role flip, tied to the
/// cleared `PERIPHERAL_ONLY` advertisement bit in [`columba`]).
///
/// Not a `const`: bindgen's `new_bitfield_1` is a plain `fn`.
fn role_count_cfg() -> raw::ble_gap_cfg_role_count_t {
    raw::ble_gap_cfg_role_count_t {
        adv_set_count: 1,
        periph_role_count: PERIPH_LINKS as u8,
        central_role_count: CENTRAL_LINKS as u8,
        central_sec_count: 0,
        _bitfield_1: raw::ble_gap_cfg_role_count_t::new_bitfield_1(0),
    }
}

/// The GAP device name a connected peer reads. Our name is
/// runtime-derived (`LN-<hex8>` of this node's identity hash), and the
/// SoftDevice's contract for this struct is explicit
/// (`nrf-softdevice-s140` bindings, `ble_gap_cfg_device_name_t`):
///
///   "If vloc is BLE_GATTS_VLOC_STACK:
///     - p_value must point to non-volatile memory (flash) or be NULL.
///     - If p_value is NULL, the device name will initially be empty."
///
/// So a pointer into RAM is not an option here, no matter how 'static
/// that RAM is — handing `sd_ble_cfg_set` one earns
/// NRF_ERROR_INVALID_ADDR, which nrf-softdevice turns into an outright
/// panic (`softdevice.rs`, `cfg_set`). That panic sits inside
/// `Softdevice::enable` below, i.e. inside `main` before its first
/// await, so the USB task never gets polled: the board dies
/// pre-enumeration and boot-loops (fixed in `e4d7ef7`; regression
/// `e52dba1`).
///
/// NULL + `max_len` is the reservation: the name lives in the
/// SoftDevice's own attribute table, empty at enable, and
/// [`set_gap_device_name`] writes it. 11 <= BLE_GAP_DEVNAME_DEFAULT_LEN
/// (31), so [`ATTR_TAB_SIZE`] needs no bump.
fn gap_device_name_cfg() -> raw::ble_gap_cfg_device_name_t {
    raw::ble_gap_cfg_device_name_t {
        p_value: ptr::null_mut(),
        current_len: 0,
        max_len: DEVICE_NAME_LEN as u16,
        write_perm: unsafe { mem::zeroed() },
        _bitfield_1: raw::ble_gap_cfg_device_name_t::new_bitfield_1(
            raw::BLE_GATTS_VLOC_STACK as u8,
        ),
    }
}

/// The LF clock configuration. Synthesized LF from the HF crystal;
/// matches Heltec/RAK/Adafruit factory bootloaders' expectation. RAK4631
/// and T114 have no dedicated 32.768 kHz LF crystal.
const CLOCK_CFG: raw::nrf_clock_lf_cfg_t = raw::nrf_clock_lf_cfg_t {
    source: raw::NRF_CLOCK_LF_SRC_RC as u8,
    rc_ctiv: 16,
    rc_temp_ctiv: 2,
    accuracy: raw::NRF_CLOCK_LF_ACCURACY_500_PPM as u8,
};

/// The one SoftDevice configuration this firmware enables with.
fn sd_config() -> nrf_softdevice::Config {
    nrf_softdevice::Config {
        clock: Some(CLOCK_CFG),
        conn_gap: Some(CONN_GAP),
        conn_gatt: Some(CONN_GATT),
        gatts_attr_tab_size: Some(ATTR_TAB_SIZE),
        gap_role_count: Some(role_count_cfg()),
        gap_device_name: Some(gap_device_name_cfg()),
        ..Default::default()
    }
}

/// The SoftDevice's RAM ceiling: `__sretained` = `ORIGIN(RETAINED)` from
/// `memory.x`. Above it sit first the cross-boot records (`.retained`),
/// then the flip-link stack floor `ORIGIN(RAM)` — so an SD that stays
/// below this line touches neither the records nor the stack. Comparing
/// against `_stack_end` instead would be lenient by exactly the retained
/// region: an SD landing inside it would pass and silently eat the
/// post-mortems and boot breadcrumbs.
fn sd_ceiling() -> u32 {
    extern "C" {
        static __sretained: u32;
    }
    ptr::addr_of!(__sretained) as u32
}

/// The SoftDevice's own assertion callback, used only for the RAM probe
/// below. The probe never starts BLE activity, so reaching this means
/// the enable/disable cycle itself faulted — worth a loud panic, which
/// the lib panic handler persists as a post-mortem.
unsafe extern "C" fn probe_fault_handler(id: u32, pc: u32, info: u32) {
    panic!("SD fault during RAM probe id={id} pc={pc:#x} info={info:#x}");
}

/// Ask the S140 what application RAM base [`sd_config`] requires,
/// without letting it initialise the BLE stack.
///
/// `sd_ble_enable` against a deliberately undersized base answers
/// `NRF_ERROR_NO_MEM` and writes the exact required base into the in-out
/// parameter, and on that path it never begins initialisation — the
/// measurement is free of side effects. The SoftDevice is disabled again
/// before returning, so `Softdevice::enable` afterwards sees a clean
/// slate. `src/bin/sd-ram-probe.rs` runs this same cycle seven times in
/// one boot; this is one instance of it, on the real config.
///
/// Returns 0 if any call failed for a reason other than the expected
/// `NO_MEM` — an inconclusive probe must not be read as "it fits".
fn required_app_ram_base() -> u32 {
    /// Below the S140's own 8 KiB MBR reservation floor, so the answer is
    /// always NO_MEM and never a success write-back.
    const UNDERSIZED_BASE: u32 = 0x2000_2000;

    // SAFETY: nothing has enabled the SoftDevice yet (this runs at the
    // top of `init`, which each binary calls once), and every pointer
    // handed over lives for the duration of its call.
    let ret = unsafe { raw::sd_softdevice_enable(&CLOCK_CFG, Some(probe_fault_handler)) };
    if ret != raw::NRF_SUCCESS {
        crate::warn!("BLE: RAM probe could not enable the SD, err={}", ret);
        return 0;
    }

    let conn_gap = raw::ble_cfg_t {
        conn_cfg: raw::ble_conn_cfg_t {
            conn_cfg_tag: APP_CONN_CFG_TAG,
            params: raw::ble_conn_cfg_t__bindgen_ty_1 {
                gap_conn_cfg: CONN_GAP,
            },
        },
    };
    let conn_gatt = raw::ble_cfg_t {
        conn_cfg: raw::ble_conn_cfg_t {
            conn_cfg_tag: APP_CONN_CFG_TAG,
            params: raw::ble_conn_cfg_t__bindgen_ty_1 {
                gatt_conn_cfg: CONN_GATT,
            },
        },
    };
    let role_count = raw::ble_cfg_t {
        gap_cfg: raw::ble_gap_cfg_t {
            role_count_cfg: role_count_cfg(),
        },
    };
    let device_name = raw::ble_cfg_t {
        gap_cfg: raw::ble_gap_cfg_t {
            device_name_cfg: gap_device_name_cfg(),
        },
    };
    let attr_tab = raw::ble_cfg_t {
        gatts_cfg: raw::ble_gatts_cfg_t {
            attr_tab_size: ATTR_TAB_SIZE,
        },
    };

    // Same five configs `Softdevice::enable` sets for `sd_config()`, in
    // the same order and under the same tag. `sd_ble_cfg_set` may itself
    // answer NO_MEM against the undersized base; like nrf-softdevice we
    // let `sd_ble_enable` deliver the verdict and only bail on errors
    // that mean the config never registered at all.
    for (id, cfg) in [
        (raw::BLE_CONN_CFGS_BLE_CONN_CFG_GAP, &conn_gap),
        (raw::BLE_CONN_CFGS_BLE_CONN_CFG_GATT, &conn_gatt),
        (raw::BLE_GAP_CFGS_BLE_GAP_CFG_ROLE_COUNT, &role_count),
        (raw::BLE_GAP_CFGS_BLE_GAP_CFG_DEVICE_NAME, &device_name),
        (raw::BLE_GATTS_CFGS_BLE_GATTS_CFG_ATTR_TAB_SIZE, &attr_tab),
    ] {
        // SAFETY: the SoftDevice is enabled and `cfg` outlives the call.
        let ret = unsafe { raw::sd_ble_cfg_set(id, cfg, UNDERSIZED_BASE) };
        if ret != raw::NRF_SUCCESS && ret != raw::NRF_ERROR_NO_MEM {
            crate::warn!("BLE: RAM probe cfg_set id={} err={}", id, ret);
            // SAFETY: enabled just above.
            let _ = unsafe { raw::sd_softdevice_disable() };
            return 0;
        }
    }

    let mut wanted: u32 = UNDERSIZED_BASE;
    // SAFETY: enabled, `wanted` is a live in-out parameter.
    let ret = unsafe { raw::sd_ble_enable(&mut wanted) };
    // SAFETY: enabled; the BLE stack was never initialised on the NO_MEM
    // path, so this is a plain teardown of the SoC-level enable.
    let _ = unsafe { raw::sd_softdevice_disable() };

    if ret != raw::NRF_ERROR_NO_MEM {
        crate::warn!("BLE: RAM probe expected NO_MEM, got err={}", ret);
        return 0;
    }
    wanted
}

/// `APP_CONN_CFG_TAG` — the tag nrf-softdevice opens every connection on
/// (`softdevice.rs:68`) and advertises with (`peripheral.rs:277`).
const APP_CONN_CFG_TAG: u8 = 1;

/// Cycles of blocking delay after the probe's `sd_softdevice_disable`,
/// before the real enable. 64 MHz core, so ~100 ms — the same settling
/// time `src/bin/sd-ram-probe.rs` leaves between its seven cycles, which
/// is the only enable/disable/enable sequence measured on a board.
const PROBE_SETTLE_CYCLES: u32 = 6_400_000;

/// Refuse to enable the SoftDevice if it would take RAM out from under
/// the retained records or our stack.
///
/// nrf-softdevice performs its own version of this check inside
/// `Softdevice::enable`, and under flip-link that check is a whole stack
/// region too lenient: it compares the SoftDevice's requirement against
/// `get_app_ram_base()`, which is `__sdata` — the *top* of the stack
/// region, not the bottom. The layout is
///
/// ```text
///   __sretained = ORIGIN(RETAINED) <- the real ceiling; below it is
///        .retained (cross-boot records)      the SD's
///   _stack_end = ORIGIN(RAM)
///        |  stack, grows DOWN
///   __sdata = _stack_start     <- what nrf-softdevice compares against
///        .data / .bss
/// ```
///
/// so a configuration whose requirement lands anywhere inside the
/// retained region or the stack passes the crate's check, the SoftDevice
/// takes it, and the symptom is not a refusal at boot but an SD internal
/// assertion under load, once the stack happens to get that deep — or,
/// for the retained region, silently corrupted post-mortems. Raising
/// `conn_count` is exactly the change that moves the requirement, which
/// is why this guard lands in phase A, before the change that needs it.
///
/// A violation panics: the post-mortem survives the reset and
/// `scripts/lnode-panic-query.sh` reads it back, so the board says which
/// two numbers disagreed instead of dying silently later.
fn assert_sd_fits_below_retained(wanted: u32) {
    // `floor=` on the wire for continuity: the value is the same address
    // the pre-RETAINED layout logged (ORIGIN(RETAINED) took over the old
    // ORIGIN(RAM)), only what sits directly above it changed.
    let floor = sd_ceiling();
    if wanted == 0 {
        // Inconclusive, not "fits". Loud, but not fatal: refusing to
        // boot over a probe that could not run would trade a possible
        // problem for a certain one.
        crate::warn!("BLE: SD RAM probe inconclusive, floor={} unchecked", floor);
        return;
    }
    crate::log::log_fmt_critical(
        "[BLE ] ",
        format_args!(
            "SD_RAM_FLOOR wanted=0x{:08x} floor=0x{:08x} margin={} fits={}",
            wanted,
            floor,
            i64::from(floor) - i64::from(wanted),
            u8::from(wanted <= floor),
        ),
    );
    assert!(
        wanted <= floor,
        "SoftDevice wants app RAM base {wanted:#010x}, its ceiling (ORIGIN(RETAINED)) is \
         {floor:#010x}: the SD would own the retained records and then the bottom of our stack. \
         Raise ORIGIN(RETAINED) and ORIGIN(RAM) in memory.x by the shortfall and shrink \
         LENGTH(RAM) by the same amount."
    );
}

/// Write this node's GAP device name into the SoftDevice's attribute
/// table: the operator's name (#235) if one is set, `LN-<hex8>` of the
/// identity hash otherwise (#255).
///
/// The name comes from [`crate::name::boot_gap_name`], which
/// [`crate::name::note_boot_name`] filled in just before
/// [`init`] — the same value the advertisement's Complete Local Name is
/// built from in [`columba::spawn`], so the two BLE surfaces cannot show
/// different names. A name longer than [`DEVICE_NAME_LEN`] arrives here
/// already shortened on a codepoint boundary
/// (`leviculum_ble_tx::gap_name`).
///
/// This is the runtime half of [`gap_device_name_cfg`]: the config
/// reserves `DEVICE_NAME_LEN` bytes with a NULL `p_value` — the only
/// pointer `BLE_GATTS_VLOC_STACK` accepts for a name that is not a flash
/// literal — leaving the name empty, and `sd_ble_gap_device_name_set`
/// fills it in. The SoftDevice copies the bytes out of `name`, so a
/// stack local is a valid source; nothing has to stay alive afterwards.
///
/// Deliberately non-fatal. The device name is cosmetic — it decides what
/// a scanner lists, nothing about whether packets move — while this call
/// sits on the boot path ahead of USB enumeration, where a panic costs
/// the whole node and takes the log with it. `e52dba1` is exactly that
/// failure, and the lesson is not only "hand it the right pointer" but
/// "never let the name be able to stop the boot".
///
/// An operator-set name is written here and nowhere else, though this
/// call would take one at runtime — the SoftDevice copies the bytes, so
/// unlike the advertisement payload there is no aliasing problem. What
/// stops it is [`columba::spawn`]'s scan response, which cannot be
/// rebuilt under a live stack: updating only the attribute would leave
/// the board advertising one name and answering with another. Both wait
/// for the reset together, and the control frame's report says so
/// (`crate::name`).
fn set_gap_device_name() {
    let name = crate::name::boot_gap_name();
    // No write access: the name is ours to publish, a peer has no
    // business changing it. Same permission the config carries.
    let write_perm: raw::ble_gap_conn_sec_mode_t = unsafe { mem::zeroed() };
    // SAFETY: the SoftDevice is enabled (the caller just returned from
    // `Softdevice::enable`), both pointers are valid for the duration of
    // the call, and `len` is the true length of `name`.
    let ret = unsafe {
        raw::sd_ble_gap_device_name_set(&write_perm, name.as_bytes().as_ptr(), name.len() as u16)
    };
    if ret == raw::NRF_SUCCESS {
        crate::info!("BLE: gap device name set to {}", name.as_str());
    } else {
        crate::warn!("BLE: gap device name set failed err={}", ret);
    }
}

/// How often [`counters_task`] emits its line: the `[TRANSPORT]`
/// cadence (`transport_stats::PERIOD`), so the periodic counter lines
/// interleave predictably in a capture.
const COUNTERS_PERIOD_SECS: u64 = 30;

/// Periodic `BLE_COUNTERS` line on the debug CDC — the
/// [`crate::transport_stats`]-style surface for the #264/#255 counters,
/// which until phase B existed only as atomics nothing printed. A
/// `BLE_TX_DROP` line names each abandoned packet as it happens, but
/// the *absence* of misrouting is a claim about a counter staying 0,
/// and an absence needs a heartbeat to be quotable from a log:
///
/// ```text
/// BLE_COUNTERS packets=<n> dropped=<n> waits=<n> unrouted=<n> links=<n> displaced=<n> refused=<n> route_miss=<n>
/// ```
///
/// `links=` is the number of claimed drain slots — live BLE links.
/// `displaced=` counts links torn down because a newer connection of
/// the same identity took over — the peer dialled in, or our own dial
/// found a link that had stopped answering — and `refused=` counts
/// dials of OURS sent away because that identity's existing link was
/// still answering. Beside a Columba phone both climb: `refused=` for
/// the fallback dials that keep finding the phone's rotated address,
/// `displaced=` once per reconnect the phone itself initiates (#382).
/// `route_miss=` counts routed
/// packets dropped because the peer the core addressed held no live
/// link here (#376); a rising value on a healthy board means the path
/// table outlived a link and the #365 cull should have fired.
/// The two-link acceptance for #255 phase B reads `links=2 unrouted=0`
/// off this line: both slots claimed, and every HVN drain edge still
/// found the link that produced it.
#[embassy_executor::task]
async fn counters_task() -> ! {
    use core::sync::atomic::Ordering;
    loop {
        embassy_time::Timer::after_secs(COUNTERS_PERIOD_SECS).await;
        crate::log::log_fmt(
            "[BLE ] ",
            format_args!(
                "BLE_COUNTERS packets={} dropped={} waits={} unrouted={} links={} displaced={} refused={} route_miss={}",
                BLE_TX_PACKETS.load(Ordering::Relaxed),
                BLE_TX_DROPPED.load(Ordering::Relaxed),
                BLE_TX_DRAIN_WAITS.load(Ordering::Relaxed),
                BLE_TX_DRAIN_UNROUTED.load(Ordering::Relaxed),
                HVN_DRAIN.claimed(),
                columba::BLE_LINKS_DISPLACED.load(Ordering::Relaxed),
                columba::BLE_LINKS_REFUSED.load(Ordering::Relaxed),
                BLE_TX_ROUTE_MISSES.load(Ordering::Relaxed),
            ),
        );
    }
}

/// Bring up S140 + start the BLE task. Peripherals previously owned by
/// MPSL/SDC (RTC0/TIMER0/PPI/RNG/etc.) are kept in the signature for ABI
/// compatibility with the binaries; the SoftDevice claims them
/// internally.
///
/// Returns the enabled SoftDevice, which anything needing a SoftDevice
/// syscall after this point has to hold — the flash writes in
/// [`crate::radio_store`] are the current caller.
///
/// # `columba_enabled`
///
/// The media profile ([`crate::media`]) decides whether this node meshes
/// over BLE at all, and the answer is a **spawn decision here** — which is
/// the acceptance test the #255 phase-A seam was written for, and it
/// held: `false` skips `columba::spawn` and nothing else changes. No
/// advertisement, no scan, no GATT service, no connection; the protocol
/// module is not reached.
///
/// The SoftDevice is enabled either way, and deliberately so. It is not
/// only the BLE stack: `sd_flash_write` is the one legal way to write
/// internal flash once it is enabled, and both persistence store tasks
/// ride on it. A board with `ble=off` that could not persist its own
/// profile could not be put back on BLE, which is the one state this
/// feature must never be able to reach. Enabling the SoftDevice without
/// advertising costs idle current and nothing on the air.
#[allow(clippy::too_many_arguments)]
pub fn init(
    spawner: &Spawner,
    columba_enabled: bool,
    identity_hash: [u8; 16],
    vbus: &'static SoftwareVbusDetect,
    _rtc0: Peri<'static, peripherals::RTC0>,
    _timer0: Peri<'static, peripherals::TIMER0>,
    _temp: Peri<'static, peripherals::TEMP>,
    _ppi_ch19: Peri<'static, peripherals::PPI_CH19>,
    _ppi_ch30: Peri<'static, peripherals::PPI_CH30>,
    _ppi_ch31: Peri<'static, peripherals::PPI_CH31>,
    _ppi_ch17: Peri<'static, peripherals::PPI_CH17>,
    _ppi_ch18: Peri<'static, peripherals::PPI_CH18>,
    _ppi_ch20: Peri<'static, peripherals::PPI_CH20>,
    _ppi_ch21: Peri<'static, peripherals::PPI_CH21>,
    _ppi_ch22: Peri<'static, peripherals::PPI_CH22>,
    _ppi_ch23: Peri<'static, peripherals::PPI_CH23>,
    _ppi_ch24: Peri<'static, peripherals::PPI_CH24>,
    _ppi_ch25: Peri<'static, peripherals::PPI_CH25>,
    _ppi_ch26: Peri<'static, peripherals::PPI_CH26>,
    _ppi_ch27: Peri<'static, peripherals::PPI_CH27>,
    _ppi_ch28: Peri<'static, peripherals::PPI_CH28>,
    _ppi_ch29: Peri<'static, peripherals::PPI_CH29>,
    _rng_periph: Peri<'static, peripherals::RNG>,
) -> &'static Softdevice {
    // Measure first, enable second: the probe leaves the BLE stack
    // uninitialised, so a configuration that would eat our stack is
    // refused before it can.
    let wanted = required_app_ram_base();
    cortex_m::asm::delay(PROBE_SETTLE_CYCLES);
    assert_sd_fits_below_retained(wanted);

    let sd = Softdevice::enable(&sd_config());
    crate::boot_trace::phase(crate::boot_trace::Phase::SdEnabled);
    set_gap_device_name();

    let sd = if columba_enabled {
        columba::spawn(spawner, sd, identity_hash)
    } else {
        crate::media::log_carrier_held_down("ble");
        sd
    };
    spawner.must_spawn(softdevice_task(sd, vbus));
    crate::boot_trace::phase(crate::boot_trace::Phase::BleTask);
    // The outbound fan-out is protocol-neutral machinery, like the
    // drain table it reads: packets in, one copy per live link out. It
    // runs either way: with no protocol task there is no claimed drain
    // slot, so it has nothing to fan out to and idles on the channel.
    spawner.must_spawn(tx_fanout_task());
    spawner.must_spawn(counters_task());

    sd
}
