//! The #375 simulation as a host test: N boards with random addresses,
//! random arrival orders, the REAL initiation rule
//! ([`leviculum_ble_tx::should_initiate`]) and the real slot limits —
//! three incoming links, one outgoing, a full board stops advertising
//! (#372), a board whose outgoing link is up stops scanning, waiting
//! boards rescan every round, and the fallback switches a board's scan
//! mode after a bounded number of empty rounds, exactly as the
//! firmware's clock does after `SCAN_FALLBACK_AFTER_MS`.
//!
//! The harness is calibrated against the issue's own Monte Carlo: under
//! the strict rule it reproduces the published disconnection rates
//! exactly (21 % of orders at 10 boards, 40 % at 20 — see the control
//! below). Part 2 of the batch turned it into a 2×2 instrument over the
//! two open policy questions:
//!
//! - **When may the fallback fire?** [`FallbackSpec::Eager`]: the clock
//!   runs whenever the outgoing slot is free — the shipped spec since
//!   part 3. [`FallbackSpec::Quiet`]: only while the board has no live
//!   link in either role — shipped briefly in part 2, kept here as the
//!   record.
//! - **Which eligible advertiser is dialled?**
//!   [`TargetChoice::FirstSeen`]: whichever eligible PDU the radio
//!   heard first (as first built; seeded random here).
//!   [`TargetChoice::LowestEligible`]: collect one scan window and dial
//!   the lowest-addressed candidate, strict verdicts before fallback
//!   verdicts, via the same [`CandidateTable`] the firmware and lnsd
//!   use — the policy is shared by construction.
//!
//! Measured on this seed stream (disconnected/linkless orders per 1000):
//!
//! | spec  | choice   | n=10        | n=20        |
//! |-------|----------|-------------|-------------|
//! | eager | first    | 2 / 0       | 48 / 0      |
//! | eager | lowest   | 0 / 0       | 0 / 0       |
//! | quiet | first    | 126 / 0     | 328 / 0     |
//! | quiet | lowest   | 28 / 0      | 78 / 0      |
//! | strict (control) | 210 / 84    | 400 / 80    |
//!
//! What the assertions below hold on to:
//!
//! - **The lowest-eligible window closes the saturated-cycle lock**:
//!   eager/lowest is 0/1000 at both sizes, and under quiet it cuts the
//!   splits by ~4× against first-seen.
//! - **The quiet spec has a real, bounded cost in this harness**:
//!   28/1000 and 78/1000 disconnected orders against eager/lowest's
//!   0/0 — well beyond noise. The mechanism: quiet suppresses exactly
//!   the cross-component merge dial. A component whose boards all hold
//!   SOME link (so their clocks are suspended) but lose the sort
//!   against the other component's advertisers can never initiate the
//!   merge, and when no strict edge exists in either direction the two
//!   components are stable disjoint. Every residual split is of this
//!   all-linked kind — the linkless column stays 0, so #375's headline
//!   failure (a board with no BLE link at all, scanning forever) never
//!   returns.
//!
//! Part 2 shipped quiet anyway, because the rig had shown an eager
//! fallback dialling a peer it was already linked to every ~20 s,
//! forever (§0 of the 2026-09-09 batch): a dial the Core Spec dooms
//! (Vol 6 Part B §4.5, one connection per address pair), spent on air
//! every cycle. Part 3 shipped eager after all — that doomed-dial
//! cycle is now closed at its root, not by the clock: a live
//! connection's address is excluded before it can leave the scanner
//! (the registry's `addr_linked`, the §4.5 exclusion) and a fallback
//! dial that cannot even connect goes into the dead-end table for two
//! minutes. With the cycle gone, quiet bought nothing this table does
//! not take away, and the 28/78 all-linked splits it cost are exactly
//! the merges eager performs.

use leviculum_ble_tx::{
    should_initiate, CandidateTable, ConnectDecision, ScanMode, WINDOW_CANDIDATES,
};

/// The firmware's incoming-slot count (`PERIPH_LINKS`, #372).
const PERIPH_SLOTS: usize = 3;

/// The fallback bound, in scan rounds. The firmware bounds the strict
/// search in time (`SCAN_FALLBACK_AFTER_MS` = 30 s over 5 s retry
/// cycles, so about six passes); one sim round is one scan pass, hence
/// six empty rounds before a board's mode flips. The lock rate barely
/// moves with this bound (checked from 6 to 50 rounds), so its exact
/// value is not what the assertions lean on.
const FALLBACK_AFTER_ROUNDS: u32 = 6;

