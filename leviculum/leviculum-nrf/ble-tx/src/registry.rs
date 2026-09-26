//! The per-interface peer-link registry: which 16-byte identity each
//! live link slot belongs to, and the first/last-link rules that decide
//! when a link transition is a *peer* transition (Codeberg #365).
//!
//! A Reticulum BLE interface is one broadcast domain carrying several
//! links, and the core is only told about peers, not links: the
//! identity's FIRST link up is a peer arrival (the main loop pulls the
//! peer's delivery path), the identity's LAST link down is a peer loss
//! (the main loop culls the paths via that peer). A same-identity link
//! on another slot — a peer whose rotated address reconnected while its
//! old link was still up — is registry churn on both edges and must
//! report neither.
//!
//! Which of the two links that peer keeps is [`PeerRegistry::link_up`]'s
//! second job, and the rule is [`judge_duplicate`]: who opened the
//! connection decides, and nothing else does. The registry still
//! carries a liveness clock per slot ([`PeerRegistry::note_heard`]),
//! fed by the caller's inbound path for EVERY frame, keepalives
//! included, exactly as lnsd's `LinkTable` feeds `last_heard_ms`: it is
//! reported with every duplicate decision, and it is what the callers'
//! expiry sweeps read through [`PeerRegistry::silence_ms`] to tear a
//! link down that has stopped answering altogether.
//!
//! The rules are pure and their failure modes are sequences (a flap, a
//! displacement, the runtime carrier-off teardown that drops every live
//! link at once), so they live here with the crate's other host-tested
//! state machines; the firmware wraps one instance in a
//! critical-section mutex and reports what the return values tell it to
//! (`leviculum_nrf::ble::columba`).

use leviculum_core::framing::ble::KEEPALIVE_INTERVAL_MS;

/// One interface's live links, indexed by the link's drain-table slot —
/// the same index that selects its outbound queue, so the drain table,
/// the fan-out and this registry can never disagree about which links
/// exist.
///
/// Two facts per slot, learned at different moments: the CONNECTION
/// address ([`conn_up`](Self::conn_up)), known the instant the link
/// exists in either role, and the peer IDENTITY
/// ([`link_up`](Self::link_up)), known only after the handshake or the
/// characteristic read. The address side exists for the scanner
/// (#375 §0): Core Spec Vol 6 Part B §4.5 permits only one connection
/// between two device addresses — an initiator "shall not send a
/// connection request to an advertiser it is already connected to",
/// and an advertiser "shall ignore" one from a device it is connected
/// to — so a dial to a live link's address can never succeed and must
/// be excluded BEFORE it spends five seconds timing out on the air.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRegistry<const N: usize> {
    slots: [Option<[u8; 16]>; N],
    addrs: [Option<u64>; N],
    last_heard_ms: [u64; N],
}

/// A link that has delivered NOTHING — no packet fragment, no
/// keepalive — for this long is dead, and is torn down: it is the
/// EXPIRY bound, on both stacks (lnsd's `LinkTable::expire`, the
/// firmware session's `link_silent` arm), and since #382 it decides
/// nothing else. [`judge_duplicate`] does not read it — a duplicate is
/// decided by who opened the connection and by nothing else — so no
/// link is ever displaced for being quiet; a link that stops answering
/// is removed by the sweep, whether or not anybody dials it.
///
/// Three missed keepalives at the protocol's 15 s cadence
/// (`leviculum_core::framing::ble::KEEPALIVE_INTERVAL_MS`). One
/// constant, imported by lnsd rather than duplicated, so the two stacks
/// cannot disagree about when a link is over.
///
/// It replaced a 30 s clock that measured PAYLOAD silence only
/// (Codeberg #382). The measurement that killed that clock: over 14.1 h
/// beside a Columba phone (`ble-accept-rns/lnsd.log`, 2026-08-30) the
/// gaps between received non-keepalive packets from a peer that was
/// demonstrably present throughout ran to a median of 51 s, a 90th
/// percentile of 182 s and a maximum of 5590 s, and 502 links outlived
/// 45 s without one byte of payload. Payload silence is what an idle
/// phone looks like; it is not evidence of anything. Keepalives are,
/// and the same log shows them arriving: only 2 of those 502 links were
/// ever closed by the silence timer.
pub const LINK_TIMEOUT_MS: u64 = 3 * KEEPALIVE_INTERVAL_MS;

/// Who opened the connection whose duplicate identity is being judged —
/// the asymmetry the rule turns on ([`judge_duplicate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The PEER connected to us (we are the peripheral; on lnsd,
    /// `Role::Peripheral`).
    Incoming,
    /// WE dialled the peer (we are the central; on lnsd,
    /// `Role::Central`).
    Outgoing,
}

