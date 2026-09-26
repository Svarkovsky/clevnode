//! When a node announces itself, and when it must not (Codeberg #376).
//!
//! Until this crate existed a board announced its delivery destination
//! at exactly one moment: immediately before a telemetry report. A phone
//! that connected between two reports learned nothing about the board
//! until the next one, and a board with no target, or with a target it
//! cannot reach, never announced at all. The field operator saw the
//! consequence on 2026-09-09: an hour without an announce from a
//! stationary relay, so its hop count could not be judged at all, while
//! the board that had announced recently sat at one hop.
//!
//! Two new occasions follow, and both need a gate:
//!
//! * **Peer up** — a BLE peer finished its identity handshake, so it can
//!   receive, and it is exactly the peer that does not know us yet. The
//!   announce goes to that ONE link (the #376 delivery hint), because an
//!   announce broadcast to every link is rebroadcast by the neighbour
//!   board and races the direct copy at the phone, which is the
//!   mechanism this issue started with. Gated by
//!   [`PeerAnnounceLimiter`].
//! * **Periodic** — a node that reports rarely, or not at all, still has
//!   to be findable. Gated by [`PeriodicAnnounce`].
//!
//! Both gates share the clock rule: **without a plausible wall clock,
//! nothing is announced.** The emission timestamp inside an announce is
//! what a receiver ranks paths by, and a later announce carrying an
//! older emission never replaces a path
//! (`docs/src/protocol-notes/announce-dedup-and-path-replacement.md`).
//! A board stamping from uptime therefore does not merely announce
//! badly, it poisons the receiver's path table for the whole life of
//! that entry — the harm outlives the packet.
//!
//! Host-tested here rather than on a board for the usual reason: the
//! interesting states are "the same phone relinked four times in ten
//! minutes" and "the clock arrived between two ticks", and both take
//! deliberate timing to reach on hardware. Both stacks use this crate
//! ([`leviculum-nrf`]'s board binaries and `lnsd`'s driver), so the
//! policy the desk measures on a board is the policy the daemon runs.
//!
//! [`leviculum-nrf`]: https://codeberg.org/Lew_Palm/leviculum

#![cfg_attr(not(test), no_std)]

/// A peer identity, as both stacks report it on peer-up and stamp on
/// inbound packets: the 16-byte truncated destination hash.
pub type PeerId = [u8; 16];

/// At most one peer-up announce per peer IDENTITY per 15 minutes
/// (Lew's decision, 2026-09-09).
///
/// A peer learns the path from the first announce and a Reticulum path
/// lives for hours, so re-announcing on every relink is airtime without
/// gain. It is not a theoretical relink either: a phone rotates its BLE
/// address as often as once a minute, and each rotation is a fresh
/// connection. So the limit is keyed on the peer identity and never on
/// its address — a rotation must not reset it, which is the whole point
/// of the identity handshake being what raises the peer-up edge.
///
/// Fifteen minutes can be this long because a missed first announce is
/// not final: [`PeriodicAnnounce`] emits one on every interface anyway,
/// and the peer's own path request is answered as before.
pub const PEER_UP_ANNOUNCE_MIN_INTERVAL_MS: u64 = 15 * 60 * 1_000;