/// Seeded orders per claim. The strict rule's failure rate at 10 boards
/// is about one order in five, so a thousand orders leaves a vanishing
/// chance of the control finding nothing.
const ORDERS: u64 = 1_000;

/// When the fallback clock may run (#375 part 2, item 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackSpec {
    /// No fallback at all: the strict rule as published (the control).
    Off,
    /// The shipped spec (part 3): the clock runs whenever the outgoing
    /// slot is free, live incoming links notwithstanding.
    Eager,
    /// Part 2's spec, kept as the record: the clock is suspended (held
    /// at zero) while the board has ANY live link in either role; only
    /// a fully linkless board may dial against the sort.
    Quiet,
}

/// Which eligible advertiser a scanning board dials (#375 part 2, item 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetChoice {
    /// Whichever eligible PDU the radio heard first — arbitrary, so
    /// seeded random here (as the firmware behaved before the window).
    FirstSeen,
    /// One scan window collected, then the lowest-addressed eligible
    /// candidate, strict verdicts before fallback verdicts — the real
    /// [`CandidateTable`] policy.
    LowestEligible,
}

/// xorshift64* — deterministic, seedable, no dependency.
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

struct Board {
    /// 48-bit static-random address, distinct per board.
    addr: u64,
    arrived: bool,
    /// The one central link: index of the dialled board.
    outgoing: Option<usize>,
    /// Peripheral links, at most [`PERIPH_SLOTS`].
    incoming: Vec<usize>,
    /// Empty scan rounds since the last link-up or permitted peer —
    /// the sim's copy of the firmware's fallback clock.
    strict_rounds: u32,
}

/// Whether the two boards already hold a link in either direction:
/// board addresses are static, so the pre-dial address exclusion
/// (Core Spec §4.5) keeps a linked board from ever being dialled
/// again, and the sim never forms a second link. (An identity
/// duplicate arriving over a ROTATED address — a phone — is decided by
/// who opened it, #382; static-address boards cannot reach that path at
/// all.)
fn linked(boards: &[Board], a: usize, b: usize) -> bool {
    boards[a].outgoing == Some(b) || boards[b].outgoing == Some(a)
}

