//! GNSS presence as a runtime tri-state (Codeberg #240).
//!
//! Whether a board *can* carry a GNSS receiver is a build-time fact (the
//! `gnss` cargo feature routes the UART). Whether one *is attached and
//! delivering* is a runtime question with three answers:
//!
//! - **NoHardware** — no line activity at all across a full baud sweep.
//!   Operator action: check the wiring.
//! - **NoFix** — sentences parse, but no valid RMC. Operator action: wait
//!   (indoors, cold start).
//! - **Fix** — a valid RMC with position and UTC. Only this state may
//!   feed a position or a timebase.
//!
//! On top of the tri-state it aggregates GSV — satellites in view and
//! best C/N0 (Codeberg #324). Those two numbers are the instrument that
//! separates "the module is misconfigured" from "the antenna is deaf"
//! while presence still reads NoFix: `sat=` in the heartbeat is GGA
//! satellites-in-USE and is legitimately 0 without a fix, so it says
//! nothing about the RF path. In-view plus C/N0 does.
//!
//! This crate is the pure part — sweep state machine, fix hysteresis,
//! transition events — free of peripherals so the host can unit-test it,
//! next to `leviculum-screen`, `leviculum-sd-policy` and
//! `leviculum-gnss-time` which follow the same pattern. The firmware's
//! GNSS task is a thin driver: it feeds UART bytes, read errors and time
//! into [`PresenceMachine`] and acts on the [`Output`]s (reconfigure the
//! UART, publish a transition, fold a parsed sentence into `GnssFix`).

#![cfg_attr(not(test), no_std)]

use nmea0183::{ParseResult, Parser, Source, GGA, GSV, RMC};

/// Baud sweep order. 9600 first — the u-blox ZOE-M8Q factory default on
/// the WisMesh Pocket V2 and the most common NMEA default overall — then
/// the two rates preconfigured modules commonly ship with (38400: u-blox
/// M9/M10 default, 115200: frequent vendor preset). A baud that produces
/// a parsed sentence sticks while sentences keep arriving; a lock that
/// starves ([`LOCK_STARVE_RESWEEP_MS`]) re-enters the sweep.
pub const BAUD_SWEEP: [u32; 3] = [9600, 38_400, 115_200];

/// Per-baud detection window in milliseconds.
///
/// NMEA cadence is 1 Hz: a receiver emits one sentence burst per second.
/// The window must cover at least a couple of sentence periods so a
/// burst boundary straddling the UART reconfiguration cannot fake
/// silence: worst case the reconfig lands just after a burst (~1 s of
/// legitimate quiet), and the next burst must then arrive and parse.
/// 3 s = three full sentence periods — that worst case plus resync
/// margin for a parser that joins mid-sentence — while keeping the full
/// three-baud sweep under 10 s of boot time.
pub const DETECT_WINDOW_MS: u64 = 3_000;

/// Fix→NoFix demotion hold in milliseconds.
///
/// RMC validity can flap for a sentence or two at signal margin (urban
/// canyon, foliage, a hand over the antenna); demoting per sentence
/// would flap the published state and every consumer with it. 10 s =
/// ten RMC periods: long enough to ride out margin flaps and short
/// reacquisitions, short enough that an unplugged antenna demotes while
/// the operator is still looking at the display. Promotion is immediate
/// — a valid RMC is a positive, checksummed claim of a solution, and
/// the calendar seed (#166) wants it as soon as it exists.
pub const FIX_HOLD_MS: u64 = 10_000;

/// Locked-line sentence-starvation threshold in milliseconds: a locked
/// baud is only kept while checksum-clean sentences keep arriving.
///
/// A healthy receiver emits a burst every second even without a fix, so
/// sentence flow — not line activity — is the liveness signal: a module
/// that rebooted onto a different baud (UBX-CFG-RST, #324) produces
/// *garbage*, not silence, and an unplugged one produces silence; both
/// starve this timer and re-enter the sweep from [`BAUD_SWEEP[0]`].
/// 15 s is deliberately longer than [`FIX_HOLD_MS`] so a published Fix
/// always demotes through NoFix before a re-sweep can begin, and long
/// enough that a slow reboot (~1 s) or a dropped burst never triggers
/// a spurious re-sweep.
pub const LOCK_STARVE_RESWEEP_MS: u64 = 15_000;