/// What to do with a second connection carrying an identity we already
/// hold a live link to (see [`judge_duplicate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duplicate {
    /// Keep the old link, drop the newcomer.
    Refuse,
    /// Tear the old link down; the newcomer takes the peer over.
    Displace,
}

/// Decide a duplicate identity from WHO opened the connection, and
/// from nothing else — no clock is consulted in either direction
/// (Codeberg #382).
///
/// The two field failures of 2026-09-09 were the two directions of this
/// question, and each names its own answer:
///
/// - **Incoming — displace.** The phone connected to a board that was
///   already linked to it, the board refused, and the phone stopped
///   reading the link it had abandoned: every announce the board sent
///   went into a dead socket (6d3e5d4). A peer that opens a second
///   connection has, by its own one-link-per-identity rule, given up on
///   the first, and it has already built the replacement — so the cost
///   of displacing is one link swapped for an equivalent one, while the
///   cost of refusing is a peer that hears nothing until the old link
///   expires. The peer's own action is better evidence than any timer
///   of ours.
/// - **Outgoing — refuse, always.** Our dial is evidence of nothing.
///   An advertisement carries no identity and the peer rotates its
///   address, so a dial that lands on an identity we already hold is
///   most likely our own fallback dial finding the peer beside us.
///   Displacing there killed the phone's working link every ~95 s
///   (13bea3e5), and the surviving "unless the old link looks dead"
///   clause went on doing it: the 2026-09-09 T114 refused one such dial
///   at 12:34:20 and displaced the same phone's link forty seconds
///   later, handing the peer a link Columba lists as `Unknown` at
///   MTU 20 in place of a fully negotiated one, purely because the old
///   link had been quiet. How quiet the peer has been is not evidence
///   about the peer's intent, and our own dial is not evidence at all,
///   so the two together cannot add up to one.
///
/// **A dead link is still cleared — by expiry, not here.** The liveness
/// clock ([`PeerRegistry::note_heard`], fed per inbound FRAME including
/// the 1-byte keepalive since 381fa5a0) is read by two sweeps that tear
/// a link down once it has delivered neither payload nor keepalive for
/// [`LINK_TIMEOUT_MS`]: lnsd's `LinkTable::expire`
/// (`leviculum-std/src/interfaces/ble/links.rs`), driven from the
/// interface tick in the same module's `mod.rs`, which disconnects the
/// device and reports the peer lost; and the firmware's per-session
/// silence arm `link_silent` (`leviculum-nrf/src/ble/columba.rs`),
/// which disconnects and reports through the same `peer_link_down`
/// range loss takes. That arm exists BECAUSE of this rule: until it
/// landed the board had no expiry sweep at all and the displacement
/// clause was the only thing that ever cleared such a link — which is
/// exactly why the clause could not simply be deleted on its own.
///
/// Silence on payload alone is deliberately no part of any of this: an
/// idle phone sends payload minutes apart (the distribution is on
/// [`LINK_TIMEOUT_MS`]), so the old 30 s payload clock fired on healthy
/// links. The keepalive is what a live peer emits whether or not it has
/// anything to say, which is exactly what "alive" needs to mean.
pub const fn judge_duplicate(origin: Origin) -> Duplicate {
    match origin {
        Origin::Incoming => Duplicate::Displace,
        Origin::Outgoing => Duplicate::Refuse,
    }
}

/// What registering a link amounted to (see [`PeerRegistry::link_up`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkUp {
    /// Registered. `first` iff this is the identity's FIRST live link —
    /// the caller reports a peer arrival exactly then.
    Accepted { first: bool },
    /// Registered, and the identity's OLD link on `old_slot` must be
    /// torn down by the caller. Never an arrival — the peer was never
    /// gone. `old_silence_ms` is how long that link had delivered
    /// nothing at all, the evidence a capture can check the decision
    /// against; on an incoming duplicate it is reported, not consulted.
    Displaced {
        old_slot: usize,
        old_silence_ms: u64,
    },
    /// NOT registered: our own dial reached an identity that already
    /// holds a link, on `old_slot`, and that link keeps the peer
    /// whatever `old_silence_ms` says — the age is reported, never
    /// consulted (see [`judge_duplicate`]). The caller drops THIS
    /// connection.
    Refused {
        old_slot: usize,
        old_silence_ms: u64,
    },
}

impl<const N: usize> Default for PeerRegistry<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> PeerRegistry<N> {
    /// No links.
    pub const fn new() -> Self {
        Self {
            slots: [None; N],
            addrs: [None; N],
            last_heard_ms: [0; N],
        }
    }

