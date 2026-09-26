//! One-shot Quectel L76K boot init: probe, constellations, sentence
//! selection, navigation mode (Codeberg #69).
//!
//! The Heltec Mesh Node T114 carries a Quectel L76K on P1.05/P1.07. It
//! is not a u-blox part and does not speak UBX: the L76K is built around
//! a CASIC-family baseband (AT6558 lineage) and is configured with
//! proprietary `$PCAS` NMEA sentences. So the [`crate::ubx`] sequence
//! next door is not merely wrong bytes here, it is the wrong protocol —
//! but the *shape* is identical, and that shape is [`ModuleInit`].
//!
//! The reference is Meshtastic's `GNSS_MODEL_MTK` branch
//! (`meshtastic/src/gps/GPS.cpp:536-550`, reached for `GPS_L76K` boards,
//! of which `variants/nrf52840/heltec_mesh_node_t114/variant.h` is one).
//! It sends exactly three sentences, 250 ms apart. We send those three,
//! with one deliberate difference and one addition:
//!
//! 1. **`$PCAS06,0`** (added, first): the version query. Its answer,
//!    `$GPTXT,01,01,02,SW=…`, is the *only* positive evidence that the
//!    module is hearing our TX line at all — everything else in this
//!    sequence is fire-and-forget, because `$PCAS` commands are not
//!    acknowledged. Meshtastic uses this very exchange to detect an L76K
//!    (`GPS.cpp:1363`, `PROBE_SIMPLE("L76K", "$PCAS06,0*1B",
//!    "$GPTXT,01,01,02,SW=", …)`); we reuse it as a one-line answer to
//!    "is the wiring right", which is exactly the question a bring-up
//!    leaves open. A timeout is reported and the sequence continues:
//!    a module that does not answer a query may still obey a command,
//!    and refusing to configure it would be a worse failure than a noisy
//!    log line.
//! 2. **`$PCAS04,7`**: enable GPS + GLONASS + BeiDou. Verbatim from the
//!    reference. More constellations is more satellites in view, which
//!    is what a cold urban fix is short of.
//! 3. **`$PCAS03,…`**: sentence selection. The reference asks for GGA
//!    and RMC only; we additionally keep **GSV** on. That is the
//!    deviation, and it is deliberate: `leviculum-gnss-presence`
//!    aggregates GSV into the `sv=` / `cno=` heartbeat fields, which are
//!    the instrument that separates "the module is misconfigured" from
//!    "the antenna is deaf" while presence still reads `no-fix` (#324).
//!    Silencing GSV would blind exactly the diagnosis this bring-up will
//!    need. Everything else the reference silences stays silenced (GLL,
//!    GSA, VTG, ZDA, ANT, DHV, LPS, UTC, GST), so the airtime saving is
//!    kept where it costs nothing.
//! 4. **`$PCAS11,3`**: vehicle navigation mode. Verbatim from the
//!    reference, whose comment records why it is not the SoftRF
//!    aviation setting.
//!
//! **What is deliberately NOT sent: a baud-rate command.** `$PCAS01`
//! would change the module's UART rate, and the driver's baud is not a
//! constant it could be kept in step with — it is whatever the presence
//! machine's sweep locked. Setting a baud here would desynchronise the
//! line we are talking on, for no gain: the sweep already finds 9600
//! (the L76K default, `meshtastic/src/configuration.h:354`, not
//! overridden by the T114 variant), 38400 and 115200.
//!
//! **Standby.** The L76K has a hardware standby pin (T114: P1.02, high
//! = force wake, `variant.h:170` with `GPS_STANDBY_ACTIVE LOW` from
//! `GPS.h:22-23`). Waking it is a GPIO act, not a sentence, so it
//! belongs to the driver and not to this crate — but it is the same
//! lesson #324 taught on the Pocket V2 (a module parked in a low-power
//! state by whatever firmware ran before ours), and the driver holds
//! that pin high for its whole life.

use crate::{AckOutcome, ModuleInit, Output};

/// Build an NMEA sentence at compile time: `$`, body, `*`, the two
/// uppercase hex digits of the XOR checksum over the body, CRLF.
///
/// `N` must be `body.len() + 6`; the assert makes a mismatch a compile
/// error because the function is only evaluated in const context (the
/// `FRAME` consts below).
const fn nmea_frame<const N: usize>(body: &[u8]) -> [u8; N] {
    assert!(N == body.len() + 6, "N must be body length + 6");
    let mut f = [0u8; N];
    f[0] = b'$';
    let mut ck: u8 = 0;
    let mut i = 0;
    while i < body.len() {
        f[1 + i] = body[i];
        ck ^= body[i];
        i += 1;
    }
    let end = 1 + body.len();
    f[end] = b'*';
    f[end + 1] = hex_upper(ck >> 4);
    f[end + 2] = hex_upper(ck & 0x0F);
    f[end + 3] = b'\r';
    f[end + 4] = b'\n';
    f
}

