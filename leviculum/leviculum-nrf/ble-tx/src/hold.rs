//! Hold a peripheral link's drain until the peer can receive
//! (Codeberg #376).
//!
//! The field T114 at 2026-09-09 11:31:18: the first notify on a fresh
//! connection, sent before the central had written the TX CCCD, failed
//! inside the SoftDevice with `sd_error code=13313`
//! (`BLE_ERROR_GATTS_SYS_ATTR_MISSING`, nrf-softdevice-s140
//! bindings.rs:778) and the packet was dropped. A notify before the
//! subscription can never arrive; feeding one to the queue converts a
//! packet into an error line.
//!
//! The policy: the peripheral pump drains nothing — packets or
//! keepalives — until BOTH the CCCD subscription and the identity
//! handshake have happened, whichever is later. Packets queued before
//! that WAIT in the link's queue; they are not dropped. The pump logs
//! `BLE_TX_HELD conn=<h> reason=not-subscribed` once per connection
//! when it actually holds a packet, which [`TxHold::note_held`]
//! arbitrates.
//!
//! Peripheral-side only: the central pump writes commands, which need
//! no subscription, and the central session subscribes before it
//! handshakes anyway (v2.2 §Connection Phase steps 5 and 6).
//!
//! Host-tested here for the usual reason — the interesting state, "a
//! packet arrived between connect and subscribe", needs a phone with
//! deliberate timing to reach on a board.

/// One peripheral connection's drain-readiness state. Owned by the
/// connection's pump, next to its [`crate::TxGap`], and dying with the
/// connection — a fresh link always starts held.
#[derive(Debug, Clone, Copy, Default)]
pub struct TxHold {
    subscribed: bool,
    handshaken: bool,
    held_logged: bool,
}

impl TxHold {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            subscribed: false,
            handshaken: false,
            held_logged: false,
        }
    }

    /// The peer wrote the TX CCCD. `false` re-arms the hold: a central
    /// that unsubscribes mid-session is one that stopped listening, and
    /// notifying it fails exactly like notifying it too early.
    pub fn note_subscription(&mut self, notifications: bool) {
        self.subscribed = notifications;
    }

    /// The identity handshake landed on this link.
    pub fn note_handshake(&mut self) {
        self.handshaken = true;
    }

    /// May the pump hand anything to the notify queue?
    #[must_use]
    pub fn ready(&self) -> bool {
        self.subscribed && self.handshaken
    }

    /// The pump is about to hold a packet: whether to log the
    /// `BLE_TX_HELD` line — `true` exactly once per connection, however
    /// many packets end up waiting.
    pub fn note_held(&mut self) -> bool {
        let first = !self.held_logged;
        self.held_logged = true;
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The #376 field failure, as policy: a packet queued before the
    /// subscription waits (not ready → the pump holds it in the queue)
    /// and drains once the subscription lands.
    #[test]
    fn queued_before_subscription_waits_and_drains_after() {
        let mut hold = TxHold::new();
        hold.note_handshake();
        assert!(
            !hold.ready(),
            "handshake done, CCCD not written: the 11:31:18 notify would \
             fail with sd_error 13313 — the drain must hold"
        );
        hold.note_subscription(true);
        assert!(hold.ready(), "subscription landed: the queue drains now");
    }

    /// #376 item 3, the peer-up announce: the announce a board sends when
    /// a peer's identity handshake completes is queued at exactly the
    /// moment this hold is armed on the peripheral path — handshake done,
    /// CCCD not yet written. It must WAIT, not be dropped.
    ///
    /// This pins the readiness edge the announce is queued into. The
    /// waiting itself is the pump's `while !tx_hold.get().ready() {
    /// tx_ready.wait().await; }` in `leviculum-nrf/src/ble/columba.rs`,
    /// which takes the packet off the queue and blocks on this state
    /// rather than handing it to `sd_ble_gatts_hvx`: the announce is the
    /// first packet on a fresh link, which is precisely the packet the
    /// field lost to sd_error 13313.
    #[test]
    fn a_peer_up_announce_is_queued_into_a_held_link_and_waits() {
        let mut hold = TxHold::new();

        // The peer-up edge: the identity handshake landed, so the core is
        // told the peer exists and the announce is built and queued.
        hold.note_handshake();
        assert!(
            !hold.ready(),
            "the announce arrives before the CCCD write: notifying now is \
             the 13313 failure, so the pump must hold it"
        );
        assert!(
            hold.note_held(),
            "the first held packet on this connection says so once"
        );
        assert!(
            !hold.note_held(),
            "and every further held packet is silent, so a phone that \
             subscribes late cannot flood the log"
        );

        // The phone subscribes: the announce that has been waiting goes.
        hold.note_subscription(true);
        assert!(hold.ready());
    }

    /// The other order — the spec's "or the identity handshake, if that
    /// is the later of the two".
    #[test]
    fn a_subscription_without_the_handshake_still_holds() {
        let mut hold = TxHold::new();
        hold.note_subscription(true);
        assert!(!hold.ready(), "subscribed but not handshaken: still held");
        hold.note_handshake();
        assert!(hold.ready());
    }

    #[test]
    fn a_fresh_connection_starts_held() {
        assert!(!TxHold::new().ready());
    }

    #[test]
    fn an_unsubscribe_re_arms_the_hold() {
        let mut hold = TxHold::new();
        hold.note_handshake();
        hold.note_subscription(true);
        assert!(hold.ready());
        hold.note_subscription(false);
        assert!(
            !hold.ready(),
            "CCCD written back to 0: notifying now fails like notifying \
             too early — hold again"
        );
    }

    /// `BLE_TX_HELD` is once per connection, not once per held packet:
    /// a queue of N packets held across the subscription gap must not
    /// write N lines.
    #[test]
    fn the_held_line_is_logged_once_per_connection() {
        let mut hold = TxHold::new();
        assert!(hold.note_held(), "first held packet logs");
        assert!(!hold.note_held(), "second does not");
        hold.note_subscription(true);
        hold.note_handshake();
        hold.note_subscription(false);
        assert!(
            !hold.note_held(),
            "a later re-hold on the same connection stays silent too"
        );
    }
}