/// The plain periodic announce of a node's own destination, on all
/// interfaces: every 30 minutes.
///
/// **What the reference does.** Reticulum leaves an application
/// destination's announce cadence to the application: `LXMRouter.announce`
/// (`reference/LXMF/LXMF/LXMRouter.py:315`) is called by the app and LXMF
/// runs no timer over the delivery destinations it holds. The one
/// destination-announce timer the reference stack runs on its own behalf
/// is Transport's management destination, `mgmt_announce_interval = 2*60*60`
/// (`reference/Reticulum/RNS/Transport.py:194`, fired from the jobs loop at
/// `reference/Reticulum/RNS/Transport.py:963`) — which this tree already
/// matches for its own management destination
/// (`leviculum_core::constants::MGMT_ANNOUNCE_INTERVAL_MS`). So there is
/// no reference number for this cadence to copy, and the value is ours to
/// argue.
///
/// **The airtime argument, on the leg that has one.** An LNode announce is
/// about 180 bytes on the wire, and the compiled default LoRa profile is
/// SF8 / BW125 / CR4:5 with an 18-symbol preamble (`RadioConfig::eu_medium`,
/// `leviculum-nrf/src/lora.rs:279`). That is 533 ms of airtime per announce
/// (`leviculum_core::rnode::airtime_ms_with_preamble(180, 125_000, 8, 5, 18)`).
/// At one announce per 30 minutes that is 0.0296 % duty — a thirtieth of
/// the strictest 1 % EU868 sub-band budget, and a three-hundredth of the
/// 10 % that ERC 70-03 h1.7 allows on the 869.463 MHz channel the default
/// profile actually uses. The airtime is not what bounds this number.
///
/// **What does bound it** is the operator's complaint: an hour of silence
/// made a relay's hop count unjudgeable. Thirty minutes halves the worst
/// case a node can be invisible for while staying three orders of
/// magnitude under the legal budget on the slowest carrier we ship. The
/// peer-up announce covers the common case (a phone that connects gets an
/// announce at once), so this cadence is the backstop, not the primary
/// path.
pub const PERIODIC_ANNOUNCE_INTERVAL_MS: u64 = 30 * 60 * 1_000;

/// How long after start the FIRST periodic announce fires.
///
/// The reference defers its own first management announce to ~15 s after
/// start (`Transport.last_mgmt_announce` is seeded at
/// `start - interval + 15`, `reference/Reticulum/RNS/Transport.py:283`),
/// and this tree matches that for its management destination. Thirty
/// seconds rather than fifteen, deliberately: on a board both announces
/// share one half-duplex LoRa radio, and two announces contending for the
/// same airtime window at boot is the collision the spacing exists to
/// avoid. Fifteen seconds apart, each has the whole window to itself.
pub const PERIODIC_ANNOUNCE_INITIAL_DELAY_MS: u64 = 30 * 1_000;

/// How soon a clock-withheld periodic announce is retried.
///
/// Not the full interval: a board that gets its GNSS fix (or a host
/// `--set-time`) one second after a withheld tick would otherwise stay
/// silent for the rest of the interval, which is the "invisible board"
/// this whole batch is about. One minute bounds the delay between "the
/// clock arrived" and "the mesh hears us" without turning a clockless
/// board into a timer that runs all day.
pub const NO_CLOCK_RETRY_MS: u64 = 60 * 1_000;

/// Why an announce did not happen. The `reason=` slot of the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    /// No plausible wall clock: the emission timestamp would come from
    /// uptime, and an announce stamped that way can never replace a path
    /// at the receiver afterwards.
    NoClock,
    /// This peer identity already got a peer-up announce inside
    /// [`PEER_UP_ANNOUNCE_MIN_INTERVAL_MS`].
    RateLimited,
}

impl Withheld {
    /// The stable token for the `reason=` field of the structured log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Withheld::NoClock => "no-clock",
            Withheld::RateLimited => "rate-limited",
        }
    }
}

/// What a gate says about one occasion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Announce now.
    Announce,
    /// Do not announce, for this reason.
    Withheld(Withheld),
}

impl Decision {
    /// Whether the caller should emit the announce.
    #[must_use]
    pub const fn is_announce(self) -> bool {
        matches!(self, Decision::Announce)
    }
}

/// The per-peer peer-up gate: at most one announce per peer identity per
/// [`PEER_UP_ANNOUNCE_MIN_INTERVAL_MS`].
///
/// `N` slots of history. A slot is free when it has never been used or
/// when its entry has aged past the window — an expired entry says
/// nothing a fresh one would not say, so reusing it is not eviction.
/// When every slot is in-window the OLDEST is taken, which is the only
/// way this can be wrong: with more than `N` peers announcing inside one
/// window, the peer whose announce is furthest in the past can get a
/// second one. Sized above the link count of both stacks, so reaching it
/// takes more distinct identities in fifteen minutes than either stack
/// can hold links for, and the cost when it happens is one extra
/// announce on one link.
#[derive(Debug, Clone)]
pub struct PeerAnnounceLimiter<const N: usize> {
    slots: [Option<(PeerId, u64)>; N],
}