/// One nibble as an uppercase ASCII hex digit (NMEA checksums are
/// uppercase; the reference's literals are).
const fn hex_upper(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'A' + (nibble - 10)
    }
}

/// `$PCAS06,0` — version query. The answer identifies the module and,
/// more usefully here, proves it reads our TX line.
pub const PROBE_FRAME: [u8; 14] = nmea_frame(b"PCAS06,0");

/// `$PCAS04,7` — constellation mask: bit 0 GPS, bit 1 BeiDou, bit 2
/// GLONASS, so 7 is all three (reference: `GPS.cpp:543`).
pub const CONSTELLATIONS_FRAME: [u8; 14] = nmea_frame(b"PCAS04,7");

/// `$PCAS03,…` — per-sentence output rates in fixed field order
/// (nGGA, nGLL, nGSA, nGSV, nRMC, nVTG, nZDA, nANT, nDHV, nLPS, two
/// reserved, nUTC, nGST). GGA, GSV and RMC every fix, everything else
/// off. The reference's line is the same with GSV off; the module docs
/// above carry the reason for the difference.
pub const SENTENCES_FRAME: [u8; 38] = nmea_frame(b"PCAS03,1,0,0,1,1,0,0,0,0,0,,,0,0");

/// `$PCAS11,3` — navigation mode 3, vehicle (reference: `GPS.cpp:549`).
pub const NAV_MODE_FRAME: [u8; 14] = nmea_frame(b"PCAS11,3");

// Same staging-buffer bound the UBX module asserts against.
const _: () = assert!(crate::MAX_FRAME >= PROBE_FRAME.len());
const _: () = assert!(crate::MAX_FRAME >= CONSTELLATIONS_FRAME.len());
const _: () = assert!(crate::MAX_FRAME >= SENTENCES_FRAME.len());
const _: () = assert!(crate::MAX_FRAME >= NAV_MODE_FRAME.len());

/// The prefix of the `$PCAS06,0` answer, matched literally in the RX
/// stream. Same token the reference matches (`GPS.cpp:1363`); the
/// software version string follows it and is not parsed.
pub const PROBE_ANSWER: &[u8] = b"$GPTXT,01,01,02,SW=";

/// Wait for the probe answer, in ms. The reference allows 500 ms
/// (`GPS.cpp:1363`); tripled here for the same reason the UBX ACK waits
/// are — the driver reads in burst-aligned ~1 s chunks, so a sub-second
/// deadline would routinely mis-log an answer already on the wire as a
/// timeout.
pub const PROBE_TIMEOUT_MS: u64 = 1_500;

/// Settle between two configuration sentences, in ms. The reference
/// holds 250 ms (`GPS.cpp:544/547/550`). On top of it the sequencer
/// demands a *fresh* clean sentence under a held lock, which is the gate
/// that carries the real weight: it proves the module is still emitting
/// at the baud we are writing at.
pub const POST_STEP_SETTLE_MS: u64 = 250;

/// One init step. `as_str` is the stable `[GNSS_INIT] step=<...>` log
/// token, disjoint from the UBX module's tokens on purpose: one look at
/// a capture says which module language the board spoke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `$PCAS06,0`: version query, the one step with an answer.
    Probe,
    /// `$PCAS04,7`: GPS + GLONASS + BeiDou.
    Constellations,
    /// `$PCAS03,…`: GGA + GSV + RMC, everything else off.
    Sentences,
    /// `$PCAS11,3`: vehicle navigation mode.
    NavMode,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Probe => "probe",
            Step::Constellations => "constellations",
            Step::Sentences => "sentences",
            Step::NavMode => "nav-mode",
        }
    }

    /// The frame this step transmits. `&'static` — the bytes live in
    /// flash; a driver with DMA constraints (nRF EasyDMA reads RAM only)
    /// must stage them into a RAM buffer of [`crate::MAX_FRAME`] bytes.
    pub fn frame(self) -> &'static [u8] {
        match self {
            Step::Probe => &PROBE_FRAME,
            Step::Constellations => &CONSTELLATIONS_FRAME,
            Step::Sentences => &SENTENCES_FRAME,
            Step::NavMode => &NAV_MODE_FRAME,
        }
    }
}