/// Number of NMEA talkers tracked separately for GSV.
///
/// A receiver running several constellations concurrently emits one GSV
/// group per talker (`GPGSV`, `GLGSV`, …), each announcing its own
/// in-view count. They must be aggregated apart and summed: a single
/// shared slot would let GLONASS overwrite GPS every second and report
/// a fraction of the sky.
const TALKERS: usize = 5;

/// Slot index for a talker. `nmea0183`'s `Source` is matched
/// exhaustively on purpose — a new constellation must be given a slot,
/// not silently folded into another one.
fn talker_slot(source: Source) -> usize {
    match source {
        Source::GPS => 0,
        Source::GLONASS => 1,
        Source::Gallileo => 2,
        Source::Beidou => 3,
        Source::GNSS => 4,
    }
}

/// One talker's GSV group assembly plus its last completed result.
///
/// GSV is a *group*: `total_messages_number` sentences that together
/// describe the in-view set, each carrying up to four SVs. Only a
/// complete group is a measurement — a half-received group carries a
/// truthful in-view count but only part of the C/N0 set, so committing
/// it would under-report signal strength exactly when the numbers are
/// being used to judge the antenna.
#[derive(Clone, Copy, Default)]
struct TalkerGsv {
    /// Message number expected next in the group being assembled;
    /// 0 means no group is in progress.
    expect_msg: u8,
    /// `total_messages_number` announced by the group in progress.
    total_msgs: u8,
    /// In-view count announced by the group in progress.
    pending_sv: u8,
    /// Best C/N0 across the sentences of the group so far.
    pending_cno: Option<u8>,
    /// Last *complete* group's in-view count.
    sv_in_view: u8,
    /// Last *complete* group's best C/N0 in dBHz.
    cno_best: Option<u8>,
}

impl TalkerGsv {
    fn on_gsv(&mut self, gsv: &GSV) {
        let msg = gsv.message_number;
        if msg == 1 {
            // A group always restarts here, whatever was in progress:
            // an abandoned predecessor is exactly the truncated-tail
            // case and must be dropped, not merged.
            self.expect_msg = 1;
            self.total_msgs = gsv.total_messages_number;
            self.pending_sv = gsv.sat_in_view;
            self.pending_cno = None;
        } else if self.expect_msg == 0
            || msg != self.expect_msg
            || gsv.total_messages_number != self.total_msgs
        {
            // A lost or reordered predecessor: the group can no longer
            // be completed honestly. Drop it and wait for the next
            // `message_number == 1`.
            self.expect_msg = 0;
            return;
        }

        for sat in gsv.get_in_view_satellites() {
            // A null C/N0 means "in view but not tracked" — it carries
            // no signal-strength claim and must not read as 0 dBHz.
            if let Some(snr) = sat.snr {
                self.pending_cno = Some(match self.pending_cno {
                    Some(best) if best >= snr => best,
                    _ => snr,
                });
            }
        }

        if self.total_msgs > 0 && msg >= self.total_msgs {
            self.sv_in_view = self.pending_sv;
            self.cno_best = self.pending_cno;
            self.expect_msg = 0;
        } else {
            self.expect_msg = msg.saturating_add(1);
        }
    }
}

/// GSV aggregation across all talkers: the `sv=` / `cno=` heartbeat
/// fields of Codeberg #324.
///
/// This is pure instrumentation — it never influences presence, the fix
/// snapshot or the timebase. Its job is to make "config problem versus
/// antenna problem" decidable from the log while the receiver still
/// reports no fix: satellites in view says the receiver hears the sky at
/// all, best C/N0 says how well.
#[derive(Clone, Copy, Default)]
struct GsvView {
    talkers: [TalkerGsv; TALKERS],
}

impl GsvView {
    fn on_gsv(&mut self, gsv: &GSV) {
        self.talkers[talker_slot(gsv.source)].on_gsv(gsv);
    }

