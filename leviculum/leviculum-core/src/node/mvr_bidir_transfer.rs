//! mvr: host-side repro of the bidirectional resource-transfer livelock
//! (Bug B, the TRANSFER phase — distinct from the path-resolve stall modelled
//! in `mvr_lnode_pathresolve`).
//!
//! ## Field failure (LoRa integ `lora_lnode_lncp_bidir_slow`)
//!
//! Two LNodes on the same SX1262 chip, SF10, BIDIRECTIONAL. Path resolution
//! succeeds; the test then wedges 900 s inside the resource TRANSFER while
//! BOTH nodes push a resource to the other at the same time. The prior host
//! repro (`mvr_lnode_pathresolve`) proved the path logic round-trips fine, so
//! it exercises the WRONG layer for this symptom: it ran over a LOSSLESS
//! FULL-DUPLEX in-process medium, which structurally cannot express a
//! loss + half-duplex livelock.
//!
//! ## The livelock mechanism (diagnosed, grounded in code)
//!
//! It is a LIVELOCK, not a deadlock: both nodes exchange packets continuously
//! but neither transfer ever concludes, because BOTH resource watchdogs reset
//! on ANY inbound event rather than on genuine progress:
//!   - sender: `outgoing.rs:537-538` resets `retries`/`last_activity_ms` on
//!     every REQ it receives;
//!   - receiver: `incoming.rs` resets its activity on every received part.
//!
//! So as long as any traffic flows, nothing ever times out and reaps.
//!
//! Prime suspect: `90b2a788` ("track distinct parts sent for the AwaitingProof
//! transition", #85) changed the sender terminate condition from "every
//! transmission counted" to "every DISTINCT part sent at least once"
//! (`outgoing.rs`, `distinct_parts_sent() == parts.len()`). On a lossy
//! half-duplex link the sender now stays in `Transferring` and keeps
//! retransmitting far longer, hogging the shared half-duplex channel and
//! starving the reverse transfer.
//!
//! ## What this harness models
//!
//! A single active link between `alpha` and `beta`, driven by a `MockClock`,
//! over a SHARED HALF-DUPLEX medium:
//!   - only one packet is ever "on the air" at a time (a packet in flight
//!     blocks the reverse direction until it is delivered or dropped);
//!   - each packet costs airtime `len * 1000 / rate_bps` ms, plus a fixed
//!     turnaround whenever the physical direction reverses;
//!   - during the transfer phase ~20 % of on-air packets are DROPPED, chosen
//!     deterministically by on-air index (seeded by index, not RNG, so the run
//!     is byte-stable and reproducible).
//!
//! Both nodes start a ~4 KB resource transfer to the other simultaneously, and
//! the harness pumps `handle_packet`/`handle_timeout` on both up to a bounded
//! simulated-time budget.
//!
//! Two outcomes, both diagnostic:
//!   - RED (both transfers do NOT complete inside the budget): the livelock
//!     reproduces in pure host-side logic, radio-free, which pins the on-device
//!     900 s hang to shared `leviculum-core` code, not radio timing.
//!   - GREEN (both complete): the logic round-trips fine host-side even under
//!     loss + half-duplex, which would pin the failure to genuine on-device
//!     radio timing (a rig question, not a code one).
//!
//! ## Empirical result (2026-07, this investigation)
//!
//! GREEN. The livelock does NOT reproduce host-side. The primary anchor
//! (`bidir_transfer_completes_under_loss_halfduplex`, 342 B/s ~ 2 s RTT, 4 KB
//! each way, 20 % loss) completes both transfers in ~47 s of simulated time.
//! A full sweep (`bidir_sweep`) over rates 342..3000 B/s x loss 0..33 % x sizes
//! 4..16 KB found no livelock: every cell either completes or cleanly hard-
//! fails. The only cells that miss a fixed 120 s budget are 16 KB transfers
//! whose raw one-way airtime at 342 B/s (~48 s, ~96 s for both on half-duplex)
//! already approaches the budget; `bidir_livelock_vs_slow` re-runs them with a
//! 5000 s budget and every one COMPLETES (they were slow, not stuck). So the
//! 900 s device hang is not a pure shared-core livelock: it needs genuine
//! on-device radio timing to manifest (rig, not host). Because Step 1 did not
//! reproduce, the planned `90b2a788` A/B (Step 2) was intentionally NOT run.
//!
//! Sans-I/O: no LoRa, no Docker, no Python, sub-second wall clock.

