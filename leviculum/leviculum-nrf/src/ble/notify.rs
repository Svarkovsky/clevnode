//! Pushing one packet's fragments through the SoftDevice's notification
//! queue — the thin driver half of Codeberg #264.
//!
//! Protocol-neutral: everything here is expressed in fragments, a GATT
//! value handle and a connection, and knows nothing about what the bytes
//! mean. A second BLE carrier reuses it as-is.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::select::{select, Either};
use embassy_time::Timer;
use leviculum_ble_tx::{
    AbortReason, Action, DrainSlot, Event, NotifyOutcome, PacketTx, TxPktLine, DRAIN_WAIT_MS,
};
use nrf_softdevice::ble::gatt_server::{self, NotifyValueError};
use nrf_softdevice::ble::Connection;
use nrf_softdevice::RawError;

/// Packets whose every fragment reached the SoftDevice's notification
/// queue.
pub static BLE_TX_PACKETS: AtomicU32 = AtomicU32::new(0);
/// Packets abandoned part-way through their fragments. Before #264 this
/// was the common case and it incremented nothing at all: the loop could
/// not tell a dropped fragment from a sent one.
pub static BLE_TX_DROPPED: AtomicU32 = AtomicU32::new(0);
/// Times the outbound loop waited for the HVN queue to drain. On a
/// one-deep queue (the S140 default) this tracks "fragments beyond the
/// first", so it is the direct measure of multi-fragment traffic.
pub static BLE_TX_DRAIN_WAITS: AtomicU32 = AtomicU32::new(0);
/// Drain edges that reached no waiter: the SoftDevice reported a
/// completed notification for a connection with no claim in
/// [`crate::ble::HVN_DRAIN`].
///
/// Expected to stay 0. It is not 0 when a connection completes a
/// notification after its pump released the slot, and it must never be
/// large: a rising count means edges are being dropped that some link
/// was waiting for, i.e. the per-connection routing is mis-keyed and the
/// `waits=` numbers are being paid for in [`DRAIN_WAIT_MS`] timeouts.
pub static BLE_TX_DRAIN_UNROUTED: AtomicU32 = AtomicU32::new(0);

/// Push one packet's fragments through the SoftDevice notification
/// queue, in order and exactly once, and report it when that fails.
///
/// Every decision — retry the same fragment, abort, give up on the
/// budget — belongs to [`PacketTx`] and is unit-tested on the host; this
/// function only performs the actions and reports the outcome.
///
/// The wait is on `BLE_GATTS_EVT_HVN_TX_COMPLETE` for **this**
/// connection (surfaced by `nrf-softdevice` as
/// `Server::on_notify_tx_complete`, routed to the caller's
/// [`DrainSlot`] by [`crate::ble::HVN_DRAIN`]), never on a guessed
/// interval, and it is bounded by [`DRAIN_WAIT_MS`] so a peer that
/// stopped listening cannot wedge the outbound task.
///
/// `fragment` yields fragment `index`; the caller keeps the buffers, so
/// nothing is copied or allocated here.
pub async fn notify_fragments<'a, F>(
    conn: &Connection,
    drain: &DrainSlot,
    handle: u16,
    fragment_count: usize,
    fragment: F,
    kind: &str,
    packet_len: usize,
) where
    F: Fn(usize) -> &'a [u8],
{
    // A drain edge still pending here belongs to a fragment of an
    // earlier packet on this same connection and has already been paid
    // for. Clearing it keeps the first wait of this packet an honest
    // measurement.
    drain.reset();

    let (mut tx, mut action) = PacketTx::start(fragment_count);
    loop {
        match action {
            Action::Send { index } => {
                let outcome = match gatt_server::notify_value(conn, handle, fragment(index)) {
                    Ok(()) => NotifyOutcome::Sent,
                    // The queue is full; the fragment was NOT taken.
                    Err(NotifyValueError::Raw(RawError::Resources)) => NotifyOutcome::QueueFull,
                    Err(NotifyValueError::Disconnected) => NotifyOutcome::Disconnected,
                    Err(NotifyValueError::Raw(err)) => NotifyOutcome::Failed(u32::from(err)),
                };
                action = tx.step(Event::Notify(outcome));
            }
            Action::AwaitDrain { .. } => {
                let event = match select(drain.wait(), Timer::after_millis(DRAIN_WAIT_MS)).await {
                    Either::First(()) => Event::Drained,
                    Either::Second(()) => Event::WaitTimedOut,
                };
                action = tx.step(event);
            }
            Action::Done => {
                BLE_TX_PACKETS.fetch_add(1, Ordering::Relaxed);
                BLE_TX_DRAIN_WAITS.fetch_add(tx.drain_waits(), Ordering::Relaxed);
                log_tx_pkt(conn, packet_len, fragment_count, tx.fragments_sent());
                return;
            }
            Action::Abort { index, reason } => {
                log_tx_pkt(conn, packet_len, fragment_count, tx.fragments_sent());
                report_tx_drop(
                    DropSite {
                        kind,
                        packet_len,
                        conn: conn.handle().unwrap_or(u16::MAX),
                    },
                    index,
                    fragment_count,
                    &tx,
                    reason.as_str(),
                    reason.code(),
                );
                // A packet abandoned after an accepted fragment leaves
                // the peer's reassembler holding a torn head, and the
                // wire protocol has no abort marker: the peer keeps the
                // head for its full reassembly window and completes it
                // with the NEXT packet's tail (#255 — Columba glued a
                // torn announce head onto the following report's END
                // fragment and rejected the result as an announce with
                // an invalid signature). The only in-band reset of the
                // peer's per-connection reassembly state is dropping
                // the connection; a reconnect is cheaper than a poisoned
                // stream. Pointless after `Disconnected` — the
                // connection, and with it the peer's partial state, is
                // already gone.
                if tx.torn() && !matches!(reason, AbortReason::Disconnected) {
                    let _ = conn.disconnect();
                    crate::log::log_fmt(
                        "[BLE ] ",
                        format_args!(
                            "BLE_TX_RESYNC action=disconnect frag={} of={} sent={}",
                            index,
                            fragment_count,
                            tx.fragments_sent(),
                        ),
                    );
                }
                return;
            }
            // Unreachable: every event fed above answers the action just
            // performed. Reported rather than swallowed — a silently
            // dropped packet is the exact bug this function removes.
            Action::Nothing => {
                log_tx_pkt(conn, packet_len, fragment_count, tx.fragments_sent());
                report_tx_drop(
                    DropSite {
                        kind,
                        packet_len,
                        conn: conn.handle().unwrap_or(u16::MAX),
                    },
                    tx.fragments_sent(),
                    fragment_count,
                    &tx,
                    "internal",
                    0,
                );
                return;
            }
        }
    }
}