impl<const N: usize> Default for PeerAnnounceLimiter<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> PeerAnnounceLimiter<N> {
    /// An empty limiter: every peer's first peer-up announces.
    #[must_use]
    pub const fn new() -> Self {
        Self { slots: [None; N] }
    }

    /// Decide one peer-up edge, and record the announce when it is
    /// allowed.
    ///
    /// `clock_ok` is the caller's plausibility answer
    /// (`NodeCore::has_plausible_wall_clock`). A clock-withheld edge is
    /// NOT recorded: nothing went on air, so the next edge for this peer
    /// must be free to announce the moment the clock exists.
    pub fn peer_up(&mut self, peer: PeerId, now_ms: u64, clock_ok: bool) -> Decision {
        if !clock_ok {
            return Decision::Withheld(Withheld::NoClock);
        }
        let slot = match self.slot_of(&peer) {
            Some(slot) => {
                if !self.would_announce(&peer, now_ms) {
                    return Decision::Withheld(Withheld::RateLimited);
                }
                slot
            }
            None => self.free_slot(now_ms),
        };
        self.slots[slot] = Some((peer, now_ms));
        Decision::Announce
    }

    /// Whether this peer would be announced to right now, without
    /// recording anything. For assertions and for a caller that wants to
    /// skip the work of building an announce it may not send.
    #[must_use]
    pub fn would_announce(&self, peer: &PeerId, now_ms: u64) -> bool {
        match self.slot_of(peer) {
            Some(slot) => match self.slots[slot] {
                Some((_, last_ms)) => {
                    now_ms.saturating_sub(last_ms) >= PEER_UP_ANNOUNCE_MIN_INTERVAL_MS
                }
                None => true,
            },
            None => true,
        }
    }

    fn slot_of(&self, peer: &PeerId) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| matches!(slot, Some((id, _)) if id == peer))
    }

    /// A never-used slot, else an aged-out one, else the oldest.
    fn free_slot(&self, now_ms: u64) -> usize {
        if let Some(idx) = self.slots.iter().position(Option::is_none) {
            return idx;
        }
        let mut oldest = 0usize;
        let mut oldest_ms = u64::MAX;
        for (idx, slot) in self.slots.iter().enumerate() {
            if let Some((_, last_ms)) = slot {
                if now_ms.saturating_sub(*last_ms) >= PEER_UP_ANNOUNCE_MIN_INTERVAL_MS {
                    return idx;
                }
                if *last_ms < oldest_ms {
                    oldest_ms = *last_ms;
                    oldest = idx;
                }
            }
        }
        oldest
    }
}

/// The timer half: a plain periodic announce on all interfaces,
/// independent of telemetry.
///
/// The caller owns the wake-up; this owns the deadline. Ask
/// [`wait_ms`](Self::wait_ms) how long to sleep, call
/// [`poll`](Self::poll) when that expires (or on any earlier wake-up —
/// a poll before the deadline is a no-op), and act on what it says.
#[derive(Debug, Clone, Copy)]
pub struct PeriodicAnnounce {
    next_ms: u64,
}

impl PeriodicAnnounce {
    /// Arm the first announce [`PERIODIC_ANNOUNCE_INITIAL_DELAY_MS`]
    /// after `now_ms`.
    #[must_use]
    pub const fn new(now_ms: u64) -> Self {
        Self {
            next_ms: now_ms + PERIODIC_ANNOUNCE_INITIAL_DELAY_MS,
        }
    }

    /// `None` before the deadline. At or after it, the decision — and
    /// the deadline moves either way, so a caller that ignores the
    /// answer still cannot spin.
    pub fn poll(&mut self, now_ms: u64, clock_ok: bool) -> Option<Decision> {
        if now_ms < self.next_ms {
            return None;
        }
        if !clock_ok {
            self.next_ms = now_ms + NO_CLOCK_RETRY_MS;
            return Some(Decision::Withheld(Withheld::NoClock));
        }
        self.next_ms = now_ms + PERIODIC_ANNOUNCE_INTERVAL_MS;
        Some(Decision::Announce)
    }