    /// Register a slot's connection address — at the connection event,
    /// in either role, before any identity is known.
    pub fn conn_up(&mut self, slot: usize, addr_value: u64) {
        self.addrs[slot] = Some(addr_value);
    }

    /// Clear a slot's connection address at teardown (the counterpart
    /// of [`conn_up`](Self::conn_up); identity clearing is
    /// [`link_down`](Self::link_down)'s job).
    pub fn conn_down(&mut self, slot: usize) {
        self.addrs[slot] = None;
    }

    /// Whether a live connection was made on this address — the
    /// scanner's pre-dial exclusion (see the struct docs). Addresses
    /// rotate, so a linked PEER can still reappear under a fresh
    /// address this check cannot know — and the identity behind an
    /// advertisement is unknowable before connecting, so that residual
    /// dial is inherent. It is resolved post-connect by
    /// [`link_up`](Self::link_up)'s identity duplicate rule, which
    /// refuses such a dial outright.
    pub fn addr_linked(&self, addr_value: u64) -> bool {
        self.addrs.iter().flatten().any(|a| *a == addr_value)
    }

    /// Register a slot's peer, at `now_ms`, with `origin` naming who
    /// opened this connection.
    ///
    /// With no other link from that identity this is a plain
    /// [`LinkUp::Accepted`], `first` iff it is the identity's FIRST live
    /// link — the caller reports a peer arrival exactly then; `origin`
    /// is not consulted.
    ///
    /// A second connection from an identity we already hold is decided
    /// by [`judge_duplicate`]: an incoming one displaces the old link,
    /// an outgoing one is [`LinkUp::Refused`], and no clock enters
    /// either decision. The old link's silence is still MEASURED and
    /// carried out on both variants — it is the number a capture checks
    /// the decision against, and the number that would show us wrong —
    /// but it is not consulted. Neither edge of a displacement is a peer
    /// transition — the peer was never gone — which is why `Displaced`
    /// carries no `first` flag.
    ///
    /// Re-registering the SAME slot is neither: the link the caller
    /// would tear down is the one it just kept.
    ///
    /// An accepted link starts its liveness clock here. The handshake
    /// (peripheral) or the identity read (central) that got us this far
    /// is itself something the peer delivered.
    pub fn link_up(&mut self, slot: usize, peer: [u8; 16], origin: Origin, now_ms: u64) -> LinkUp {
        let old = self
            .slots
            .iter()
            .position(|id| *id == Some(peer))
            .filter(|old| *old != slot);
        if let Some(old_slot) = old {
            // Measured for the log line on BOTH branches, consulted on
            // neither: it is the evidence a capture would convict the
            // rule with, not an input to it (#382).
            let old_silence_ms = now_ms.saturating_sub(self.last_heard_ms[old_slot]);
            if judge_duplicate(origin) == Duplicate::Refuse {
                return LinkUp::Refused {
                    old_slot,
                    old_silence_ms,
                };
            }
            self.slots[slot] = Some(peer);
            self.last_heard_ms[slot] = now_ms;
            return LinkUp::Displaced {
                old_slot,
                old_silence_ms,
            };
        }
        let first = self.slots.iter().flatten().all(|id| *id != peer);
        self.slots[slot] = Some(peer);
        self.last_heard_ms[slot] = now_ms;
        LinkUp::Accepted { first }
    }

    /// A slot's peer delivered a frame at `now_ms` — the liveness clock
    /// the expiry reads through [`silence_ms`](Self::silence_ms), and
    /// the one [`link_up`](Self::link_up) reports (never consults) with
    /// each duplicate decision.
    ///
    /// EVERY inbound frame, keepalives included (Codeberg #382). A
    /// keepalive is the one thing a peer with nothing to say still
    /// sends, so excluding it made a quiet peer indistinguishable from
    /// a departed one; the failure the old payload-only clock guarded
    /// against — a degraded link that still passes 1-byte writes while
    /// packet-sized ones fail — is now caught by the peer itself, which
    /// reconnects and displaces the link as an incoming duplicate.
    ///
    /// Called per FRAME, not per reassembled packet, so a long packet's
    /// fragments each count and a packet that never finishes
    /// reassembling still proves the link delivers.
    pub fn note_heard(&mut self, slot: usize, now_ms: u64) {
        self.last_heard_ms[slot] = now_ms;
    }