extern crate std;

use std::collections::VecDeque;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::LinkId;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::resource::{ResourceStatus, ResourceStrategy};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, NoStorage};
use crate::transport::{Action, InterfaceId, TickOutput};

type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

/// Half-duplex turnaround: the fixed cost of flipping the link direction
/// (RX/TX switch plus preamble), a LoRa-ish 100 ms.
const TURNAROUND_MS: u64 = 100;

/// Default simulated-time budget. A healthy pair of 4 KB half-duplex transfers
/// under 20 % loss completes well within this.
const TIME_BUDGET_MS: u64 = 120_000;

/// Tunable run parameters (swept by `bidir_sweep`, fixed for the anchors).
#[derive(Debug, Clone, Copy)]
struct Params {
    /// On-air byte rate (BYTES/s): airtime per packet is `len * 1000 / rate`
    /// ms. Lower = longer airtime per packet, larger link RTT.
    rate_bps: u64,
    /// Drop every on-air packet whose transfer-phase index is
    /// `== drop_phase (mod drop_modulus)`. Keyed by index, not RNG, so the
    /// drop set is identical on every run. `drop_modulus == 0` disables loss.
    drop_modulus: usize,
    drop_phase: usize,
    /// Payload size each node pushes to the other.
    payload_bytes: usize,
    /// Simulated-time budget before the run gives up.
    budget_ms: u64,
}

/// The committed anchor's parameters, at the DEVICE regime the field failure
/// runs in: ~4 KB each way, 20 % deterministic loss, 342 B/s. At 342 B/s a
/// ~450 B part is ~1.3 s of airtime, putting the link RTT into the ~2 s LoRa
/// SF10 scale (as `#123`'s own device test sets it). This is the harshest
/// on-scale config that still fits the budget, and it COMPLETES host-side.
const DEFAULT_PARAMS: Params = Params {
    rate_bps: 342,
    drop_modulus: 5,
    drop_phase: 2,
    payload_bytes: 4096,
    budget_ms: TIME_BUDGET_MS,
};

/// Hard step cap: a safety backstop far above any legitimate run so a harness
/// bug (clock that never advances) fails loudly instead of hanging.
const MAX_STEPS: usize = 2_000_000;

// ----------------------------------------------------------------------------
// Sans-I/O helpers (same pattern as mvr_resource_window / mvr_response_resource).
// ----------------------------------------------------------------------------

fn add_iface(node: &mut EndpointNode, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

/// Deterministic, poorly-compressible payload keyed by a per-node salt so the
/// two directions carry distinct bytes (a receiver that mixed up the two
/// resources would be caught by the equality assert).
fn payload(size: usize, salt: u8) -> Vec<u8> {
    (0..size).map(|i| ((i * 31 + 7) as u8) ^ salt).collect()
}

/// Which node a packet on the shared medium is travelling toward. The physical
/// link direction is fully determined by the target, so a change of target is
/// exactly a half-duplex turnaround.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Alpha,
    Beta,
}

/// A node bundled with its interface index and the resource data it received
/// (from the OTHER node) once its incoming transfer completes.
struct Endpoint {
    node: EndpointNode,
    iface: usize,
    received: Option<Vec<u8>>,
    failed: bool,
}

/// Two `NodeCore`s sharing ONE active link over a single half-duplex medium.
struct BidirMedium {
    alpha: Endpoint,
    beta: Endpoint,
    link_id: LinkId,
    params: Params,
    /// Packets in flight, delivered strictly one at a time (models a channel
    /// that can carry only one transmission at a time — half-duplex).
    medium: VecDeque<(Target, Vec<u8>)>,
    last_dir: Option<Target>,
    /// True once establishment is done and both transfers have been kicked off;
    /// loss only bites during the transfer phase.
    loss_active: bool,
    /// On-air transmission counter during the transfer phase (drives the
    /// deterministic drop pattern).
    air_index: usize,
    /// How many on-air packets the loss pattern actually dropped (proves a
    /// green run genuinely exercised loss + retransmission).
    dropped: usize,
}

impl BidirMedium {
    fn now(&self) -> u64 {
        self.alpha.node.transport.clock().now_ms()
    }

