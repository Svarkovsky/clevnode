//! The per-connection inter-packet transmit gap (Codeberg #376).
//!
//! Born as a bench instrument for the direct-announce loss, now the
//! default behaviour: the drain leaves at least [`DEFAULT_TX_GAP_MS`]
//! between the **last fragment of one packet** and the **first fragment
//! of the next packet** on the same connection handle. The operator's
//! knob (`TYPE_BLE_TX_GAP`, `--set-ble-tx-gap`) stays for measurement
//! and overrides the default — `0` disables the gap entirely — and a
//! reset restores the default, so a bench cannot leave a board silently
//! unpaced (or mispaced) after the session that paced it.
//!
//! Interface-layer only, like the LoRa transmit spacing (#345, the
//! `leviculum-tx-spacing` crate this is the BLE sibling of): the fan-out
//! and the core never learn a gap exists. Per connection rather than
//! global, like everything else in this crate ([`crate::drain`] for why):
//! link A's pacing must not hold link B's traffic.
//!
//! The arithmetic lives here rather than in the firmware pump for the
//! usual reason — a policy that needs hardware to be exercised is a
//! policy that is never exercised — and because the failure mode is
//! subtle: a gap measured from the *start* of the previous packet, or
//! one that also spaces the first packet of a connection, would silently
//! shrink or misplace the very window the bench is trying to open.

/// The compiled default inter-packet gap, in milliseconds.
///
/// 100 ms is the MEASURED value, not a derived one: the 2026-09-09 desk
/// run on #376 showed a Columba phone one hop from two boards losing
/// the first of two fragmented packets whose fragments arrived back to
/// back on one connection, and a 100 ms gap between packets on the link
/// let both through — both boards at one hop, proofs direct. A smaller
/// value may well suffice (one to two connection intervals, 30 to
/// 50 ms, is the plausible floor) but was not measured, so the default
/// states what the bench proved and nothing sharper.
pub const DEFAULT_TX_GAP_MS: u16 = 100;

/// Resolve the gap a pump serves: the operator's override when one was
/// set this boot (`0` = no gap at all), the compiled
/// [`DEFAULT_TX_GAP_MS`] otherwise. Every pump on both stacks — the
/// peripheral notify path and the central write path, firmware and
/// lnsd — resolves through here, so the default cannot drift between
/// them.
#[must_use]
pub const fn effective_tx_gap_ms(override_ms: Option<u16>) -> u16 {
    match override_ms {
        Some(ms) => ms,
        None => DEFAULT_TX_GAP_MS,
    }
}

/// One connection's gap state: when its previous packet finished.
///
/// Owned by that connection's pump task, next to its defragmenter and
/// keepalive clock, and dying with the connection — a new link starts
/// with no history and its first packet is never deferred.
#[derive(Debug, Default)]
pub struct TxGap {
    /// When the last fragment of the previous packet was handed off,
    /// in the pump's millisecond timebase. `None` until a first packet
    /// has gone out on this connection.
    last_packet_end_ms: Option<u64>,
}