    fn sv_in_view(&self) -> u8 {
        self.talkers
            .iter()
            .fold(0u8, |acc, t| acc.saturating_add(t.sv_in_view))
    }

    fn cno_best(&self) -> Option<u8> {
        self.talkers.iter().filter_map(|t| t.cno_best).max()
    }
}

/// The runtime presence answer. See the crate doc for the semantics of
/// each state and the operator action it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    NoHardware,
    NoFix,
    Fix,
}

impl Presence {
    /// Stable event token for the `[GNSS_PRESENCE] state=<...>` debug
    /// line (periculum replays grep for these exact strings).
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::NoHardware => "no-hardware",
            Presence::NoFix => "no-fix",
            Presence::Fix => "fix",
        }
    }
}

/// One machine output, handed to the driver's `emit` callback in the
/// order it must be acted on.
#[derive(Debug)]
pub enum Output {
    /// The published presence changed. `baud` is the locked baud rate,
    /// or 0 for [`Presence::NoHardware`] (no baud was ever found).
    Transition { state: Presence, baud: u32 },
    /// Reconfigure the UART to this baud rate before the next read.
    SetBaud(u32),
    /// A parsed, checksum-clean RMC to fold into `GnssFix` (both valid
    /// and not-valid — the fold clears the stale time claim on the
    /// latter).
    Rmc(RMC),
    /// A parsed, checksum-clean GGA to fold into `GnssFix`.
    Gga(GGA),
}

/// Where the machine currently is between the sweep and a locked baud.
enum Phase {
    /// Probing `BAUD_SWEEP[idx]`. `activity` accumulates over the whole
    /// sweep pass (all bauds), not just the current window: it decides
    /// silence (→ NoHardware) versus unparseable noise (→ keep
    /// sweeping) when the pass ends.
    Sweep {
        idx: usize,
        window_start_ms: u64,
        activity: bool,
    },
    /// A full sweep pass saw zero line activity; parked at
    /// `BAUD_SWEEP[0]` listening. Published state: NoHardware. Any
    /// later activity restarts the sweep.
    Silent,
    /// A parsed sentence locked `BAUD_SWEEP[idx]`. The lock holds while
    /// sentences keep arriving; `last_sentence_ms` drives the
    /// starvation re-sweep ([`LOCK_STARVE_RESWEEP_MS`]), and
    /// `last_valid_rmc_ms` drives the Fix hold (`None` until the first
    /// valid RMC).
    Locked {
        idx: usize,
        last_valid_rmc_ms: Option<u64>,
        last_sentence_ms: u64,
    },
}

/// Sweep + hysteresis state machine. Feed it bytes, UART errors and
/// time; act on the outputs. All timestamps are milliseconds from one
/// monotonic clock.
pub struct PresenceMachine {
    parser: Parser,
    phase: Phase,
    published: Option<Presence>,
    sentences: u32,
    gsv: GsvView,
}

impl PresenceMachine {
    pub fn new(now_ms: u64) -> Self {
        Self {
            parser: Parser::new(),
            phase: Phase::Sweep {
                idx: 0,
                window_start_ms: now_ms,
                activity: false,
            },
            published: None,
            sentences: 0,
            gsv: GsvView::default(),
        }
    }

    /// Cumulative count of checksum-clean sentences (health-log fodder;
    /// the driver no longer sees parse results directly).
    pub fn sentences_seen(&self) -> u32 {
        self.sentences
    }

    /// Satellites in view summed over all talkers, from the last
    /// *complete* GSV group of each (#324 instrumentation). 0 until a
    /// group completes; a group whose tail was lost never contributes.
    ///
    /// This is in-view, not in-use: it counts what the receiver hears,
    /// so it is non-zero long before a fix — which is what makes it a
    /// discriminator while presence still reads `no-fix`.
    pub fn sv_in_view(&self) -> u8 {
        self.gsv.sv_in_view()
    }

    /// Best C/N0 in dBHz across the last complete GSV group of every
    /// talker; `None` until one completes with a tracked satellite.
    pub fn cno_best(&self) -> Option<u8> {
        self.gsv.cno_best()
    }