    /// How long until the next poll is worth making. Zero means "now".
    #[must_use]
    pub fn wait_ms(&self, now_ms: u64) -> u64 {
        self.next_ms.saturating_sub(now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PeerId = [0xaa; 16];
    const B: PeerId = [0xbb; 16];

    fn t0() -> u64 {
        1_700_000_000_000
    }

    /// The batch's first sentence: one announce per peer-up edge.
    #[test]
    fn a_first_peer_up_announces() {
        let mut gate = PeerAnnounceLimiter::<8>::new();
        assert_eq!(gate.peer_up(A, t0(), true), Decision::Announce);
    }

    /// The rate limit, in the shape the field produces it: the SAME
    /// identity relinking twice inside the window announces once. The
    /// phone rotates its BLE address on every reconnect, so "same peer"
    /// can only mean the identity — which is why the limiter is keyed on
    /// nothing else.
    #[test]
    fn the_same_identity_relinking_inside_the_window_announces_once() {
        let mut gate = PeerAnnounceLimiter::<8>::new();
        assert_eq!(gate.peer_up(A, t0(), true), Decision::Announce);
        for minute in 1..15u64 {
            assert_eq!(
                gate.peer_up(A, t0() + minute * 60_000, true),
                Decision::Withheld(Withheld::RateLimited),
                "relink at +{minute} min: inside the 15 minute window"
            );
        }
    }

    /// And announces again once the window is over.
    #[test]
    fn the_window_expires() {
        let mut gate = PeerAnnounceLimiter::<8>::new();
        assert_eq!(gate.peer_up(A, t0(), true), Decision::Announce);
        let just_before = t0() + PEER_UP_ANNOUNCE_MIN_INTERVAL_MS - 1;
        assert_eq!(
            gate.peer_up(A, just_before, true),
            Decision::Withheld(Withheld::RateLimited)
        );
        assert_eq!(
            gate.peer_up(A, t0() + PEER_UP_ANNOUNCE_MIN_INTERVAL_MS, true),
            Decision::Announce
        );
    }

    /// One peer's limit is not another's: the phone and the neighbour
    /// board are two peers on the same interface.
    #[test]
    fn the_limit_is_per_peer() {
        let mut gate = PeerAnnounceLimiter::<8>::new();
        assert_eq!(gate.peer_up(A, t0(), true), Decision::Announce);
        assert_eq!(gate.peer_up(B, t0(), true), Decision::Announce);
        assert_eq!(
            gate.peer_up(A, t0() + 1_000, true),
            Decision::Withheld(Withheld::RateLimited)
        );
    }

    /// No clock, no announce — the gate that outranks the limit.
    #[test]
    fn no_announce_without_a_clock() {
        let mut gate = PeerAnnounceLimiter::<8>::new();
        assert_eq!(
            gate.peer_up(A, t0(), false),
            Decision::Withheld(Withheld::NoClock)
        );
    }

    /// A clock-withheld edge spends nothing: the peer's first REAL
    /// announce must not be rate-limited by an announce that never went
    /// on air.
    #[test]
    fn a_clock_withheld_edge_does_not_spend_the_peers_window() {
        let mut gate = PeerAnnounceLimiter::<8>::new();
        assert_eq!(
            gate.peer_up(A, t0(), false),
            Decision::Withheld(Withheld::NoClock)
        );
        assert_eq!(
            gate.peer_up(A, t0() + 1, true),
            Decision::Announce,
            "the clock arrived one millisecond later; nothing was spent"
        );
    }

    /// The address rotation the limit exists for, stated as a test: two
    /// different BLE addresses are irrelevant here because an address
    /// never enters this gate. Same identity, five relinks, one
    /// announce.
    #[test]
    fn an_address_rotation_cannot_reset_the_limit() {
        let mut gate = PeerAnnounceLimiter::<8>::new();
        assert_eq!(gate.peer_up(A, t0(), true), Decision::Announce);
        let announces = (1..=5u64)
            .filter(|n| gate.peer_up(A, t0() + n * 60_000, true).is_announce())
            .count();
        assert_eq!(
            announces, 0,
            "five relinks in five minutes, no new announce"
        );
    }

    /// Slot pressure: with more in-window identities than slots, the
    /// oldest is displaced and the cost is one extra announce for it —
    /// never a wrong "rate-limited" for a peer that never got one.
    #[test]
    fn a_full_table_displaces_the_oldest_and_never_denies_a_new_peer() {
        let mut gate = PeerAnnounceLimiter::<2>::new();
        assert_eq!(gate.peer_up(A, t0(), true), Decision::Announce);
        assert_eq!(gate.peer_up(B, t0() + 1_000, true), Decision::Announce);
        let c: PeerId = [0xcc; 16];
        assert_eq!(
            gate.peer_up(c, t0() + 2_000, true),
            Decision::Announce,
            "a peer this gate has never seen always announces"
        );
        // A displaced the oldest slot; B, still held, is still limited.
        assert_eq!(
            gate.peer_up(B, t0() + 3_000, true),
            Decision::Withheld(Withheld::RateLimited)
        );
    }

    /// `would_announce` answers what `peer_up` would, without spending
    /// anything.
    #[test]
    fn would_announce_does_not_record() {
        let mut gate = PeerAnnounceLimiter::<8>::new();
        assert!(gate.would_announce(&A, t0()));
        assert!(gate.would_announce(&A, t0()), "still nothing recorded");
        assert_eq!(gate.peer_up(A, t0(), true), Decision::Announce);
        assert!(!gate.would_announce(&A, t0() + 1_000));
    }

    /// The periodic timer: nothing before the initial delay, one
    /// announce at it, nothing again until the interval.
    #[test]
    fn the_periodic_announce_fires_at_the_initial_delay_then_at_the_interval() {
        let mut timer = PeriodicAnnounce::new(t0());
        assert_eq!(timer.poll(t0(), true), None);
        assert_eq!(
            timer.poll(t0() + PERIODIC_ANNOUNCE_INITIAL_DELAY_MS - 1, true),
            None
        );
        let first = t0() + PERIODIC_ANNOUNCE_INITIAL_DELAY_MS;
        assert_eq!(timer.poll(first, true), Some(Decision::Announce));
        assert_eq!(timer.poll(first + 1, true), None);
        assert_eq!(
            timer.poll(first + PERIODIC_ANNOUNCE_INTERVAL_MS - 1, true),
            None
        );
        assert_eq!(
            timer.poll(first + PERIODIC_ANNOUNCE_INTERVAL_MS, true),
            Some(Decision::Announce)
        );
    }

    /// Clockless: withheld with the named reason, and retried in a
    /// minute rather than in half an hour.
    #[test]
    fn a_clockless_periodic_tick_is_withheld_and_retried_soon() {
        let mut timer = PeriodicAnnounce::new(t0());
        let first = t0() + PERIODIC_ANNOUNCE_INITIAL_DELAY_MS;
        assert_eq!(
            timer.poll(first, false),
            Some(Decision::Withheld(Withheld::NoClock))
        );
        assert_eq!(timer.poll(first + NO_CLOCK_RETRY_MS - 1, true), None);
        assert_eq!(
            timer.poll(first + NO_CLOCK_RETRY_MS, true),
            Some(Decision::Announce),
            "the clock arrived during the minute; the announce follows it"
        );
    }

    /// `wait_ms` is the sleep the caller owes, and it never returns a
    /// deadline in the past.
    #[test]
    fn wait_ms_tracks_the_deadline() {
        let mut timer = PeriodicAnnounce::new(t0());
        assert_eq!(timer.wait_ms(t0()), PERIODIC_ANNOUNCE_INITIAL_DELAY_MS);
        let first = t0() + PERIODIC_ANNOUNCE_INITIAL_DELAY_MS;
        assert_eq!(timer.wait_ms(first), 0);
        assert_eq!(timer.poll(first, true), Some(Decision::Announce));
        assert_eq!(timer.wait_ms(first), PERIODIC_ANNOUNCE_INTERVAL_MS);
        assert_eq!(timer.wait_ms(first + PERIODIC_ANNOUNCE_INTERVAL_MS * 2), 0);
    }

    /// The reason tokens are the log's vocabulary: pinned so a rename
    /// has to be deliberate.
    #[test]
    fn the_reason_tokens_are_stable() {
        assert_eq!(Withheld::NoClock.as_str(), "no-clock");
        assert_eq!(Withheld::RateLimited.as_str(), "rate-limited");
    }
}