impl TxGap {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_packet_end_ms: None,
        }
    }

    /// How long the next packet's first fragment must still be held
    /// back, in milliseconds. `0` is "send now": no gap configured, no
    /// previous packet on this connection, or the gap already elapsed
    /// while the queue was idle — an idle link pays nothing.
    #[must_use]
    pub fn wait_ms(&self, now_ms: u64, gap_ms: u16) -> u64 {
        if gap_ms == 0 {
            return 0;
        }
        match self.last_packet_end_ms {
            None => 0,
            Some(end_ms) => end_ms
                .saturating_add(u64::from(gap_ms))
                .saturating_sub(now_ms),
        }
    }

    /// The last fragment of a packet has been handed off; the next
    /// packet's gap is measured from here.
    ///
    /// The pump calls this for packets only, never for keepalives: the
    /// knob is specified packet-to-packet, and a keepalive that slid the
    /// window would turn a 20 ms experiment into "20 ms after the most
    /// recent keepalive", which is not the quantity being swept.
    pub fn packet_done(&mut self, now_ms: u64) {
        self.last_packet_end_ms = Some(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_core::envelope::BLE_TX_GAP_MAX_MS;

    #[test]
    fn zero_gap_imposes_nothing_even_with_history() {
        let mut gap = TxGap::new();
        gap.packet_done(1_000);
        assert_eq!(gap.wait_ms(1_000, 0), 0, "back-to-back at gap 0");
    }

    #[test]
    fn the_default_gap_applies_when_no_override_is_set() {
        assert_eq!(effective_tx_gap_ms(None), DEFAULT_TX_GAP_MS);
        let mut gap = TxGap::new();
        gap.packet_done(1_000);
        assert_eq!(
            gap.wait_ms(1_000, effective_tx_gap_ms(None)),
            u64::from(DEFAULT_TX_GAP_MS),
            "a fleet that never touches the knob is paced at the default"
        );
    }

    #[test]
    fn the_override_wins_over_the_default() {
        assert_eq!(effective_tx_gap_ms(Some(20)), 20);
        let mut gap = TxGap::new();
        gap.packet_done(1_000);
        assert_eq!(gap.wait_ms(1_000, effective_tx_gap_ms(Some(20))), 20);
    }

    #[test]
    fn a_zero_override_disables_the_gap() {
        assert_eq!(effective_tx_gap_ms(Some(0)), 0);
        let mut gap = TxGap::new();
        gap.packet_done(1_000);
        assert_eq!(
            gap.wait_ms(1_000, effective_tx_gap_ms(Some(0))),
            0,
            "the knob at 0 restores the unpaced pre-default behaviour"
        );
    }

    #[test]
    fn the_first_packet_of_a_connection_is_never_deferred() {
        let gap = TxGap::new();
        assert_eq!(gap.wait_ms(0, 500), 0);
        assert_eq!(gap.wait_ms(123_456, BLE_TX_GAP_MAX_MS), 0);
    }

    #[test]
    fn a_packet_inside_the_gap_waits_exactly_the_remainder() {
        let mut gap = TxGap::new();
        gap.packet_done(1_000);
        assert_eq!(gap.wait_ms(1_000, 20), 20, "immediately after: full gap");
        assert_eq!(gap.wait_ms(1_015, 20), 5, "15 ms later: the remainder");
    }

    #[test]
    fn the_gap_boundary_itself_is_send_now() {
        let mut gap = TxGap::new();
        gap.packet_done(1_000);
        assert_eq!(gap.wait_ms(1_020, 20), 0, "exactly at end + gap");
    }

    #[test]
    fn an_idle_link_pays_nothing_for_a_gap_that_already_elapsed() {
        let mut gap = TxGap::new();
        gap.packet_done(1_000);
        assert_eq!(gap.wait_ms(60_000, 500), 0);
    }

    #[test]
    fn the_gap_is_measured_from_the_end_of_the_previous_packet() {
        // The subtle wrong version: measuring from when the previous
        // packet *started*. A multi-fragment packet that took 30 ms to
        // drain would then eat its own gap. `packet_done` is called at
        // the end, so the window opens after the last fragment — this
        // test pins the contract by moving the end, not the start.
        let mut gap = TxGap::new();
        gap.packet_done(1_030); // previous packet finished at 1030
        assert_eq!(gap.wait_ms(1_040, 20), 10);
    }

    #[test]
    fn each_packet_slides_the_window_forward() {
        let mut gap = TxGap::new();
        gap.packet_done(1_000);
        gap.packet_done(2_000);
        assert_eq!(
            gap.wait_ms(2_010, 50),
            40,
            "the window is the latest packet's, not the first's"
        );
    }

    #[test]
    fn the_largest_acceptable_gap_does_not_overflow() {
        let mut gap = TxGap::new();
        gap.packet_done(u64::MAX - 1);
        // Saturating arithmetic: a pathological timebase near the top of
        // u64 must clamp (end + gap saturates to u64::MAX, so one
        // millisecond remains), not wrap into a bogus wait.
        assert_eq!(gap.wait_ms(u64::MAX - 1, BLE_TX_GAP_MAX_MS), 1);
    }
}