    /// The baud rate the driver should have the UART configured to.
    pub fn current_baud(&self) -> u32 {
        match self.phase {
            Phase::Sweep { idx, .. } | Phase::Locked { idx, .. } => BAUD_SWEEP[idx],
            Phase::Silent => BAUD_SWEEP[0],
        }
    }

    /// The last published presence, `None` while still detecting.
    pub fn published(&self) -> Option<Presence> {
        self.published
    }

    /// Whether a baud is currently locked (a checksum-clean sentence
    /// arrived at the configured rate and the lock has not starved).
    /// The one-shot UBX init (#324) gates every TX step on this: a UBX
    /// frame sent at an unlocked baud is garbage into the module.
    pub fn locked(&self) -> bool {
        matches!(self.phase, Phase::Locked { .. })
    }

    /// Feed a chunk of UART bytes.
    pub fn on_bytes(&mut self, bytes: &[u8], now_ms: u64, emit: &mut dyn FnMut(Output)) {
        if !bytes.is_empty() {
            self.note_activity(now_ms);
        }
        for &b in bytes {
            // An `Err` result is a checksum mismatch / malformed frame —
            // garbage at a wrong baud or line noise; it already counted
            // as activity and needs nothing else.
            if let Some(Ok(sentence)) = self.parser.parse_from_byte(b) {
                self.sentences = self.sentences.saturating_add(1);
                self.on_sentence(sentence, now_ms, emit);
            }
        }
        self.poll(now_ms, emit);
    }

    /// A UART read error (framing, overrun). An error proves an active
    /// line just as bytes do — a wrong-baud stream often surfaces as
    /// framing errors rather than data.
    pub fn on_uart_error(&mut self, now_ms: u64, emit: &mut dyn FnMut(Output)) {
        self.note_activity(now_ms);
        self.poll(now_ms, emit);
    }

    /// Drive time forward: window expiry during the sweep; the Fix hold
    /// and the sentence-starvation re-sweep once locked. The driver
    /// calls this on read timeouts; `on_bytes` calls it internally
    /// after every chunk.
    pub fn poll(&mut self, now_ms: u64, emit: &mut dyn FnMut(Output)) {
        match self.phase {
            Phase::Sweep {
                idx,
                window_start_ms,
                activity,
            } => {
                if now_ms.saturating_sub(window_start_ms) >= DETECT_WINDOW_MS {
                    self.end_window(idx, activity, now_ms, emit);
                }
            }
            Phase::Locked {
                idx,
                last_valid_rmc_ms,
                last_sentence_ms,
            } => {
                if self.published == Some(Presence::Fix) {
                    if let Some(t) = last_valid_rmc_ms {
                        if now_ms.saturating_sub(t) >= FIX_HOLD_MS {
                            self.set_state(Presence::NoFix, BAUD_SWEEP[idx], emit);
                        }
                    }
                }
                // Starvation strictly outlasts the Fix hold (constants
                // assert it), so a Fix has always demoted through NoFix
                // by the time the lock is abandoned.
                if now_ms.saturating_sub(last_sentence_ms) >= LOCK_STARVE_RESWEEP_MS {
                    self.reset_line_state();
                    self.phase = Phase::Sweep {
                        idx: 0,
                        window_start_ms: now_ms,
                        activity: false,
                    };
                    emit(Output::SetBaud(BAUD_SWEEP[0]));
                }
            }
            Phase::Silent => {}
        }
    }

    /// Drop everything derived from the stream we were reading: the
    /// half-parsed sentence in the parser and the GSV measurement. Both
    /// belong to a line we have just stopped believing — a stale
    /// `sv=`/`cno=` reading would report a healthy antenna long after
    /// the module went quiet or changed baud.
    fn reset_line_state(&mut self) {
        self.parser = Parser::new();
        self.gsv = GsvView::default();
    }