    /// How long the slot's link has delivered nothing at all — no
    /// payload, no keepalive — at `now_ms`. `None` while the slot holds
    /// no identity: a connection that has not handshaked yet has no
    /// liveness clock to read, and the slot's previous tenant's clock
    /// is not it.
    ///
    /// The expiry sweep's input. lnsd's `LinkTable::expire` makes the
    /// same subtraction over its own rows; the firmware has no sweep
    /// task, so its per-session silence arm asks the registry directly
    /// (`leviculum_nrf::ble::columba::link_silent`). A link at or past
    /// [`LINK_TIMEOUT_MS`] here is torn down — that, not a displacement,
    /// is what clears a dead link since #382.
    pub fn silence_ms(&self, slot: usize, now_ms: u64) -> Option<u64> {
        self.slots[slot]?;
        Some(now_ms.saturating_sub(self.last_heard_ms[slot]))
    }

    /// Clear a slot. `Some(identity)` iff that took the identity's LAST
    /// live link — the caller reports a peer loss exactly then. An
    /// unclaimed slot yields `None`: no link, no loss.
    pub fn link_down(&mut self, slot: usize) -> Option<[u8; 16]> {
        let identity = self.slots[slot].take()?;
        self.slots
            .iter()
            .flatten()
            .all(|id| *id != identity)
            .then_some(identity)
    }

    /// Whether the identity holds a live link — the central path's
    /// duplicate check (BLE addresses rotate, identities do not).
    pub fn is_linked(&self, peer: &[u8; 16]) -> bool {
        self.slots.iter().flatten().any(|id| id == peer)
    }

    /// The slot of a live link to this peer, if it holds one (Codeberg
    /// #376) — the fan-out's peer-to-link map.
    ///
    /// A peer holding two links is still one peer, and either link
    /// reaches it, so the lowest slot is returned. Two links exist only
    /// during a displacement's hand-over, whose old link is
    /// already being torn down: [`Self::link_up`] registers the new slot
    /// BEFORE the old one is signalled, and the old session clears its
    /// registry entry before releasing its drain slot, so the window is a
    /// fan-out or two wide and both slots are live throughout it.
    pub fn slot_for(&self, peer: &[u8; 16]) -> Option<usize> {
        self.slots.iter().position(|id| id.as_ref() == Some(peer))
    }

    /// The number of DISTINCT live peer identities (Codeberg #365) —
    /// the value the main loop mirrors into the core as the
    /// interface's peer count. Distinct, not per-slot: during a
    /// displacement's hand-over (#376) one peer briefly holds two links,
    /// and it is still one peer.
    pub fn peer_count(&self) -> usize {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|id| (i, id)))
            .filter(|(i, id)| {
                self.slots[..*i]
                    .iter()
                    .flatten()
                    .all(|earlier| earlier != *id)
            })
            .count()
    }
}

/// What the core's #376 delivery hint made of one outbound packet
/// (`leviculum_nrf::ble::tx_fanout_task`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxFanout {
    /// No hint — a broadcast (announce, path request, anything the core
    /// did not address at a named peer). Copied into every live link's
    /// queue, which is what a Reticulum interface owes a broadcast: one
    /// `try_send` reaches every peer on the medium, exactly as one LoRa
    /// transmission reaches every listener.
    Flood,
    /// Hinted, and the peer holds a live link: queue on that slot alone.
    Route(usize),
    /// Hinted at a peer with NO live link here. The packet is DROPPED,
    /// not flooded.
    ///
    /// The hint exists because the core routed these bytes at that one
    /// neighbour. The remaining links are not a route to it: flooding
    /// them spends their airtime on a packet they must forward or drop,
    /// and a neighbour that forwards it re-creates exactly the relayed
    /// duplicate the hint removes (the 2026-09-09 desk failure, where a
    /// telemetry report addressed to the phone reached it twice, once
    /// through the other board). The peer's disappearance is separately
    /// reported to the core as a peer loss (Codeberg #365), which culls
    /// the paths via it, so the next packet for that destination is
    /// routed afresh — over another interface or after a fresh path
    /// request — instead of sprayed at links that cannot deliver it.
    NoLink,
}

