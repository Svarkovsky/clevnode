//! Routing the SoftDevice's HVN drain edge to the link that produced it.
//!
//! `BLE_GATTS_EVT_HVN_TX_COMPLETE` says "a notification queue slot is
//! free again", and the queue it refers to is **per connection**
//! (`ble_gatts_conn_cfg_t::hvn_tx_queue_size` is a per-connection
//! configuration). The outbound fragment pump waits on that edge instead
//! of guessing an interval (Codeberg #264), so the edge has to reach the
//! waiter for *that* connection and no other.
//!
//! One global [`Signal`] is indistinguishable from a correct
//! implementation while exactly one connection exists, and wrong the
//! moment a second one does. A `Signal` holds one value and the first
//! waiter to poll takes it, so link A's drain is consumed by whichever
//! of the two fragment pumps happens to poll first: link B re-offers a
//! fragment into a queue that is still full and books a `waits=` that
//! was never its own, while link A — whose queue actually has room —
//! learns nothing and burns a full [`DRAIN_WAIT_MS`] before it re-offers
//! anyway. The `waits=` counter is the honest measurement #264 exists to
//! provide; this is the bug that would make it lie, removed *before* the
//! central role makes it reachable (#255 phase A).
//!
//! [`DRAIN_WAIT_MS`]: crate::DRAIN_WAIT_MS
//!
//! The routing is a handful of atomics and a fixed slot table, i.e.
//! exactly the kind of thing that is never exercised if it only exists
//! inside the firmware crate. It lives here, next to [`PacketTx`], and
//! the two-waiter property is asserted on the host.
//!
//! [`PacketTx`]: crate::PacketTx

