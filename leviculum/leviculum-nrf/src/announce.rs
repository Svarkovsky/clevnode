//! When this board announces itself, and to whom (Codeberg #376).
//!
//! Until #376 a board announced its `lxmf.delivery` destination at
//! exactly one moment: immediately before a telemetry report
//! ([`crate::telemetry::Reporter::tick`]). Two consequences the field
//! reported on 2026-09-09 — a phone that connects between two reports
//! learns nothing about the board it is linked to, and a board with no
//! target (or an unreachable one) never announces at all, so an operator
//! watching a stationary relay saw an hour with no announce and could
//! not judge its hop count.
//!
//! This is the board's half of the answer. Two occasions:
//!
//! * [`AnnounceGate::peer_up`] — a BLE peer finished its identity
//!   handshake. The announce is addressed at THAT peer
//!   (`NodeCore::announce_destination_to_peer`), so it travels on that
//!   link alone. Broadcasting it is what produced the two-hop reading
//!   this issue is named for: the neighbour board gets a copy it was
//!   never meant to have, rebroadcasts it, and the relayed announce
//!   races the direct one into the phone's path table.
//! * [`AnnounceGate::periodic`] — the timer, on every interface, so a
//!   board that reports rarely is still findable.
//!
//! Both send the announce the telemetry path sends: same destination,
//! same [`crate::telemetry::announce_app_data`], same clock gate. One
//! announce shape exists on this board and this is it; the `lnflash
//! --announce` arm in the binaries is the third caller of the same pair.
//!
//! The WHEN is not decided here either — the cadence, the per-peer rate
//! limit and the clock gate live in [`leviculum_announce_policy`], which
//! is host-tested and which `lnsd` uses verbatim. This module is the
//! wiring: read the gate, build the announce, write the line.

use leviculum_announce_policy::{Decision, PeerAnnounceLimiter, PeriodicAnnounce, Withheld};
use leviculum_core::node::NodeCore;
use leviculum_core::traits::{Clock, Storage};
use leviculum_core::transport::Action;
use leviculum_core::DestinationHash;
use rand_core::CryptoRngCore;

extern crate alloc;
use alloc::vec::Vec;

/// How many peer identities the rate limit remembers.
///
/// `MAX_LINKS` is 4 on both boards (`crate::ble::MAX_LINKS`), so eight
/// slots hold every currently linked peer plus four that have come and
/// gone inside the fifteen-minute window. Overflowing it costs one extra
/// announce on one link, never a wrongly withheld one — see
/// [`PeerAnnounceLimiter`].
const PEER_SLOTS: usize = 8;

/// The BLE interface's index in the board's interface table. Serial is 0,
/// LoRa 1, BLE 2 on both binaries; the peer-up edge only ever comes from
/// BLE, and the announce it triggers has to name the same interface the
/// peer is behind.
const BLE_IFACE: usize = 2;

/// The board's announce occasions, and the gates on them.
pub struct AnnounceGate {
    peers: PeerAnnounceLimiter<PEER_SLOTS>,
    periodic: PeriodicAnnounce,
    /// The last critical withhold reason written, so a standing
    /// condition is stated once and not once a minute for as long as it
    /// lasts. Same rule, and the same argument, as
    /// [`crate::telemetry::Reporter`]'s `last_withheld`: a silent board
    /// has to be legible in a log tail, which a flood is not.
    last_withheld: Option<Withheld>,
}