    /// Line activity: marks the sweep pass, and wakes the machine out of
    /// Silent parking back into a fresh sweep.
    fn note_activity(&mut self, now_ms: u64) {
        match &mut self.phase {
            Phase::Sweep { activity, .. } => *activity = true,
            Phase::Silent => {
                self.reset_line_state();
                self.phase = Phase::Sweep {
                    idx: 0,
                    window_start_ms: now_ms,
                    activity: true,
                };
            }
            Phase::Locked { .. } => {}
        }
    }

    /// A checksum-clean, recognised sentence. Locks the baud if still
    /// sweeping, then routes RMC/GGA content.
    fn on_sentence(&mut self, sentence: ParseResult, now_ms: u64, emit: &mut dyn FnMut(Output)) {
        if let Phase::Sweep { idx, .. } = self.phase {
            self.phase = Phase::Locked {
                idx,
                last_valid_rmc_ms: None,
                last_sentence_ms: now_ms,
            };
            self.set_state(Presence::NoFix, BAUD_SWEEP[idx], emit);
        }
        if let Phase::Locked {
            last_sentence_ms, ..
        } = &mut self.phase
        {
            *last_sentence_ms = now_ms;
        }
        match sentence {
            ParseResult::RMC(Some(rmc)) => {
                // Presence follows RMC validity and nothing else: GGA
                // quality feeds the GnssFix snapshot but never promotes
                // the state, so position/timebase gating stays keyed to
                // valid RMC only (#166 seed gate contract).
                if rmc.mode.is_valid() {
                    if let Phase::Locked {
                        idx,
                        last_valid_rmc_ms,
                        ..
                    } = &mut self.phase
                    {
                        *last_valid_rmc_ms = Some(now_ms);
                        let baud = BAUD_SWEEP[*idx];
                        self.set_state(Presence::Fix, baud, emit);
                    }
                }
                emit(Output::Rmc(rmc));
            }
            ParseResult::GGA(Some(gga)) => emit(Output::Gga(gga)),
            // GSV is instrumentation only (#324): it feeds the in-view
            // and C/N0 counters and emits nothing, so presence, the fix
            // snapshot and the timebase stay keyed to RMC/GGA exactly as
            // before.
            ParseResult::GSV(Some(gsv)) => self.gsv.on_gsv(&gsv),
            // Void RMC/GGA (cold start) and other sentence types carry
            // no foldable content; they already served as lock evidence.
            _ => {}
        }
    }

    /// A detection window ran out without a parsed sentence.
    fn end_window(
        &mut self,
        idx: usize,
        pass_activity: bool,
        now_ms: u64,
        emit: &mut dyn FnMut(Output),
    ) {
        // Garbage half-sentences must not leak parser state across a
        // baud change.
        self.reset_line_state();
        let next = idx + 1;
        if next < BAUD_SWEEP.len() {
            self.phase = Phase::Sweep {
                idx: next,
                window_start_ms: now_ms,
                activity: pass_activity,
            };
            emit(Output::SetBaud(BAUD_SWEEP[next]));
        } else if pass_activity {
            // Something is on the line but nothing parsed at any baud —
            // an unsupported rate or noise. Keep sweeping quietly: no
            // state change, no events beyond the reconfigures.
            self.phase = Phase::Sweep {
                idx: 0,
                window_start_ms: now_ms,
                activity: false,
            };
            emit(Output::SetBaud(BAUD_SWEEP[0]));
        } else {
            // A whole pass of silence: nothing is wired to the UART.
            // Park at the default baud and keep listening — a read is
            // event-driven, so this costs no busy loop, and any later
            // byte restarts the sweep via note_activity.
            self.phase = Phase::Silent;
            self.set_state(Presence::NoHardware, 0, emit);
            emit(Output::SetBaud(BAUD_SWEEP[0]));
        }
    }

    /// Publish a presence change; duplicate settles are suppressed so
    /// re-sweeps cannot spam the event channel.
    fn set_state(&mut self, state: Presence, baud: u32, emit: &mut dyn FnMut(Output)) {
        if self.published != Some(state) {
            self.published = Some(state);
            emit(Output::Transition { state, baud });
        }
    }
}

#[cfg(test)]
mod tests;