use core::sync::atomic::{AtomicU16, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

/// `BLE_CONN_HANDLE_INVALID`, reused here as "this slot is free".
///
/// The SoftDevice never hands out `0xFFFF` as a live connection handle —
/// it is the value it uses to mean "no connection" — so it cannot
/// collide with a handle a caller passes in. [`DrainRouter::claim`]
/// rejects it explicitly all the same, because a caller that turned a
/// `None` handle into `0xFFFF` would otherwise claim the free slot it
/// happens to be looking at and then receive every other link's drains.
pub const NO_CONN_HANDLE: u16 = 0xFFFF;

/// One connection's drain edge.
///
/// Handed out by [`DrainRouter::claim`] for the lifetime of a
/// connection and returned by [`DrainRouter::release`]. The fragment
/// pump holds the reference and waits on it; the `Server` callback
/// reaches the same slot by handle.
pub struct DrainSlot {
    /// The connection this slot belongs to, or [`NO_CONN_HANDLE`].
    handle: AtomicU16,
    signal: Signal<CriticalSectionRawMutex, ()>,
}

impl DrainSlot {
    const fn new() -> Self {
        Self {
            handle: AtomicU16::new(NO_CONN_HANDLE),
            signal: Signal::new(),
        }
    }

    /// Wait for the next drain edge on this connection.
    pub async fn wait(&self) {
        self.signal.wait().await;
    }

    /// Drop a drain edge that is already pending.
    ///
    /// A packet clears the slot before its first fragment: an edge still
    /// sitting here belongs to a fragment of an *earlier* packet and has
    /// already been paid for, so leaving it would make the first wait of
    /// this packet return immediately and report a drain that did not
    /// happen.
    pub fn reset(&self) {
        self.signal.reset();
    }

    /// The connection handle this slot is claimed for, `None` when free.
    #[must_use]
    pub fn conn_handle(&self) -> Option<u16> {
        match self.handle.load(Ordering::Acquire) {
            NO_CONN_HANDLE => None,
            handle => Some(handle),
        }
    }
}

/// A fixed table of [`DrainSlot`]s, one per concurrent link.
///
/// `N` is sized by the SoftDevice's `conn_count`, not by anything this
/// module needs; a claim beyond `N` fails rather than aliasing two
/// connections onto one slot, because aliasing is precisely the failure
/// mode being removed.
pub struct DrainRouter<const N: usize> {
    slots: [DrainSlot; N],
}

impl<const N: usize> Default for DrainRouter<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> DrainRouter<N> {
    /// An empty table. `const` so the firmware can hold it in a `static`
    /// without a `StaticCell` and without an init ordering question.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { DrainSlot::new() }; N],
        }
    }

    /// Reserve the slot for `handle`, or return the one already
    /// reserved for it.
    ///
    /// Returns `None` when the table is full or `handle` is
    /// [`NO_CONN_HANDLE`]. A full table means more live connections than
    /// `conn_count` allows, which the SoftDevice does not produce; the
    /// caller reports it rather than sharing a slot.
    ///
    /// The returned slot starts with no pending edge.
    pub fn claim(&self, handle: u16) -> Option<&DrainSlot> {
        self.claim_indexed(handle).map(|(_, slot)| slot)
    }

    /// [`claim`](Self::claim), plus the slot's index in the table.
    ///
    /// The index is what makes a claim double as the link's *identity*
    /// for anything else sized `[_; N]` alongside this table — the
    /// firmware's per-link outgoing queues are the caller (#255 phase
    /// B): a connection claims one slot for its whole lifetime, so
    /// "slot `i` is claimed" and "link `i` is live" are the same fact,
    /// and a second registry that could drift from this one is not
    /// built. The index is stable from claim to release.
    pub fn claim_indexed(&self, handle: u16) -> Option<(usize, &DrainSlot)> {
        if handle == NO_CONN_HANDLE {
            return None;
        }
        // A re-claim of a live handle is the same slot, not a second
        // one: two slots for one connection would split its drain edges
        // between two waiters at random.
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.handle.load(Ordering::Acquire) == handle {
                return Some((index, slot));
            }
        }
        for (index, slot) in self.slots.iter().enumerate() {
            if slot
                .handle
                .compare_exchange(NO_CONN_HANDLE, handle, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // A handle the SoftDevice reused can carry an edge left
                // over from the connection that had it before.
                slot.signal.reset();
                return Some((index, slot));
            }
        }
        None
    }

    /// The connection handle claimed at `index`, `None` when that slot
    /// is free or `index` is beyond the table. The fan-out over live
    /// links iterates this.
    #[must_use]
    pub fn handle_at(&self, index: usize) -> Option<u16> {
        self.slots.get(index).and_then(DrainSlot::conn_handle)
    }

    /// Free the slot held by `handle`. Idempotent; unknown handles are
    /// ignored.
    pub fn release(&self, handle: u16) {
        if handle == NO_CONN_HANDLE {
            return;
        }
        for slot in &self.slots {
            if slot
                .handle
                .compare_exchange(handle, NO_CONN_HANDLE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                slot.signal.reset();
                return;
            }
        }
    }

    /// Deliver one drain edge to the link that produced it.
    ///
    /// Returns whether a slot took it. `false` means the edge belonged
    /// to a connection with no outstanding claim — a link that never
    /// sent a fragment, or one already torn down — and dropping it is
    /// correct: no waiter exists that the edge answers.
    pub fn drained(&self, handle: u16) -> bool {
        if handle == NO_CONN_HANDLE {
            return false;
        }
        for slot in &self.slots {
            if slot.handle.load(Ordering::Acquire) == handle {
                slot.signal.signal(());
                return true;
            }
        }
        false
    }

    /// How many links the table can hold.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// How many slots are currently claimed. For the `[BLE ]` counters
    /// and the tests; not on any hot path.
    #[must_use]
    pub fn claimed(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.conn_handle().is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    /// Poll a future once with a waker that does nothing. Every property
    /// here is about *which* signal a poll observes, so no executor is
    /// needed and none is used: a manual poll is the whole test.
    macro_rules! poll_once {
        ($fut:expr) => {{
            let mut cx = Context::from_waker(Waker::noop());
            $fut.as_mut().poll(&mut cx)
        }};
    }

    /// Positive control for the harness AND the red state this module
    /// fixes: with one shared `Signal` — `ble.rs`'s `HVN_TX_DRAINED`
    /// before #255 phase A — a drain edge belongs to whoever polls
    /// first, not to the link whose queue actually drained. Without this
    /// test, "the other waiter stayed pending" below could pass because
    /// the poll harness never delivers anything at all.
    #[test]
    fn control_one_global_signal_hands_a_drain_to_whoever_polls_first() {
        let global: Signal<CriticalSectionRawMutex, ()> = Signal::new();
        let mut wait_a = pin!(global.wait());
        let mut wait_b = pin!(global.wait());
        assert_eq!(poll_once!(wait_a), Poll::Pending);
        assert_eq!(poll_once!(wait_b), Poll::Pending);

        // Link A's HVN queue drained.
        global.signal(());

        assert_eq!(
            poll_once!(wait_b),
            Poll::Ready(()),
            "link B took an edge that was link A's"
        );
        assert_eq!(
            poll_once!(wait_a),
            Poll::Pending,
            "and link A, whose queue actually drained, is still waiting"
        );
    }

    /// The property: a drain edge is consumable only by the link that
    /// produced it. Link B is polled *first* here, precisely because
    /// theft — not a spurious wake — is what the control above shows a
    /// global signal doing. At `conn_count = 2` this is the difference
    /// between a `waits=` count that measures B's own queue and one that
    /// counts A's traffic, and between A re-offering its fragment at
    /// once and burning a full [`crate::DRAIN_WAIT_MS`] first (#264
    /// honest-measurement property, #255 phase A).
    #[test]
    fn a_drain_on_one_link_cannot_be_consumed_by_the_other_links_waiter() {
        let router: DrainRouter<4> = DrainRouter::new();
        let link_a = router.claim(0x0000).expect("slot for link A");
        let link_b = router.claim(0x0001).expect("slot for link B");

        let mut wait_a = pin!(link_a.wait());
        let mut wait_b = pin!(link_b.wait());
        assert_eq!(poll_once!(wait_a), Poll::Pending);
        assert_eq!(poll_once!(wait_b), Poll::Pending);

        assert!(router.drained(0x0000), "link A had a claim");

        assert_eq!(
            poll_once!(wait_b),
            Poll::Pending,
            "link B must not consume link A's drain"
        );
        assert_eq!(
            poll_once!(wait_a),
            Poll::Ready(()),
            "and it is still there for link A"
        );

        // …and B's own drain still reaches it.
        assert!(router.drained(0x0001));
        assert_eq!(poll_once!(wait_b), Poll::Ready(()));
    }

    /// The single-connection case, which is what ships today: one claim,
    /// one waiter, every drain delivered. Byte-for-byte the old
    /// behaviour, and the reason phase A can land before phase B.
    #[test]
    fn a_single_link_receives_every_one_of_its_drains() {
        let router: DrainRouter<4> = DrainRouter::new();
        let link = router.claim(7).expect("slot");
        for _ in 0..3 {
            let mut wait = pin!(link.wait());
            assert_eq!(poll_once!(wait), Poll::Pending);
            assert!(router.drained(7));
            assert_eq!(poll_once!(wait), Poll::Ready(()));
        }
    }

    #[test]
    fn a_drain_for_an_unclaimed_handle_is_dropped_not_broadcast() {
        let router: DrainRouter<4> = DrainRouter::new();
        let link = router.claim(1).expect("slot");
        let mut wait = pin!(link.wait());
        assert_eq!(poll_once!(wait), Poll::Pending);

        assert!(!router.drained(2), "no slot holds handle 2");

        assert_eq!(
            poll_once!(wait),
            Poll::Pending,
            "an unroutable edge must not fall back to waking someone"
        );
    }

    #[test]
    fn claiming_the_same_handle_twice_yields_the_same_slot() {
        let router: DrainRouter<2> = DrainRouter::new();
        let first = router.claim(3).expect("slot");
        let second = router.claim(3).expect("same slot");
        assert!(core::ptr::eq(first, second));
        assert_eq!(router.claimed(), 1);
    }

    #[test]
    fn a_released_slot_comes_back_free_and_edgeless() {
        let router: DrainRouter<1> = DrainRouter::new();
        let link = router.claim(4).expect("slot");
        assert!(router.drained(4));
        router.release(4);
        assert_eq!(router.claimed(), 0);

        // Same slot, new connection: the stale edge must not carry over,
        // or the new link's first wait returns without a drain.
        let reused = router.claim(5).expect("slot");
        assert!(core::ptr::eq(link, reused));
        let mut wait = pin!(reused.wait());
        assert_eq!(poll_once!(wait), Poll::Pending);
    }

    #[test]
    fn a_full_table_refuses_rather_than_aliasing_two_links() {
        let router: DrainRouter<2> = DrainRouter::new();
        assert!(router.claim(1).is_some());
        assert!(router.claim(2).is_some());
        assert!(router.claim(3).is_none(), "capacity is 2");
        assert_eq!(router.claimed(), 2);
        assert_eq!(router.capacity(), 2);
    }

    #[test]
    fn the_invalid_handle_is_neither_claimable_nor_routable() {
        let router: DrainRouter<2> = DrainRouter::new();
        assert!(router.claim(NO_CONN_HANDLE).is_none());
        assert_eq!(router.claimed(), 0, "the free marker must stay free");
        assert!(!router.drained(NO_CONN_HANDLE));
    }

    /// The two-slot traffic case #255 phase B makes real: two links,
    /// each pushing a two-fragment packet through its own [`PacketTx`]
    /// on a one-deep HVN queue, drains interleaved. Every wait must be
    /// paid for by the link's OWN drain and booked to its own
    /// `waits=` counter — the composed form of the routing property,
    /// with the actual #264 state machine in the loop instead of bare
    /// waiters.
    ///
    /// [`PacketTx`]: crate::PacketTx
    #[test]
    fn two_links_interleaved_traffic_each_pays_only_its_own_waits() {
        use crate::{Action, Event, NotifyOutcome, PacketTx};

        let router: DrainRouter<4> = DrainRouter::new();
        let link_a = router.claim(0x0000).expect("slot for link A");
        let link_b = router.claim(0x0001).expect("slot for link B");

        // Both links: fragment 0 accepted, fragment 1 refused (the
        // S140's one-deep queue), so both machines ask to await drain.
        let advance = |tx: &mut PacketTx, action: Action, queue_full: bool| match action {
            Action::Send { index } => {
                let outcome = if queue_full && index > 0 {
                    NotifyOutcome::QueueFull
                } else {
                    NotifyOutcome::Sent
                };
                tx.step(Event::Notify(outcome))
            }
            other => other,
        };

        let (mut tx_a, mut act_a) = PacketTx::start(2);
        let (mut tx_b, mut act_b) = PacketTx::start(2);
        act_a = advance(&mut tx_a, act_a, true); // frag 0 sent
        act_b = advance(&mut tx_b, act_b, true);
        act_a = advance(&mut tx_a, act_a, true); // frag 1 refused
        act_b = advance(&mut tx_b, act_b, true);
        assert!(matches!(act_a, Action::AwaitDrain { index: 1 }));
        assert!(matches!(act_b, Action::AwaitDrain { index: 1 }));

        let mut wait_a = pin!(link_a.wait());
        let mut wait_b = pin!(link_b.wait());
        assert_eq!(poll_once!(wait_a), Poll::Pending);
        assert_eq!(poll_once!(wait_b), Poll::Pending);

        // Link B's queue drains first. A stays pending, B's machine
        // re-offers fragment 1 and completes.
        assert!(router.drained(0x0001));
        assert_eq!(poll_once!(wait_a), Poll::Pending, "not link A's drain");
        assert_eq!(poll_once!(wait_b), Poll::Ready(()));
        act_b = tx_b.step(Event::Drained);
        assert!(matches!(act_b, Action::Send { index: 1 }));
        act_b = advance(&mut tx_b, act_b, false);
        assert!(matches!(act_b, Action::Done));

        // Now link A's. Same completion, and each side booked exactly
        // the one wait its own queue caused.
        assert!(router.drained(0x0000));
        assert_eq!(poll_once!(wait_a), Poll::Ready(()));
        act_a = tx_a.step(Event::Drained);
        assert!(matches!(act_a, Action::Send { index: 1 }));
        act_a = advance(&mut tx_a, act_a, false);
        assert!(matches!(act_a, Action::Done));

        assert_eq!(tx_a.drain_waits(), 1, "A paid for A's queue");
        assert_eq!(tx_b.drain_waits(), 1, "B paid for B's queue");
    }

    /// The index a claim hands out is the link's identity for anything
    /// sized alongside the table (the per-link outgoing queues, #255
    /// phase B): stable across re-claims, distinct across links, dead
    /// after release.
    #[test]
    fn the_claim_index_is_stable_distinct_and_dies_with_the_release() {
        let router: DrainRouter<4> = DrainRouter::new();
        let (idx_a, _) = router.claim_indexed(0x10).expect("slot A");
        let (idx_b, _) = router.claim_indexed(0x11).expect("slot B");
        assert_ne!(idx_a, idx_b);
        assert_eq!(router.handle_at(idx_a), Some(0x10));
        assert_eq!(router.handle_at(idx_b), Some(0x11));

        let (again, _) = router.claim_indexed(0x10).expect("re-claim");
        assert_eq!(again, idx_a, "a re-claim is the same slot");

        router.release(0x10);
        assert_eq!(router.handle_at(idx_a), None, "released slot reads free");
        assert_eq!(router.handle_at(idx_b), Some(0x11), "the other lives on");
        assert_eq!(router.handle_at(999), None, "beyond the table is free");
    }

    /// The #372 fan-out view at full occupancy: with all four slots
    /// claimed, `handle_at` names every live link (that iteration IS
    /// the firmware's per-link queue fan-out), each slot's drain edge
    /// still reaches only its own waiter, and a release makes exactly
    /// that slot disappear from the fan-out.
    #[test]
    fn four_claims_fan_out_and_drain_independently() {
        let router: DrainRouter<4> = DrainRouter::new();
        let handles = [0x10u16, 0x11, 0x12, 0x13];
        let slots: Vec<&DrainSlot> = handles
            .iter()
            .map(|&h| router.claim(h).expect("slot"))
            .collect();
        let live: Vec<Option<u16>> = (0..4).map(|i| router.handle_at(i)).collect();
        assert_eq!(live, handles.iter().copied().map(Some).collect::<Vec<_>>());

        // One drain, one waiter: slot 2's edge wakes slot 2 alone.
        let mut waits: Vec<_> = slots.iter().map(|s| Box::pin(s.wait())).collect();
        for w in waits.iter_mut() {
            assert_eq!(poll_once!(w), Poll::Pending);
        }
        assert!(router.drained(0x12));
        for (i, w) in waits.iter_mut().enumerate() {
            let expected = if i == 2 {
                Poll::Ready(())
            } else {
                Poll::Pending
            };
            assert_eq!(poll_once!(w), expected, "slot {i}");
        }

        // A released link leaves the fan-out; the rest stay.
        router.release(0x11);
        assert_eq!(router.handle_at(1), None);
        assert_eq!(router.claimed(), 3);
        assert_eq!(router.handle_at(0), Some(0x10));
        assert_eq!(router.handle_at(3), Some(0x13));
    }

    #[test]
    fn releasing_an_unknown_handle_leaves_every_claim_alone() {
        let router: DrainRouter<2> = DrainRouter::new();
        let link = router.claim(9).expect("slot");
        router.release(11);
        router.release(NO_CONN_HANDLE);
        assert_eq!(link.conn_handle(), Some(9));
        assert_eq!(router.claimed(), 1);
    }
}