impl AnnounceGate {
    /// Arm the periodic announce relative to `now_ms` (boot).
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self {
            peers: PeerAnnounceLimiter::new(),
            periodic: PeriodicAnnounce::new(now_ms),
            last_withheld: None,
        }
    }

    /// How long the caller may sleep before the next [`Self::periodic`]
    /// is worth calling.
    #[must_use]
    pub fn periodic_wait_ms(&self, now_ms: u64) -> u64 {
        self.periodic.wait_ms(now_ms)
    }

    /// A BLE peer completed its identity handshake: announce to it, on
    /// its link alone.
    ///
    /// Returns the actions to dispatch — empty when the gate withheld,
    /// when this board registered no delivery destination, or when the
    /// announce could not be built. The packet goes through the ordinary
    /// BLE outbound path, so the peripheral hold (`leviculum_ble_tx::hold`)
    /// makes it WAIT in the link's queue until the peer has subscribed
    /// rather than failing as an early notify.
    pub fn peer_up<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        delivery_hash: Option<&DestinationHash>,
        peer: [u8; 16],
    ) -> Vec<Action>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let Some(hash) = delivery_hash else {
            return Vec::new();
        };
        let now_ms = node.now_ms();
        match self
            .peers
            .peer_up(peer, now_ms, node.has_plausible_wall_clock())
        {
            Decision::Announce => {
                self.last_withheld = None;
                let app_data = crate::telemetry::announce_app_data(node.identity());
                match node.announce_destination_to_peer(hash, Some(&app_data), BLE_IFACE, peer) {
                    Ok(out) => {
                        log_sent(hash, "peer-up", Some(&peer));
                        out.actions
                    }
                    Err(_) => Vec::new(),
                }
            }
            Decision::Withheld(reason) => {
                self.log_withheld(reason, Some(&peer));
                Vec::new()
            }
        }
    }

    /// The periodic tick. Call it whenever the loop wakes; it is a no-op
    /// before its own deadline, so the caller does not have to be exact.
    pub fn periodic<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        delivery_hash: Option<&DestinationHash>,
    ) -> Vec<Action>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let now_ms = node.now_ms();
        let clock_ok = node.has_plausible_wall_clock();
        let Some(decision) = self.periodic.poll(now_ms, clock_ok) else {
            return Vec::new();
        };
        let Some(hash) = delivery_hash else {
            return Vec::new();
        };
        match decision {
            Decision::Announce => {
                self.last_withheld = None;
                let app_data = crate::telemetry::announce_app_data(node.identity());
                match node.announce_destination(hash, Some(&app_data)) {
                    Ok(out) => {
                        log_sent(hash, "periodic", None);
                        out.actions
                    }
                    Err(_) => Vec::new(),
                }
            }
            Decision::Withheld(reason) => {
                self.log_withheld(reason, None);
                Vec::new()
            }
        }
    }
}

/// `[ANNOUNCE] sent dst=<hex8> reason=<r> [peer=<hex8>]`, the line the
/// desk recipe greps for. Critical, like the `reason=host` line the
/// `lnflash --announce` arm already writes: an announce is the event a
/// capture is opened for.
fn log_sent(hash: &DestinationHash, reason: &str, peer: Option<&[u8; 16]>) {
    let d = hash.as_bytes();
    crate::log::log_fmt_critical(
        "[INFO!] ",
        format_args!(
            "[ANNOUNCE] sent dst={:02x}{:02x}{:02x}{:02x} reason={}{}",
            d[0],
            d[1],
            d[2],
            d[3],
            reason,
            Peer(peer)
        ),
    );
}

/// The withheld sibling. `no-clock` is critical — it is why a board is
/// silent, and an operator hunting silence must see it without raising
/// the log level; it is written once per change, not once per retry.
/// `rate-limited` is not critical: a phone rotating its BLE address
/// relinks about once a minute, and one critical line per minute per
/// peer would bury the events a capture is taken for.
impl AnnounceGate {
    fn log_withheld(&mut self, reason: Withheld, peer: Option<&[u8; 16]>) {
        match reason {
            Withheld::NoClock => {
                if self.last_withheld == Some(reason) {
                    return;
                }
                self.last_withheld = Some(reason);
                crate::log::log_fmt_critical(
                    "[INFO!] ",
                    format_args!(
                        "[ANNOUNCE] withheld reason={}{}",
                        reason.as_str(),
                        Peer(peer)
                    ),
                );
            }
            Withheld::RateLimited => crate::log::log_fmt(
                "[BLE  ] ",
                format_args!(
                    "[ANNOUNCE] withheld reason={}{}",
                    reason.as_str(),
                    Peer(peer)
                ),
            ),
        }
    }
}

/// The optional ` peer=<hex8>` tail of an `[ANNOUNCE]` line: written
/// when the occasion names a peer, absent when it does not. A formatter
/// rather than two format strings, so the two occasions cannot drift
/// into two spellings of the same field.
struct Peer<'a>(Option<&'a [u8; 16]>);

impl core::fmt::Display for Peer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(p) => write!(f, " peer={:02x}{:02x}{:02x}{:02x}", p[0], p[1], p[2], p[3]),
            None => Ok(()),
        }
    }
}