    /// Advance both clocks in lockstep so they behave as one shared clock.
    fn advance(&self, ms: u64) {
        self.alpha.node.transport.clock().advance(ms);
        self.beta.node.transport.clock().advance(ms);
    }

    fn both_done(&self) -> bool {
        self.alpha.received.is_some() && self.beta.received.is_some()
    }

    /// Absorb a node's output: record any resource it received or a failure,
    /// and return the packets it wants to put on the wire.
    fn absorb(target_of_output: Target, ep: &mut Endpoint, out: TickOutput) -> Vec<Vec<u8>> {
        for ev in &out.events {
            match ev {
                NodeEvent::ResourceCompleted {
                    is_sender: false,
                    data,
                    ..
                } => ep.received = Some(data.clone()),
                NodeEvent::ResourceFailed { .. } => ep.failed = true,
                _ => {}
            }
        }
        let _ = target_of_output;
        action_data(&out)
    }

    /// Queue every packet a node emitted onto the shared medium, tagged with the
    /// node it travels to. `from` is the producing node; packets go to the peer.
    fn enqueue_from(&mut self, from: Target, pkts: Vec<Vec<u8>>) {
        let to = match from {
            Target::Alpha => Target::Beta,
            Target::Beta => Target::Alpha,
        };
        for p in pkts {
            self.medium.push_back((to, p));
        }
    }

    /// Deliver one packet with airtime pacing, a turnaround on direction flips,
    /// and deterministic transfer-phase loss. Returns false when the medium is
    /// idle.
    fn deliver_next(&mut self) -> bool {
        let Some((to, pkt)) = self.medium.pop_front() else {
            return false;
        };

        if self.last_dir != Some(to) {
            self.advance(TURNAROUND_MS);
            self.last_dir = Some(to);
        }
        self.advance(pkt.len() as u64 * 1000 / self.params.rate_bps);

        if self.loss_active && self.params.drop_modulus > 0 {
            let idx = self.air_index;
            self.air_index += 1;
            if idx % self.params.drop_modulus == self.params.drop_phase {
                // The frame occupied the air (airtime already elapsed) but
                // arrives corrupt: the target never sees it.
                self.dropped += 1;
                return true;
            }
        }

        let (produced, from) = match to {
            Target::Alpha => {
                let out = self
                    .alpha
                    .node
                    .handle_packet(InterfaceId(self.alpha.iface), &pkt);
                (
                    Self::absorb(Target::Alpha, &mut self.alpha, out),
                    Target::Alpha,
                )
            }
            Target::Beta => {
                let out = self
                    .beta
                    .node
                    .handle_packet(InterfaceId(self.beta.iface), &pkt);
                (
                    Self::absorb(Target::Beta, &mut self.beta, out),
                    Target::Beta,
                )
            }
        };
        self.enqueue_from(from, produced);
        true
    }