/// Replay one arrival order to quiescence and return the final boards.
fn run_sim(n: usize, seed: u64, spec: FallbackSpec, choice: TargetChoice) -> Vec<Board> {
    let mut rng = seed | 1;
    let mut boards: Vec<Board> = Vec::with_capacity(n);
    while boards.len() < n {
        let addr = (next_rand(&mut rng) & 0xFFFF_FFFF_FFFF) | 0xC000_0000_0000;
        if boards.iter().any(|b| b.addr == addr) {
            continue;
        }
        boards.push(Board {
            addr,
            arrived: false,
            outgoing: None,
            incoming: Vec::new(),
            strict_rounds: 0,
        });
    }

    // The arrival order under test: a seeded shuffle, one per round.
    let mut arrival: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        arrival.swap(i, (next_rand(&mut rng) as usize) % (i + 1));
    }

    let mut linkless_streak: u32 = 0;
    for round in 0..10_000 {
        if round < n {
            boards[arrival[round]].arrived = true;
        }

        // Scan order within the round is part of the replayed
        // randomness: which searching board wins a contended slot.
        let mut scan_order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            scan_order.swap(i, (next_rand(&mut rng) as usize) % (i + 1));
        }

        let mut any_link = false;
        for i in scan_order {
            // Linked centrals do not scan; unarrived boards are absent.
            if !boards[i].arrived || boards[i].outgoing.is_some() {
                continue;
            }
            // The quiet spec suspends the clock while ANY link is live
            // (part 2's firmware reset it on every scan pass that found
            // a live connection); an outgoing link already stopped the
            // scan above, so incoming links are what decides here.
            let suspended = spec == FallbackSpec::Quiet && !boards[i].incoming.is_empty();
            let mode = if spec != FallbackSpec::Off
                && !suspended
                && boards[i].strict_rounds >= FALLBACK_AFTER_ROUNDS
            {
                ScanMode::Fallback
            } else {
                ScanMode::Strict
            };
            // Visible: arrived, advertising (a full board is not), not
            // ourselves, not already linked to us; then the real rule.
            let candidates: Vec<(usize, ConnectDecision)> = (0..n)
                .filter_map(|p| {
                    if p == i
                        || !boards[p].arrived
                        || boards[p].incoming.len() >= PERIPH_SLOTS
                        || linked(&boards, i, p)
                    {
                        return None;
                    }
                    let decision =
                        should_initiate(0, boards[i].addr, Some(0), boards[p].addr, mode);
                    decision.initiate().then_some((p, decision))
                })
                .collect();
            if candidates.is_empty() {
                if suspended {
                    boards[i].strict_rounds = 0;
                } else {
                    boards[i].strict_rounds += 1;
                }
            } else {
                let target = match choice {
                    // The pre-window firmware dialled whichever eligible
                    // PDU it saw first, so the pick is arbitrary: seeded
                    // random.
                    TargetChoice::FirstSeen => {
                        candidates[(next_rand(&mut rng) as usize) % candidates.len()].0
                    }
                    // One round IS one collected window here: every
                    // eligible advertiser was heard, the table chooses.
                    TargetChoice::LowestEligible => {
                        let mut window: CandidateTable<usize, WINDOW_CANDIDATES> =
                            CandidateTable::new();
                        for &(p, decision) in &candidates {
                            window.offer(boards[p].addr, decision, p);
                        }
                        window
                            .into_best()
                            .map(|(_, _, p)| p)
                            .expect("a non-empty candidate set chooses")
                    }
                };
                boards[i].outgoing = Some(target);
                boards[target].incoming.push(i);
                // A link up resets the clock in either role, as the
                // firmware does on the connection event.
                boards[i].strict_rounds = 0;
                boards[target].strict_rounds = 0;
                any_link = true;
            }
        }

        linkless_streak = if any_link { 0 } else { linkless_streak + 1 };
        // Quiescent: everyone has arrived and even the boards that
        // reached fallback during the streak found nobody. Visibility
        // only changes when a link forms (a quiet-suspended board's
        // links never drop here), so nothing changes hereafter.
        if round >= n && linkless_streak > FALLBACK_AFTER_ROUNDS {
            break;
        }
    }

    // The churn rule held: no pair ever holds two links.
    for a in 0..n {
        if let Some(b) = boards[a].outgoing {
            assert_ne!(boards[b].outgoing, Some(a), "duplicate pair link");
        }
        assert!(boards[a].incoming.len() <= PERIPH_SLOTS);
    }
    boards
}

/// Connectivity over the undirected link graph.
fn is_connected(boards: &[Board]) -> bool {
    let n = boards.len();
    let mut seen = vec![false; n];
    let mut queue = vec![0usize];
    seen[0] = true;
    while let Some(at) = queue.pop() {
        for (next, seen_next) in seen.iter_mut().enumerate() {
            if !*seen_next && linked(boards, at, next) {
                *seen_next = true;
                queue.push(next);
            }
        }
    }
    seen.iter().all(|&s| s)
}

/// One configuration's rates over [`ORDERS`] seeded orders.
struct Outcome {
    /// Orders whose final graph is not one connected component.
    disconnected: usize,
    /// Orders where some board ended with no BLE link at all.
    linkless: usize,
}

fn measure(n: usize, spec: FallbackSpec, choice: TargetChoice) -> Outcome {
    let mut outcome = Outcome {
        disconnected: 0,
        linkless: 0,
    };
    for seed in 0..ORDERS {
        let boards = run_sim(n, 0xB1E5_0000 + seed, spec, choice);
        if !is_connected(&boards) {
            outcome.disconnected += 1;
        }
        if boards
            .iter()
            .any(|b| b.outgoing.is_none() && b.incoming.is_empty())
        {
            outcome.linkless += 1;
        }
    }
    outcome
}