/// One `BLE_TX_PKT` line per multi-fragment packet handed to a link
/// (#373), at the terminal action, success and failure alike — the line
/// [`TxPktLine`]'s docs and host test pin. Single-fragment packets and
/// keepalives stay unlogged.
fn log_tx_pkt(conn: &Connection, packet_len: usize, frags: usize, sent: usize) {
    if frags <= 1 {
        return;
    }
    crate::log::log_fmt(
        "[BLE ] ",
        format_args!(
            "{}",
            TxPktLine {
                conn: conn.handle().unwrap_or(u16::MAX),
                len: packet_len,
                frags,
                sent,
            }
        ),
    );
}

/// Emit the structured drop event and bump the counters.
///
/// Format per `docs/src/structured-event-logs.md`: `NAME key=value …
/// t=<ms>`, one line, scalar values, no whitespace inside a value — so
/// `grep BLE_TX_DROP` over a captured debug-port log is a usable
/// measurement of how much BLE traffic never left the node.
///
/// The trailing `t=` is not written here: `log_fmt` appends it to every
/// line, from the same `Instant::now()`. This call site used to write
/// its own, which after that change rendered the field twice.
/// What identifies one drop line beyond the state machine's own
/// counters: the traffic kind, the packet's size, and the SoftDevice
/// connection handle of the link the TX targeted (`conn=` — the fan-out
/// sends one copy per live link, and the 2026-09-08 desk log could not
/// attribute an `sd_error` without it, #365).
struct DropSite<'a> {
    kind: &'a str,
    packet_len: usize,
    conn: u16,
}

fn report_tx_drop(
    site: DropSite<'_>,
    index: usize,
    fragment_count: usize,
    tx: &PacketTx,
    reason: &str,
    code: u32,
) {
    let dropped = BLE_TX_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
    BLE_TX_DRAIN_WAITS.fetch_add(tx.drain_waits(), Ordering::Relaxed);
    crate::log::log_fmt(
        "[BLE ] ",
        format_args!(
            "BLE_TX_DROP kind={} len={} frag={} of={} sent={} reason={} code={} conn={} waits={} dropped={}",
            site.kind,
            site.packet_len,
            index,
            fragment_count,
            tx.fragments_sent(),
            reason,
            code,
            site.conn,
            tx.drain_waits(),
            dropped,
        ),
    );
}