    /// The medium is idle but the transfers are unfinished: jump the sim clock
    /// to the earliest node deadline and fire both timeout handlers.
    fn timeout_step(&mut self) {
        let deadline = [
            self.alpha.node.next_deadline(),
            self.beta.node.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(deadline) = deadline else {
            // No packets in flight and no node has a deadline: a true deadlock.
            // Jump past the budget so the caller's time check ends the run.
            self.advance(self.params.budget_ms);
            return;
        };
        let now = self.now();
        self.advance(deadline.saturating_sub(now).max(1));

        let a_out = self.alpha.node.handle_timeout();
        let a_pkts = Self::absorb(Target::Alpha, &mut self.alpha, a_out);
        self.enqueue_from(Target::Alpha, a_pkts);

        let b_out = self.beta.node.handle_timeout();
        let b_pkts = Self::absorb(Target::Beta, &mut self.beta, b_out);
        self.enqueue_from(Target::Beta, b_pkts);
    }

    /// Establish a clean (lossless, full establishment) link alpha -> beta.
    fn establish(&mut self, dest_hash: crate::DestinationHash, signing_key: &[u8; 32]) {
        let (link_id, _routed, out) = self.alpha.node.connect(dest_hash, signing_key);
        self.link_id = link_id;
        let pkts = Self::absorb(Target::Alpha, &mut self.alpha, out);
        self.enqueue_from(Target::Alpha, pkts);

        let mut steps = 0usize;
        while self.deliver_next() {
            steps += 1;
            assert!(steps < MAX_STEPS, "link establishment did not quiesce");
        }
        assert_eq!(self.alpha.node.active_link_count(), 1, "alpha link active");
        assert_eq!(self.beta.node.active_link_count(), 1, "beta link active");
    }

    fn resource_status(&self, which: Target, outgoing: bool) -> Option<ResourceStatus> {
        let ep = match which {
            Target::Alpha => &self.alpha,
            Target::Beta => &self.beta,
        };
        let link = ep.node.links.get(&self.link_id)?;
        if outgoing {
            link.outgoing_resource().map(|r| r.status())
        } else {
            link.incoming_resource().map(|r| r.status())
        }
    }
}

/// A node that owns a link-accepting destination (beta) or a bare initiator
/// (alpha). Both are otherwise identical endpoints.
fn make_listener() -> (EndpointNode, crate::DestinationHash, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["bidir"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    (node, dest_hash, signing_key)
}

fn make_initiator() -> EndpointNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().build(OsRng, clock, NoStorage)
}

/// Outcome of one bidirectional run.
struct BidirResult {
    alpha_received: Option<Vec<u8>>,
    beta_received: Option<Vec<u8>>,
    alpha_failed: bool,
    beta_failed: bool,
    dropped: usize,
    sim_ms: u64,
    /// Resource statuses at the end of the run, for diagnosing a livelock:
    /// (alpha outgoing, alpha incoming, beta outgoing, beta incoming).
    end_status: (
        Option<ResourceStatus>,
        Option<ResourceStatus>,
        Option<ResourceStatus>,
        Option<ResourceStatus>,
    ),
}

/// Establish the link, start both 4 KB transfers simultaneously, and pump both
/// nodes over the shared half-duplex + lossy medium up to the time budget.
fn run_bidir() -> BidirResult {
    run_bidir_with(DEFAULT_PARAMS)
}

fn run_bidir_with(params: Params) -> BidirResult {
    let (beta_node, dest_hash, signing_key) = make_listener();
    let alpha_node = make_initiator();

    let mut alpha = Endpoint {
        node: alpha_node,
        iface: 0,
        received: None,
        failed: false,
    };
    let mut beta = Endpoint {
        node: beta_node,
        iface: 0,
        received: None,
        failed: false,
    };
    alpha.iface = add_iface(&mut alpha.node, "A_mesh");
    beta.iface = add_iface(&mut beta.node, "B_mesh");

    let mut h = BidirMedium {
        alpha,
        beta,
        link_id: LinkId::new([0u8; 16]),
        params,
        medium: VecDeque::new(),
        last_dir: None,
        loss_active: false,
        air_index: 0,
        dropped: 0,
    };

    h.establish(dest_hash, &signing_key);

    // Both sides accept incoming resources: each node is simultaneously a
    // sender (its own resource) and a receiver (the peer's).
    h.alpha
        .node
        .set_resource_strategy(&h.link_id, ResourceStrategy::AcceptAll)
        .expect("alpha link must exist");
    h.beta
        .node
        .set_resource_strategy(&h.link_id, ResourceStrategy::AcceptAll)
        .expect("beta link must exist");

    let alpha_payload = payload(params.payload_bytes, 0xA1);
    let beta_payload = payload(params.payload_bytes, 0xB2);

    // Kick off BOTH transfers before any pumping, so they contend for the
    // half-duplex channel from the first round.
    let (_a_hash, out) = h
        .alpha
        .node
        .send_resource(&h.link_id, &alpha_payload, None, false)
        .expect("alpha send_resource must advertise");
    let pkts = BidirMedium::absorb(Target::Alpha, &mut h.alpha, out);
    h.enqueue_from(Target::Alpha, pkts);

    let (_b_hash, out) = h
        .beta
        .node
        .send_resource(&h.link_id, &beta_payload, None, false)
        .expect("beta send_resource must advertise");
    let pkts = BidirMedium::absorb(Target::Beta, &mut h.beta, out);
    h.enqueue_from(Target::Beta, pkts);

    let t0 = h.now();
    h.loss_active = true;

    let mut steps = 0usize;
    while !h.both_done() && h.now().saturating_sub(t0) < params.budget_ms {
        steps += 1;
        assert!(steps < MAX_STEPS, "bidir pump exceeded the safety step cap");
        if !h.deliver_next() {
            h.timeout_step();
        }
    }

    let end_status = (
        h.resource_status(Target::Alpha, true),
        h.resource_status(Target::Alpha, false),
        h.resource_status(Target::Beta, true),
        h.resource_status(Target::Beta, false),
    );

    // Integrity: whatever a node received must be the OTHER node's payload.
    // (A wrong-bytes delivery is a corruption bug regardless of the outcome
    // under study, so this always holds.)
    if let Some(data) = &h.alpha.received {
        assert_eq!(data, &beta_payload, "alpha received the wrong bytes");
    }
    if let Some(data) = &h.beta.received {
        assert_eq!(data, &alpha_payload, "beta received the wrong bytes");
    }

    BidirResult {
        alpha_received: h.alpha.received.clone(),
        beta_received: h.beta.received.clone(),
        alpha_failed: h.alpha.failed,
        beta_failed: h.beta.failed,
        dropped: h.dropped,
        sim_ms: h.now().saturating_sub(t0),
        end_status,
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

/// The headline finding: at the device regime (342 B/s ~= 2 s SF10 RTT), two
/// nodes pushing ~4 KB to each other simultaneously over a shared, LOSSY,
/// HALF-DUPLEX medium BOTH complete, well inside the simulated-time budget.
///
/// This is the crux of the Bug B investigation. The field failure
/// (`lora_lnode_lncp_bidir_slow`) hangs 900 s inside exactly this bidirectional
/// transfer, and the diagnosed mechanism was a livelock — both resource
/// watchdogs reset on any inbound event, so a sender that keeps retransmitting
/// on the shared half-duplex channel could in theory starve the reverse
/// transfer forever. This harness reproduces loss + half-duplex + the 2 s RTT
/// scale radio-free, and NO livelock appears: the transfers complete.
///
/// A full parameter sweep (`bidir_sweep`) and a livelock-vs-slow probe
/// (`bidir_livelock_vs_slow`) confirm the negative across rates 342..3000 B/s,
/// loss 0..33 %, sizes 4..16 KB: every configuration either COMPLETES (given
/// enough time) or cleanly HARD-FAILS, but none livelocks. The only
/// non-completions inside a fixed budget are large transfers whose raw airtime
/// alone exceeds the budget (slow, not stuck) — the probe shows they finish
/// when the budget is lifted.
///
/// The conclusion this anchor pins: the 900 s device hang does NOT reproduce in
/// the sans-I/O core, so it is not a pure shared-core-logic livelock; it needs
/// genuine on-device radio timing (CAD/CSMA channel capture, real RX/TX
/// switching, hardware RTT feedback) to manifest — a rig question, not a
/// host-side one. If this ever goes RED (a real host-side bidir livelock
/// emerges), that is a genuine regression to chase.
#[test]
fn bidir_transfer_completes_under_loss_halfduplex() {
    let r = run_bidir();
    assert!(
        r.dropped > 0,
        "the loss pattern must actually drop packets for this to be a \
         meaningful loss test (dropped {})",
        r.dropped
    );
    assert!(
        !r.alpha_failed && !r.beta_failed,
        "neither transfer should hard-fail at this config \
         (alpha_failed={} beta_failed={})",
        r.alpha_failed,
        r.beta_failed
    );
    assert!(
        r.alpha_received.is_some() && r.beta_received.is_some(),
        "bidirectional transfer did NOT complete both ways within {} ms sim \
         time (alpha_received={} beta_received={}, dropped {}, end statuses \
         alpha_out={:?} alpha_in={:?} beta_out={:?} beta_in={:?}). If this is \
         RED, a genuine host-side bidirectional livelock has emerged: both \
         resources stuck while the watchdogs reset on every inbound event.",
        r.sim_ms,
        r.alpha_received.is_some(),
        r.beta_received.is_some(),
        r.dropped,
        r.end_status.0,
        r.end_status.1,
        r.end_status.2,
        r.end_status.3,
    );
}

/// Decisive livelock-vs-slow probe: for the non-completing cells the 120 s
/// budget is suspiciously close to the raw airtime (16 KB at 342 B/s is ~48 s
/// one-way, ~96 s for both directions on half-duplex even losslessly), so a
/// failure to complete in 120 s could be a merely SLOW transfer, not a
/// livelock. This re-runs those cells with an enormous budget: if they then
/// COMPLETE, the transfer was progressing all along (slow, not stuck); if they
/// still never complete, it is a genuine livelock. Run with:
/// `cargo test -p leviculum-core --lib bidir_livelock_vs_slow -- --ignored --nocapture`.
#[test]
#[ignore = "investigation probe, run manually with --nocapture"]
fn bidir_livelock_vs_slow() {
    // The cells from bidir_sweep that did NOT complete both ways in 120 s.
    let cells: [(u64, usize, usize, usize); 4] = [
        (342, 3, 1, 16384),  // 33 % loss, both stuck
        (342, 5, 2, 16384),  // 20 % loss, both stuck
        (342, 10, 3, 16384), // 10 % loss, alpha stuck
        (700, 3, 1, 16384),  // 33 % loss, alpha stuck
    ];
    // 10x the LoRa-slow airtime ceiling: if progress is being made at all,
    // this is more than enough to finish; if it is a livelock, it never will.
    let huge_budget = 5_000_000;
    std::eprintln!("BIDIR-PROBE rate loss size | a_in b_in sim_ms verdict");
    for (rate, dm, dp, size) in cells {
        let p = Params {
            rate_bps: rate,
            drop_modulus: dm,
            drop_phase: dp,
            payload_bytes: size,
            budget_ms: huge_budget,
        };
        let r = run_bidir_with(p);
        let both = r.alpha_received.is_some() && r.beta_received.is_some();
        let verdict = if both {
            "SLOW (completed given time)"
        } else {
            "LIVELOCK (never completes)"
        };
        let loss_pct = 100 / dm;
        std::eprintln!(
            "BIDIR-PROBE rate={rate} loss={loss_pct} size={size} | a_in={} b_in={} sim_ms={} => {verdict} status=({:?},{:?},{:?},{:?})",
            r.alpha_received.is_some(),
            r.beta_received.is_some(),
            r.sim_ms,
            r.end_status.0,
            r.end_status.1,
            r.end_status.2,
            r.end_status.3,
        );
    }
}

/// Investigation sweep (not a pass/fail anchor): probe whether ANY host-side
/// regime — slower rates (larger RTT, toward the ~2 s device scale), heavier
/// loss, larger payloads — turns the bidirectional transfer into a livelock.
/// Prints one line per cell to stderr. Run with:
/// `cargo test -p leviculum-core --lib bidir_sweep -- --ignored --nocapture`.
#[test]
#[ignore = "investigation sweep, run manually with --nocapture"]
fn bidir_sweep() {
    // Rates spanning ~150 ms/part (3000 B/s) down to ~1.3 s/part (342 B/s),
    // the latter putting the link RTT into the ~2 s device regime.
    let rates: [u64; 4] = [3000, 1500, 700, 342];
    // None, 10 %, 20 %, 33 % deterministic loss.
    let drops: [(usize, usize); 4] = [(0, 0), (10, 3), (5, 2), (3, 1)];
    let sizes: [usize; 2] = [4096, 16384];

    std::eprintln!("BIDIR-SWEEP rate loss size | alpha_in beta_in sim_ms (a_out,a_in,b_out,b_in)");
    for &rate in &rates {
        for &(dm, dp) in &drops {
            for &size in &sizes {
                let p = Params {
                    rate_bps: rate,
                    drop_modulus: dm,
                    drop_phase: dp,
                    payload_bytes: size,
                    budget_ms: TIME_BUDGET_MS,
                };
                let r = run_bidir_with(p);
                let loss_pct = 100usize.checked_div(dm).unwrap_or(0);
                std::eprintln!(
                    "BIDIR-SWEEP rate={rate} loss={loss_pct} size={size} | a_in={} b_in={} a_fail={} b_fail={} sim_ms={} status=({:?},{:?},{:?},{:?})",
                    r.alpha_received.is_some(),
                    r.beta_received.is_some(),
                    r.alpha_failed,
                    r.beta_failed,
                    r.sim_ms,
                    r.end_status.0,
                    r.end_status.1,
                    r.end_status.2,
                    r.end_status.3,
                );
            }
        }
    }
}