/// Rolling literal matcher for [`PROBE_ANSWER`] in the RX byte stream.
///
/// The answer arrives inside a normal NMEA burst, so it has to be found
/// between other sentences and possibly split across read chunks. A
/// byte that breaks the match may itself be the start of a new one
/// (`$` is the only such byte here, since the pattern contains no other
/// `$`), so the reset is to "matched one byte" in that case.
#[derive(Default)]
struct AnswerScanner {
    matched: usize,
}

impl AnswerScanner {
    /// Feed one byte; `true` on the byte that completes the pattern.
    fn push(&mut self, byte: u8) -> bool {
        if byte == PROBE_ANSWER[self.matched] {
            self.matched += 1;
            if self.matched == PROBE_ANSWER.len() {
                self.matched = 0;
                return true;
            }
            return false;
        }
        // Mismatch: restart, but do not swallow a byte that begins the
        // pattern itself.
        self.matched = usize::from(byte == PROBE_ANSWER[0]);
        false
    }
}

/// Sequencer state. `Gate` holds the send of `step` until the settle
/// deadline has passed AND a clean sentence has arrived after it
/// (`snap` is the sentence count taken at the first poll past
/// `earliest_ms`; a later, larger count is the fresh-sentence proof).
#[derive(Clone, Copy)]
enum State {
    AwaitLock,
    AwaitAnswer {
        deadline_ms: u64,
    },
    Gate {
        step: Step,
        earliest_ms: u64,
        snap: Option<u32>,
    },
    Done,
}

/// The one-shot init sequencer. Construct once per boot; feed RX bytes
/// via [`ModuleInit::on_bytes`] and drive time, lock state and the
/// clean-sentence counter via [`ModuleInit::poll`]. After the final step
/// it stays silent forever.
pub struct L76kInit {
    state: State,
    scanner: AnswerScanner,
}

impl Default for L76kInit {
    fn default() -> Self {
        Self::new()
    }
}

impl L76kInit {
    pub fn new() -> Self {
        Self {
            state: State::AwaitLock,
            scanner: AnswerScanner::default(),
        }
    }

    /// Emit the Send for `step` and move to its follow-up state. Only
    /// the probe has an answer to wait for; a `$PCAS` command is not
    /// acknowledged, so the step after it starts its settle immediately.
    fn send(&mut self, step: Step, now_ms: u64, emit: &mut dyn FnMut(Output)) {
        emit(Output::Send {
            step: step.as_str(),
            frame: step.frame(),
        });
        self.state = match step {
            Step::Probe => State::AwaitAnswer {
                deadline_ms: now_ms + PROBE_TIMEOUT_MS,
            },
            Step::Constellations => self.gate(Step::Sentences, now_ms),
            Step::Sentences => self.gate(Step::NavMode, now_ms),
            Step::NavMode => State::Done,
        };
    }

    /// The settle-plus-fresh-sentence gate in front of `step`.
    fn gate(&self, step: Step, now_ms: u64) -> State {
        State::Gate {
            step,
            earliest_ms: now_ms + POST_STEP_SETTLE_MS,
            snap: None,
        }
    }
}

impl ModuleInit for L76kInit {
    fn on_bytes(&mut self, bytes: &[u8], now_ms: u64, emit: &mut dyn FnMut(Output)) {
        for &b in bytes {
            if !self.scanner.push(b) {
                continue;
            }
            if !matches!(self.state, State::AwaitAnswer { .. }) {
                continue;
            }
            emit(Output::AckResult {
                step: Step::Probe.as_str(),
                outcome: AckOutcome::Ack,
            });
            self.state = self.gate(Step::Constellations, now_ms);
        }
    }

    fn poll(&mut self, locked: bool, sentences: u32, now_ms: u64, emit: &mut dyn FnMut(Output)) {
        match self.state {
            State::AwaitLock => {
                if locked {
                    self.send(Step::Probe, now_ms, emit);
                }
            }
            State::AwaitAnswer { deadline_ms } => {
                if now_ms >= deadline_ms {
                    // No answer is not a refusal: an unacknowledged
                    // command may still land. Report and continue, the
                    // reference's discipline everywhere else too.
                    emit(Output::AckResult {
                        step: Step::Probe.as_str(),
                        outcome: AckOutcome::Timeout,
                    });
                    self.state = self.gate(Step::Constellations, now_ms);
                }
            }
            State::Gate {
                step,
                earliest_ms,
                snap,
            } => {
                if now_ms < earliest_ms {
                    return;
                }
                match snap {
                    None => {
                        self.state = State::Gate {
                            step,
                            earliest_ms,
                            snap: Some(sentences),
                        }
                    }
                    Some(s) if sentences > s && locked => {
                        self.send(step, now_ms, emit);
                    }
                    Some(_) => {}
                }
            }
            State::Done => {}
        }
    }
}

#[cfg(test)]
mod tests;