/// Map the core's delivery hint onto this interface's links (see
/// [`TxFanout`]).
///
/// Pure, so the decision is host-tested; the firmware's fan-out task
/// supplies the registry and executes the answer.
pub fn plan_fanout<const N: usize>(
    registry: &PeerRegistry<N>,
    peer: Option<&[u8; 16]>,
) -> TxFanout {
    match peer {
        None => TxFanout::Flood,
        Some(peer) => match registry.slot_for(peer) {
            Some(slot) => TxFanout::Route(slot),
            None => TxFanout::NoLink,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 16] = [0xaa; 16];
    const B: [u8; 16] = [0xbb; 16];

    /// The number itself, pinned. It is the link timeout both
    /// interfaces expire on — three missed keepalives — not a second
    /// constant beside it, and since #382 it is an expiry bound only:
    /// [`judge_duplicate`] cannot reach it.
    #[test]
    fn the_dead_link_bound_is_the_link_timeout_itself() {
        assert_eq!(LINK_TIMEOUT_MS, 45_000);
        assert_eq!(LINK_TIMEOUT_MS, 3 * KEEPALIVE_INTERVAL_MS);
    }

    #[test]
    fn the_first_link_of_an_identity_is_an_arrival_and_displaces_nothing() {
        let mut reg = PeerRegistry::<4>::new();
        assert_eq!(
            reg.link_up(0, A, Origin::Incoming, 0),
            LinkUp::Accepted { first: true }
        );
        assert_eq!(
            reg.link_up(1, B, Origin::Outgoing, 0),
            LinkUp::Accepted { first: true },
            "a different identity is its own arrival, in either direction"
        );
    }

    /// The 2026-09-09 evening field T114, as a unit: the phone's own
    /// link is up and answering, our fallback dial reaches the same
    /// identity from its rotated address, and the EXISTING link keeps
    /// the peer. Our dial is evidence of nothing, so it loses however
    /// long the phone has had nothing to say.
    #[test]
    fn our_own_dial_is_refused_while_the_old_link_still_answers() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 1_000);
        // Only keepalives since; no payload for ten minutes.
        let mut t = 1_000;
        while t < 600_000 {
            t += KEEPALIVE_INTERVAL_MS;
            reg.note_heard(0, t);
        }
        assert_eq!(
            reg.link_up(1, A, Origin::Outgoing, t + LINK_TIMEOUT_MS - 1),
            LinkUp::Refused {
                old_slot: 0,
                old_silence_ms: LINK_TIMEOUT_MS - 1
            },
            "ten minutes without payload is a quiet phone, not a dead link"
        );
        assert_eq!(
            reg.link_up(1, A, Origin::Outgoing, t + 1),
            LinkUp::Refused {
                old_slot: 0,
                old_silence_ms: 1
            },
            "and a link that answered a millisecond ago, the same"
        );
        assert_eq!(reg.slot_for(&A), Some(0), "the old link still holds it");
        assert!(
            reg.link_down(1).is_none(),
            "the refused slot was never registered"
        );
        assert_eq!(reg.link_down(0), Some(A), "and the old link is the peer");
    }

    /// The 2026-09-09 morning field T114, the case 6d3e5d4 was written
    /// for: the phone opened a second connection, which by its own
    /// one-link rule means it has stopped reading the first. No clock
    /// is consulted — the peer just told us, and it told us one
    /// millisecond after the old link last spoke.
    #[test]
    fn an_incoming_duplicate_displaces_the_old_link_however_fresh_it_is() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 1_000);
        reg.note_heard(0, 5_000);
        assert_eq!(
            reg.link_up(1, A, Origin::Incoming, 5_001),
            LinkUp::Displaced {
                old_slot: 0,
                old_silence_ms: 1
            }
        );
        // Both slots hold the identity until the old session's teardown
        // clears its own entry — the hand-over window `slot_for`
        // documents; the peer is one peer throughout it.
        assert_eq!(reg.peer_count(), 1);
        assert_eq!(reg.link_down(0), None, "the old link's death is churn");
        assert_eq!(reg.slot_for(&A), Some(1), "the new link holds the peer");
    }

    /// The other half of the field pair, and the one 381fa5a0 left
    /// standing: the old link is STALE — nothing at all for many times
    /// the link timeout — and our dial is refused all the same.
    ///
    /// Staleness is not a reason, for two independent reasons. First,
    /// it is not evidence about the peer: the age is measured on OUR
    /// receive path, so a link that has heard nothing is equally a peer
    /// that has left, a peer whose keepalives we are dropping, and a
    /// peer that is fine — and the 2026-09-09 T114 shows which one it
    /// usually was, having refused this dial at 12:34:20 and displaced
    /// the same living phone forty seconds later. Second, it is not
    /// NEEDED: a link this stale is one the expiry
    /// ([`PeerRegistry::silence_ms`] at [`LINK_TIMEOUT_MS`], swept by
    /// lnsd's `LinkTable::expire` and by the firmware session's
    /// `link_silent` arm) is already tearing down on its own schedule,
    /// dial or no dial. Displacing here can only ever be the same
    /// removal done earlier on worse evidence — and done by REPLACING
    /// the peer's link with one it never advertised, which is the part
    /// that cost MTU and identity in the field.
    #[test]
    fn our_own_dial_is_refused_even_when_the_old_link_stopped_answering() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 1_000);
        let now = 1_000 + 4 * LINK_TIMEOUT_MS;
        assert_eq!(
            reg.link_up(1, A, Origin::Outgoing, now),
            LinkUp::Refused {
                old_slot: 0,
                old_silence_ms: 4 * LINK_TIMEOUT_MS
            },
            "no age refutes the asymmetry: our dial is evidence of nothing"
        );
        assert_eq!(reg.slot_for(&A), Some(0), "the old link still holds it");
        // And it does not survive: the expiry the caller runs sees the
        // very same age and is what takes the link away.
        assert_eq!(reg.silence_ms(0, now), Some(4 * LINK_TIMEOUT_MS));
        assert!(reg.silence_ms(0, now).is_some_and(|s| s >= LINK_TIMEOUT_MS));
    }

    /// No clock is an input any more, in either direction, and the
    /// function's signature is where that is enforced: there is nothing
    /// left to pass it. Same rule lnsd's `admit` calls, so a phone that
    /// walks between the two stacks meets one rule.
    #[test]
    fn the_duplicate_rule_reads_origin_and_nothing_else() {
        assert_eq!(judge_duplicate(Origin::Outgoing), Duplicate::Refuse);
        assert_eq!(judge_duplicate(Origin::Incoming), Duplicate::Displace);

        // Across the whole range of ages a registry can present, the
        // outcome never moves.
        for age in [0, 1, LINK_TIMEOUT_MS - 1, LINK_TIMEOUT_MS, 3_600_000] {
            let mut reg = PeerRegistry::<4>::new();
            reg.link_up(0, A, Origin::Incoming, 0);
            assert!(
                matches!(
                    reg.link_up(1, A, Origin::Outgoing, age),
                    LinkUp::Refused { .. }
                ),
                "outgoing duplicate at age {age} must be refused"
            );
            let mut reg = PeerRegistry::<4>::new();
            reg.link_up(0, A, Origin::Incoming, 0);
            assert!(
                matches!(
                    reg.link_up(1, A, Origin::Incoming, age),
                    LinkUp::Displaced { .. }
                ),
                "incoming duplicate at age {age} must displace"
            );
        }
    }

    /// The keepalive is the whole point of #382: a peer with nothing to
    /// report still sends one every 15 s, so a link the caller keeps
    /// feeding never comes near the expiry bound — and a dial of ours
    /// could not have taken it even if it had.
    #[test]
    fn a_keepalive_alone_keeps_a_link_alive_and_out_of_reach_of_our_dial() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 0);
        let mut t = 0;
        for _ in 0..100 {
            t += KEEPALIVE_INTERVAL_MS;
            reg.note_heard(0, t);
            assert!(
                reg.silence_ms(0, t).is_some_and(|s| s < LINK_TIMEOUT_MS),
                "the expiry never comes for a link that keeps answering"
            );
        }
        assert!(
            matches!(
                reg.link_up(1, A, Origin::Outgoing, t + LINK_TIMEOUT_MS - 1),
                LinkUp::Refused { .. }
            ),
            "25 minutes of keepalives and no payload is a healthy link"
        );
        assert!(
            matches!(
                reg.link_up(1, A, Origin::Outgoing, t + 10 * LINK_TIMEOUT_MS),
                LinkUp::Refused { .. }
            ),
            "and past the expiry bound the answer is still ours to not give"
        );
    }

    /// The expiry's own input, which is the mechanism item 2 of #382
    /// hands the dead-link job to: no identity, no clock; a registered
    /// link starts at zero and ages from what it last delivered.
    #[test]
    fn silence_is_reported_only_for_a_slot_that_holds_a_link() {
        let mut reg = PeerRegistry::<4>::new();
        assert_eq!(
            reg.silence_ms(0, 10_000),
            None,
            "an un-handshaked connection has no liveness clock to read"
        );
        reg.link_up(0, A, Origin::Incoming, 10_000);
        assert_eq!(reg.silence_ms(0, 10_000), Some(0));
        assert_eq!(
            reg.silence_ms(0, 10_000 + LINK_TIMEOUT_MS),
            Some(LINK_TIMEOUT_MS)
        );
        reg.note_heard(0, 10_000 + LINK_TIMEOUT_MS);
        assert_eq!(reg.silence_ms(0, 10_000 + LINK_TIMEOUT_MS), Some(0));
        reg.link_down(0);
        assert_eq!(
            reg.silence_ms(0, 99_000),
            None,
            "and a freed slot's stale clock is nobody's evidence"
        );
    }

    /// A same-slot re-registration must not name its own slot: the
    /// caller would refuse — or tear down — the very connection it just
    /// kept. True in both directions and on both sides of the boundary.
    #[test]
    fn re_registering_the_same_slot_neither_refuses_nor_displaces() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 0);
        assert_eq!(
            reg.link_up(0, A, Origin::Incoming, 1),
            LinkUp::Accepted { first: false }
        );
        assert_eq!(
            reg.link_up(0, A, Origin::Outgoing, 10 * LINK_TIMEOUT_MS),
            LinkUp::Accepted { first: false }
        );
    }

    /// A slot's liveness clock belongs to the link that holds it now:
    /// a fresh registration resets it, so a peer inheriting a slot whose
    /// previous tenant went silent is not instantly displaceable.
    #[test]
    fn a_new_link_starts_its_own_liveness_clock() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, B, Origin::Incoming, 0);
        reg.link_down(0);
        // Slot 0 was silent for ages; A claims it now.
        let t = 10 * LINK_TIMEOUT_MS;
        reg.link_up(0, A, Origin::Incoming, t);
        assert!(matches!(
            reg.link_up(1, A, Origin::Outgoing, t + 1),
            LinkUp::Refused { .. }
        ));
    }

    #[test]
    fn the_last_link_of_an_identity_is_a_loss() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 0);
        assert_eq!(reg.link_down(0), Some(A));
    }

    #[test]
    fn a_non_last_link_down_is_churn_not_a_loss() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 0);
        reg.link_up(1, A, Origin::Incoming, LINK_TIMEOUT_MS);
        assert_eq!(reg.link_down(0), None);
        assert!(reg.is_linked(&A));
        assert_eq!(reg.link_down(1), Some(A));
    }

    #[test]
    fn an_unclaimed_slot_going_down_reports_nothing() {
        let mut reg = PeerRegistry::<4>::new();
        assert_eq!(reg.link_down(2), None);
    }

    #[test]
    fn the_duplicate_check_tracks_liveness() {
        let mut reg = PeerRegistry::<4>::new();
        assert!(!reg.is_linked(&A));
        reg.link_up(0, A, Origin::Incoming, 0);
        assert!(reg.is_linked(&A));
        reg.link_down(0);
        assert!(!reg.is_linked(&A));
    }

    /// The runtime carrier-off teardown: `--set-media ble=off` makes
    /// every connection task drop its own link, in whatever order the
    /// executor reaches them. Every linked identity must yield exactly
    /// one loss — that is what feeds one `PeerEvent::Lost` per peer to
    /// the core's path cull, the same report range loss produces.
    #[test]
    fn dropping_every_claimed_slot_yields_one_loss_per_identity() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 0);
        reg.link_up(2, B, Origin::Incoming, 0);
        let losses: Vec<[u8; 16]> = (0..4).filter_map(|slot| reg.link_down(slot)).collect();
        assert_eq!(losses, vec![A, B]);
        assert!(!reg.is_linked(&A));
        assert!(!reg.is_linked(&B));
    }

    /// Same teardown mid-displacement-hand-over: two links, one
    /// identity. One loss, not two — the peer left once.
    #[test]
    fn a_displaced_identity_is_lost_once_when_all_slots_drop() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 0);
        reg.link_up(1, A, Origin::Incoming, LINK_TIMEOUT_MS);
        let losses: Vec<[u8; 16]> = (0..4).filter_map(|slot| reg.link_down(slot)).collect();
        assert_eq!(losses, vec![A]);
    }

    /// The #372 shape: every slot filled by a different identity — a
    /// phone and three boards. Each is an arrival, the count says four,
    /// and losing one leaves the other three linked and untouched.
    #[test]
    fn n_identities_fill_n_slots_and_one_loss_leaves_n_minus_one() {
        const C: [u8; 16] = [0xcc; 16];
        const D: [u8; 16] = [0xdd; 16];
        let mut reg = PeerRegistry::<4>::new();
        for (slot, id) in [A, B, C, D].into_iter().enumerate() {
            assert_eq!(
                reg.link_up(slot, id, Origin::Incoming, 0),
                LinkUp::Accepted { first: true },
                "each identity's first link"
            );
        }
        assert_eq!(reg.peer_count(), 4);

        assert_eq!(reg.link_down(1), Some(B), "B's only link is a loss");
        assert_eq!(reg.peer_count(), 3);
        for id in [A, C, D] {
            assert!(reg.is_linked(&id), "the others are untouched");
        }
        assert!(!reg.is_linked(&B));
    }

    /// The #375 §0 exclusion input: a connection's address is known
    /// from the connection event, before any identity arrives, and
    /// clears at teardown.
    #[test]
    fn conn_addresses_are_tracked_from_connect_to_teardown() {
        let mut reg = PeerRegistry::<4>::new();
        assert!(!reg.addr_linked(0xC0DE));

        // The rig's exact gap: connected, identity not yet presented.
        reg.conn_up(1, 0xC0DE);
        assert!(reg.addr_linked(0xC0DE), "excluded before any dial");
        assert!(!reg.addr_linked(0xBEEF));

        // Identity arrives; the address side is unaffected.
        assert_eq!(
            reg.link_up(1, A, Origin::Incoming, 0),
            LinkUp::Accepted { first: true }
        );
        assert!(reg.addr_linked(0xC0DE));

        // Teardown clears both facts independently.
        assert_eq!(reg.link_down(1), Some(A));
        assert!(
            reg.addr_linked(0xC0DE),
            "identity gone, connection fact still set"
        );
        reg.conn_down(1);
        assert!(!reg.addr_linked(0xC0DE));
    }

    /// Two live connections on different slots: clearing one leaves the
    /// other's address linked.
    #[test]
    fn one_teardown_leaves_the_other_connection_linked() {
        let mut reg = PeerRegistry::<4>::new();
        reg.conn_up(0, 0x1111);
        reg.conn_up(2, 0x2222);
        reg.conn_down(0);
        assert!(!reg.addr_linked(0x1111));
        assert!(reg.addr_linked(0x2222));
    }

    /// The mirrored peer count is DISTINCT identities: a displaced
    /// identity on two slots is one peer, and a slot gap does not
    /// confuse the count.
    #[test]
    fn peer_count_is_distinct_identities_across_slot_gaps() {
        let mut reg = PeerRegistry::<4>::new();
        assert_eq!(reg.peer_count(), 0);
        reg.link_up(1, A, Origin::Incoming, 0);
        assert_eq!(reg.peer_count(), 1);
        // The displacement hand-over: same identity on a second slot.
        reg.link_up(3, A, Origin::Incoming, LINK_TIMEOUT_MS);
        assert_eq!(reg.peer_count(), 1, "two links, one peer");
        reg.link_up(0, B, Origin::Incoming, LINK_TIMEOUT_MS);
        assert_eq!(reg.peer_count(), 2);
        reg.link_down(1);
        assert_eq!(reg.peer_count(), 2, "A still holds slot 3");
        reg.link_down(3);
        assert_eq!(reg.peer_count(), 1);
    }

    /// Without a hint the fan-out is a flood, whatever the registry
    /// holds: a broadcast owes every peer on the medium a copy.
    #[test]
    fn a_packet_without_a_hint_floods_every_live_link() {
        let mut reg = PeerRegistry::<4>::new();
        assert_eq!(plan_fanout(&reg, None), TxFanout::Flood, "no links");
        reg.link_up(0, A, Origin::Incoming, 0);
        reg.link_up(1, B, Origin::Incoming, 0);
        assert_eq!(plan_fanout(&reg, None), TxFanout::Flood);
    }

    /// With a hint the packet goes on the hinted peer's link and on no
    /// other — the whole point of #376 part 2.
    #[test]
    fn a_hinted_packet_takes_only_that_peers_link() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, A, Origin::Incoming, 0);
        reg.link_up(2, B, Origin::Incoming, 0);
        assert_eq!(plan_fanout(&reg, Some(&A)), TxFanout::Route(0));
        assert_eq!(plan_fanout(&reg, Some(&B)), TxFanout::Route(2));
    }

    /// A peer holding two links (the displacement hand-over window) is
    /// one peer: either link reaches it, and the decision picks one
    /// rather than duplicating the packet across both.
    #[test]
    fn a_peer_with_two_links_gets_the_packet_once() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(1, A, Origin::Incoming, 0);
        reg.link_up(3, A, Origin::Incoming, LINK_TIMEOUT_MS);
        assert_eq!(plan_fanout(&reg, Some(&A)), TxFanout::Route(1));
    }

    /// The peer walked out between the core's routing decision and this
    /// fan-out: DROP, never a fallback flood. See [`TxFanout::NoLink`].
    #[test]
    fn a_hint_for_a_peer_with_no_link_drops_instead_of_flooding() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(0, B, Origin::Incoming, 0);
        assert_eq!(
            plan_fanout(&reg, Some(&A)),
            TxFanout::NoLink,
            "A is gone; B's link is not a route to A"
        );
        // And with nothing live at all it is still a drop, not a flood.
        reg.link_down(0);
        assert_eq!(plan_fanout(&reg, Some(&A)), TxFanout::NoLink);
    }

    /// The peer's LAST link died: `slot_for` must not keep naming the
    /// slot the teardown released.
    #[test]
    fn slot_for_forgets_a_slot_at_teardown() {
        let mut reg = PeerRegistry::<4>::new();
        reg.link_up(2, A, Origin::Incoming, 0);
        assert_eq!(reg.slot_for(&A), Some(2));
        reg.link_down(2);
        assert_eq!(reg.slot_for(&A), None);
    }
}