/// The batch's two-spec table (item 1): both fallback specs, with and
/// without the lowest-eligible window, at both sizes, plus the strict
/// baseline — printed for the record (`--nocapture`), with the
/// load-bearing cells asserted:
///
/// - eager/lowest yields 0/1000 disconnected orders at both sizes —
///   the saturated-cycle lock is a first-seen artefact and the window
///   removes it entirely;
/// - the quiet spec's cost against eager/lowest is exactly the pinned
///   28 and 78 orders (the module docs say why, and why part 3 shipped
///   eager over it); the seed stream is fixed, so equality, like the
///   calibration cells;
/// - no fallback configuration ever leaves a board linkless — #375's
///   headline failure stays gone under every spec;
/// - the calibration cells still match the published measurements
///   (eager/first: 2 and 48), so the instrument itself has not moved.
#[test]
fn the_two_spec_table_the_window_closes_the_lock_and_quiet_costs_a_pinned_rest() {
    let configs = [
        (FallbackSpec::Off, TargetChoice::FirstSeen, "strict"),
        (FallbackSpec::Eager, TargetChoice::FirstSeen, "eager/first"),
        (
            FallbackSpec::Eager,
            TargetChoice::LowestEligible,
            "eager/lowest",
        ),
        (FallbackSpec::Quiet, TargetChoice::FirstSeen, "quiet/first"),
        (
            FallbackSpec::Quiet,
            TargetChoice::LowestEligible,
            "quiet/lowest",
        ),
    ];
    let mut rates = std::collections::HashMap::new();
    println!("spec/choice     n=10 disc/linkless   n=20 disc/linkless   (per {ORDERS})");
    for (spec, choice, label) in configs {
        let at10 = measure(10, spec, choice);
        let at20 = measure(20, spec, choice);
        println!(
            "{label:<15} {:>4} / {:<10} {:>4} / {:<10}",
            at10.disconnected, at10.linkless, at20.disconnected, at20.linkless
        );
        rates.insert(label, (at10, at20));
    }

    let (at10, at20) = &rates["eager/lowest"];
    assert_eq!(
        (at10.disconnected, at20.disconnected),
        (0, 0),
        "eager/lowest: the lowest-eligible window must close the saturation lock"
    );
    let (at10, at20) = &rates["quiet/lowest"];
    assert_eq!(
        (at10.disconnected, at20.disconnected),
        (28, 78),
        "quiet/lowest moved: the quiet spec's documented cost is stale, re-measure"
    );
    for label in ["eager/first", "eager/lowest", "quiet/first", "quiet/lowest"] {
        let (at10, at20) = &rates[label];
        assert_eq!(
            (at10.linkless, at20.linkless),
            (0, 0),
            "{label}: a fallback spec left some board with no BLE link at all"
        );
    }
    let (cal10, cal20) = &rates["eager/first"];
    assert_eq!(
        (cal10.disconnected, cal20.disconnected),
        (2, 48),
        "the calibration cells moved: the instrument changed, re-measure everything"
    );
}

/// The shipped configuration (eager fallback + lowest-eligible window,
/// #375 part 3) at both sizes: no board is EVER left without a BLE
/// link, and no arrival order ends disconnected — every order forms one
/// connected component. Eager is safe to ship because the doomed dial
/// that forced part 2's quiet spec is closed at its root: the §4.5
/// exclusion keeps a live connection's address out of the scanner and
/// the dead-end table backs off a fallback target that will not
/// connect. The quiet rows in the module table stay as the record of
/// what the suspension cost (28 and 78 all-linked splits per 1000).
#[test]
fn the_shipped_config_eager_lowest_connects_every_order_and_strands_nobody() {
    for n in [10usize, 20] {
        let mut split = 0usize;
        for seed in 0..ORDERS {
            let boards = run_sim(
                n,
                0xB1E5_0000 + seed,
                FallbackSpec::Eager,
                TargetChoice::LowestEligible,
            );
            for (i, b) in boards.iter().enumerate() {
                assert!(
                    b.outgoing.is_some() || !b.incoming.is_empty(),
                    "board {i} ended with no BLE link at n={n}, seed {seed}"
                );
            }
            if !is_connected(&boards) {
                split += 1;
            }
        }
        assert_eq!(
            split, 0,
            "eager/lowest left {split} of {ORDERS} orders disconnected at n={n}"
        );
    }
}

/// The control: identical harness, identical seeds, fallback off — the
/// strict sort alone must strand boards for a large share of orders, or
/// the fallback tests above prove nothing about the fallback. The
/// measured rate is 210/1000 disconnected orders (the issue's Monte
/// Carlo says 21 %), 84 of which leave some board with no link at all.
#[test]
fn control_the_strict_rule_alone_disconnects_a_fifth_of_the_orders() {
    let outcome = measure(10, FallbackSpec::Off, TargetChoice::FirstSeen);
    assert!(
        outcome.disconnected >= 100,
        "the strict rule connected almost every order ({}/{ORDERS} lost); \
         the control lost its teeth",
        outcome.disconnected
    );
    assert!(
        outcome.linkless >= 50,
        "strict orders with a fully linkless board: {}/{ORDERS}",
        outcome.linkless
    );
}
