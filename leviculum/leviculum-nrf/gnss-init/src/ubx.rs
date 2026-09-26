//! One-shot u-blox M8 UBX boot init: factory clear, full power,
//! antenna supply (Codeberg #324). Drives the WisMesh Pocket V2's
//! ZOE-M8Q; see [`crate::l76k`] for the T114's Quectel module.
//!
//! The WisMesh Pocket V2's ZOE-M8Q ran Meshtastic firmware before it ran
//! ours, and u-blox modules persist configuration in battery-backed RAM
//! across power cycles. A persisted power-save or constellation setting
//! explains a module that streams NMEA but never acquires — and robust
//! firmware must not depend on a field module's factory defaults either
//! way. This crate is the wake-up call, kept to the smallest honest set:
//!
//! 1. **UBX-CFG-CFG** (`clearMask`+`loadMask` all sections): revert the
//!    persisted configuration to factory defaults AND apply them to the
//!    running configuration. This — not CFG-RST — is the message that
//!    forgets saved config: CFG-RST's `navBbrMask` bits cover only the
//!    GNSS *data* sections (ephemeris, almanac, position, time; M8
//!    protocol spec §UBX-CFG-RST), so a config saved via CFG-CFG (which
//!    Meshtastic does on every boot, `meshtastic/src/gps/GPS.cpp:754-756`
//!    with `_message_SAVE`, `ubx.h:323-328`) would survive any cold
//!    start. The `loadMask` half is what makes the reset unnecessary:
//!    "This only replaces the Permanent Configuration, not the Current
//!    Configuration. To make the u-blox receiver operate with the
//!    Default Configuration which was restored to the Permanent
//!    Configuration, a UBX-CFG-CFG/load command **must be sent or** the
//!    u-blox receiver must be reset" (M8 protocol spec §3.1, emphasis
//!    ours; execution order within one message is clear, save, load).
//! 2. **UBX-CFG-PMS** (`powerSetupValue=0x00`, full power): belt and
//!    braces against any power-save mode surviving the clear. The M8
//!    factory default is full power, so this is a no-op on a healthy
//!    module; Meshtastic drives the same message with 0x03
//!    (aggressive 1 Hz, `ubx.h:315-321`, sent at `GPS.cpp:741`) — we
//!    send the inverse.
//! 3. **UBX-CFG-ANT** (`flags=svcs` only, `pins` untouched): drive the
//!    antenna supply control signal, with every automatic power-down
//!    path off. The wake depends on the antenna being powered, so it
//!    configures that explicitly instead of trusting whatever the clear
//!    left behind. The M8 SPG default (spec appendix C.1) is
//!    `svcs=1 scd=1 pdwnOnSCD=1 recovery=1 ocd=0` — supply on, but
//!    with short-circuit detection armed to CUT the supply, on default
//!    detection pins whose board-level wiring we cannot verify. A false
//!    short on an unverifiable pin powering down the antenna is exactly
//!    the deaf-module failure #324 chases, so each bit is deliberate:
//!    `svcs=1` (the one bit the wake needs), `scd=0`/`ocd=0` (status
//!    detection we never read — the driver polls no UBX-MON-HW — on
//!    wiring we cannot verify), `pdwnOnSCD=0` (no automatic supply
//!    cut), `recovery=0` (meaningless without a short state). `pins`
//!    is all-zero with `reconfig=0`: the pin fields only apply when
//!    `reconfig` is set, so the module keeps its current routing — the
//!    spec recommends the default pins and we have no RAK wiring data
//!    that would justify rerouting. Meshtastic has no CFG-ANT
//!    precedent to compare against (verified: no `0x06, 0x13` send in
//!    `meshtastic/src/gps/`); the M8 spec (UBX-13003221 R28,
//!    §UBX-CFG-ANT and appendix C.1) is the sole source here.
//!
//! Nothing else — and in particular **no reset**. The wake used to fire
//! a UBX-CFG-RST cold start (`navBbrMask=0xFFFF`) between steps 1 and 2,
//! deliberately, as a diagnostic: it proved the module could acquire
//! from nothing. It also wiped ephemeris, almanac and last position on
//! *every* boot, so every boot paid a fresh sky download — tens of
//! minutes at a half-sky window where a warm or hot start needs seconds.
//! A tracker reboots far more often than it needs that proof, so the
//! cold start left the boot path (#324); [`COLD_START_FRAME`] stays in
//! the crate as a diagnostic frame a future control-envelope command
//! could send on demand (noted for #235).
//!
//! Nothing in the remaining three messages needs a reset to take effect.
//! The M8 spec is explicit for the general case — "The u-blox receiver
//! will change its Current Configuration immediately after receiving the
//! configuration message" (§3.1) — and step 1's `loadMask` covers the
//! one documented exception, the clear-only case quoted above. So not
//! even the mildest documented restart (a hot start,
//! `navBbrMask=0x0000`) is warranted; a restart we cannot justify from
//! the spec is a restart that costs a fix for nothing.
//!
//! Frame bytes are computed, never hardcoded: `ubx_frame` is a
//! `const fn` implementing the UBX framing (`B5 62`, class/id,
//! little-endian length, 8-bit Fletcher checksum; M8 protocol spec §UBX
//! frame structure), evaluated at compile time; the tests assert the
//! resulting bytes against an independently computed fixture.
//!
//! ACK discipline follows the Meshtastic reference (it waits for ACKs,
//! `SEND_UBX_PACKET`/`getACK`, `ubx.h:3-10`): every step is ACK-checked,
//! and the outcome — ok, nak or timeout — is reported for the log, but
//! never retried and never blocking (the reference warns and continues
//! too). A timeout is expected often enough on the first step to matter:
//! clearing the `ioPort` sub-section "results in an IO system reset.
//! Because of this undefined data may be output for a short period of
//! time after receiving the message" (M8 protocol spec §UBX-CFG-CFG,
//! `clearMask`), which can swallow the acknowledgement.
//!
//! Like `leviculum-gnss-presence` this is the pure, host-tested part.
//! The firmware's GNSS task feeds it lock state, the clean-sentence
//! counter, RX bytes and time, and acts on the [`Output`]s (write a
//! frame to the UART TX, log a step). Every TX step is gated on a
//! currently locked baud — a UBX frame sent at the wrong baud is garbage
//! into the module — and the steps after a disruptive one additionally
//! require a *fresh* clean sentence after a settle period, so a frame is
//! never fired into a module that is still reconfiguring or whose I/O
//! system has just been reset under it.

use crate::{AckOutcome, ModuleInit, Output};

/// Build a UBX frame at compile time: sync (`B5 62`), class, id,
/// little-endian length, payload, 8-bit Fletcher checksum over
/// class..payload (M8 protocol spec §UBX frame structure).
///
/// `N` must be `payload.len() + 8`; the assert makes a mismatch a
/// compile error because the function is only evaluated in const
/// context (the `FRAME` consts below).
const fn ubx_frame<const N: usize>(class: u8, id: u8, payload: &[u8]) -> [u8; N] {
    assert!(N == payload.len() + 8, "N must be payload length + 8");
    let mut f = [0u8; N];
    f[0] = 0xB5;
    f[1] = 0x62;
    f[2] = class;
    f[3] = id;
    f[4] = payload.len() as u8;
    f[5] = (payload.len() >> 8) as u8;
    let mut i = 0;
    while i < payload.len() {
        f[6 + i] = payload[i];
        i += 1;
    }
    let (a, b) = fletcher8(&f, 2, N - 2);
    f[N - 2] = a;
    f[N - 1] = b;
    f
}

/// 8-bit Fletcher checksum over `bytes[from..to]`, as the UBX protocol
/// defines it (CK_A/CK_B running sums, modulo 256).
const fn fletcher8(bytes: &[u8], from: usize, to: usize) -> (u8, u8) {
    let mut a: u8 = 0;
    let mut b: u8 = 0;
    let mut i = from;
    while i < to {
        a = a.wrapping_add(bytes[i]);
        b = b.wrapping_add(a);
        i += 1;
    }
    (a, b)
}

/// UBX-CFG-CFG payload: `clearMask=0x0000FFFF` (all configuration
/// sections, defined bits live in the low half), `saveMask=0`,
/// `loadMask=0x0000FFFF` (load the now-cleared sections, i.e. apply the
/// defaults to the running config), `deviceMask=0x17` (BBR, flash,
/// EEPROM, SPI flash — every device the reference saves to,
/// `meshtastic/src/gps/ubx.h:323-328`).
const FACTORY_CLEAR_PAYLOAD: [u8; 13] = [
    0xFF, 0xFF, 0x00, 0x00, // clearMask
    0x00, 0x00, 0x00, 0x00, // saveMask
    0xFF, 0xFF, 0x00, 0x00, // loadMask
    0x17, // deviceMask
];

/// UBX-CFG-RST payload: `navBbrMask=0xFFFF` (the spec's cold-start
/// special set — clear every GNSS data section), `resetMode=0x01`
/// (controlled software reset: full receiver restart including the
/// I/O subsystem, so the module reboots cleanly onto its default
/// port configuration).
///
/// Diagnostic only — see [`COLD_START_FRAME`].
const COLD_START_PAYLOAD: [u8; 4] = [0xFF, 0xFF, 0x01, 0x00];

/// UBX-CFG-PMS payload: `version=0`, `powerSetupValue=0x00` (full
/// power), `period`/`onTime` zero (only valid for Interval mode).
const FULL_POWER_PAYLOAD: [u8; 8] = [0x00; 8];

/// UBX-CFG-ANT payload: `flags=0x0001` (`svcs` alone — supply control
/// on, every detection/power-down bit off; the module docs above walk
/// through each bit against the M8 SPG default), `pins=0x0000` with
/// `reconfig=0` (bit 15 clear: pin fields are not applied, current
/// routing kept).
const ANTENNA_SUPPLY_PAYLOAD: [u8; 4] = [0x01, 0x00, 0x00, 0x00];

/// UBX-CFG-CFG frame reverting the persisted configuration to defaults.
pub const FACTORY_CLEAR_FRAME: [u8; 21] = ubx_frame(0x06, 0x09, &FACTORY_CLEAR_PAYLOAD);

/// UBX-CFG-RST cold-start frame — **diagnostic, not on the boot path**.
///
/// Sending this wipes ephemeris, almanac and last position and reboots
/// the module, so the next fix is a true cold start: minutes of sky
/// download instead of the seconds a warm or hot start takes. That is
/// the point of it — it proves a module can acquire from nothing, which
/// is exactly what #324 needed to establish once. It is exactly why it
/// must not run on every boot of a tracker that reboots often.
///
/// The frame stays here, tested, so the diagnostic is a wire-correct
/// tool one command away rather than something to re-derive under
/// pressure. Nothing in this crate sends it; [`UbxInit`] never emits it
/// (asserted in the tests). A caller that does send it owns the
/// aftermath: the module reboots onto its default port configuration,
/// so the line must be re-locked (the presence machine's starve
/// re-sweep) before anything else is written. Wiring it to an on-demand
/// control-envelope command is a possibility for #235.
pub const COLD_START_FRAME: [u8; 12] = ubx_frame(0x06, 0x04, &COLD_START_PAYLOAD);

/// UBX-CFG-PMS full-power frame.
pub const FULL_POWER_FRAME: [u8; 16] = ubx_frame(0x06, 0x86, &FULL_POWER_PAYLOAD);

/// UBX-CFG-ANT antenna-supply frame.
pub const ANTENNA_SUPPLY_FRAME: [u8; 12] = ubx_frame(0x06, 0x13, &ANTENNA_SUPPLY_PAYLOAD);

// The driver stages every frame through one `[u8; MAX_FRAME]` buffer, so
// a frame that outgrew it must fail the build, not the copy.
const _: () = assert!(crate::MAX_FRAME >= FACTORY_CLEAR_FRAME.len());
const _: () = assert!(crate::MAX_FRAME >= COLD_START_FRAME.len());
const _: () = assert!(crate::MAX_FRAME >= FULL_POWER_FRAME.len());
const _: () = assert!(crate::MAX_FRAME >= ANTENNA_SUPPLY_FRAME.len());

/// ACK wait for UBX-CFG-CFG, in ms. The reference waits 2000 ms for
/// exactly this message (`meshtastic/src/gps/GPS.cpp:756`).
pub const CFG_ACK_TIMEOUT_MS: u64 = 2_000;

/// ACK wait for UBX-CFG-PMS, in ms. The reference uses 500 ms
/// (`GPS.cpp:741`); tripled here because our driver reads in
/// burst-aligned ~1 s chunks, so a sub-second deadline would routinely
/// mis-log an ACK that is already on the wire as a timeout.
pub const PMS_ACK_TIMEOUT_MS: u64 = 1_500;

/// ACK wait for UBX-CFG-ANT, in ms. No reference cadence exists
/// (Meshtastic never sends CFG-ANT); same burst-aligned reasoning as
/// [`PMS_ACK_TIMEOUT_MS`].
pub const ANT_ACK_TIMEOUT_MS: u64 = 1_500;

/// Settle after CFG-CFG before CFG-PMS, in ms. The reference holds 1 s
/// after a config message that restarts the GNSS subsystem
/// (`GPS.cpp:711-713`). On top of the settle the sequencer demands a
/// fresh clean sentence, proving the module is alive at our baud — which
/// is the gate that really matters here, because clearing the `ioPort`
/// sub-section resets the I/O system and the load may put the port back
/// on its default baud (M8 protocol spec §UBX-CFG-CFG, `clearMask`).
/// Recovering from that is the presence machine's starve re-sweep, not
/// something this settle tries to model.
pub const POST_CFG_SETTLE_MS: u64 = 1_000;

/// Settle after CFG-PMS before CFG-ANT, in ms. CFG-PMS switches the
/// power regime without a reboot, so the CFG-side hold suffices (the
/// reference's 1 s after a reconfiguring message, `GPS.cpp:711-713`);
/// the fresh-sentence-under-lock gate carries the real weight here as
/// everywhere else.
pub const POST_PMS_SETTLE_MS: u64 = 1_000;

/// One init step. `as_str` is the stable `[GNSS_INIT] step=<...>` log
/// token. There is no variant for the cold start: [`Step`] is the boot
/// path, and the cold start is not on it ([`COLD_START_FRAME`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// UBX-CFG-CFG: revert persisted configuration to defaults and load
    /// them into the running configuration.
    FactoryClear,
    /// UBX-CFG-PMS: force full-power operation.
    FullPower,
    /// UBX-CFG-ANT: drive the antenna supply, auto-power-down off.
    AntennaSupply,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::FactoryClear => "cfg",
            Step::FullPower => "pms",
            Step::AntennaSupply => "ant",
        }
    }

    /// The frame this step transmits. `&'static` — the bytes live in
    /// flash; a driver with DMA constraints (nRF EasyDMA reads RAM
    /// only) must stage them into a RAM buffer of [`MAX_FRAME`] bytes.
    pub fn frame(self) -> &'static [u8] {
        match self {
            Step::FactoryClear => &FACTORY_CLEAR_FRAME,
            Step::FullPower => &FULL_POWER_FRAME,
            Step::AntennaSupply => &ANTENNA_SUPPLY_FRAME,
        }
    }

    /// UBX class/id, for matching the ACK payload.
    fn class_id(self) -> (u8, u8) {
        match self {
            Step::FactoryClear => (0x06, 0x09),
            Step::FullPower => (0x06, 0x86),
            Step::AntennaSupply => (0x06, 0x13),
        }
    }
}

/// Scanner for UBX-ACK-ACK / UBX-ACK-NAK frames in the RX byte stream
/// (`B5 62 05 <01|00> 02 00 <cls> <id> CK_A CK_B`). NMEA text between
/// frames is skipped byte-wise (NMEA is pure ASCII, so the 0xB5 sync
/// byte cannot occur inside a sentence); a checksum mismatch drops the
/// candidate frame.
#[derive(Default)]
struct AckScanner {
    buf: [u8; 10],
    pos: usize,
}

impl AckScanner {
    /// Feed one byte; `Some((class, id, is_ack))` on a complete,
    /// checksum-valid ACK/NAK frame.
    fn push(&mut self, byte: u8) -> Option<(u8, u8, bool)> {
        let ok = match self.pos {
            0 => byte == 0xB5,
            1 => byte == 0x62,
            2 => byte == 0x05,
            3 => byte == 0x01 || byte == 0x00,
            4 => byte == 0x02,
            5 => byte == 0x00,
            _ => true,
        };
        if !ok {
            // Resync: the mismatching byte may itself start a frame.
            if byte == 0xB5 {
                self.buf[0] = 0xB5;
                self.pos = 1;
            } else {
                self.pos = 0;
            }
            return None;
        }
        self.buf[self.pos] = byte;
        self.pos += 1;
        if self.pos < self.buf.len() {
            return None;
        }
        self.pos = 0;
        let (a, b) = fletcher8(&self.buf, 2, 8);
        if a != self.buf[8] || b != self.buf[9] {
            return None;
        }
        Some((self.buf[6], self.buf[7], self.buf[3] == 0x01))
    }
}

/// Sequencer state. `Gate` holds the send of `step` until the settle
/// deadline has passed AND a clean sentence has arrived after it
/// (`snap` is the sentence count taken at the first poll past
/// `earliest_ms`; a later, larger count is the fresh-sentence proof).
#[derive(Clone, Copy)]
enum State {
    AwaitLock,
    AwaitAck {
        step: Step,
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
/// via [`on_bytes`](Self::on_bytes) and drive time, lock state and the
/// clean-sentence counter via [`poll`](Self::poll). After the final
/// step it stays silent forever.
pub struct UbxInit {
    state: State,
    scanner: AckScanner,
}

impl Default for UbxInit {
    fn default() -> Self {
        Self::new()
    }
}

impl UbxInit {
    pub fn new() -> Self {
        Self {
            state: State::AwaitLock,
            scanner: AckScanner::default(),
        }
    }
}

impl ModuleInit for UbxInit {
    fn on_bytes(&mut self, bytes: &[u8], now_ms: u64, emit: &mut dyn FnMut(Output)) {
        for &b in bytes {
            let Some((class, id, is_ack)) = self.scanner.push(b) else {
                continue;
            };
            let State::AwaitAck { step, .. } = self.state else {
                continue;
            };
            if (class, id) != step.class_id() {
                continue;
            }
            let outcome = if is_ack {
                AckOutcome::Ack
            } else {
                AckOutcome::Nak
            };
            emit(Output::AckResult {
                step: step.as_str(),
                outcome,
            });
            self.advance_past_ack(step, now_ms);
        }
    }

    fn poll(&mut self, locked: bool, sentences: u32, now_ms: u64, emit: &mut dyn FnMut(Output)) {
        match self.state {
            State::AwaitLock => {
                if locked {
                    self.send(Step::FactoryClear, now_ms, emit);
                }
            }
            State::AwaitAck { step, deadline_ms } => {
                if now_ms >= deadline_ms {
                    emit(Output::AckResult {
                        step: step.as_str(),
                        outcome: AckOutcome::Timeout,
                    });
                    self.advance_past_ack(step, now_ms);
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

impl UbxInit {
    /// Emit the Send for `step` and move to its follow-up state.
    fn send(&mut self, step: Step, now_ms: u64, emit: &mut dyn FnMut(Output)) {
        emit(Output::Send {
            step: step.as_str(),
            frame: step.frame(),
        });
        self.state = match step {
            Step::FactoryClear => State::AwaitAck {
                step,
                deadline_ms: now_ms + CFG_ACK_TIMEOUT_MS,
            },
            Step::FullPower => State::AwaitAck {
                step,
                deadline_ms: now_ms + PMS_ACK_TIMEOUT_MS,
            },
            Step::AntennaSupply => State::AwaitAck {
                step,
                deadline_ms: now_ms + ANT_ACK_TIMEOUT_MS,
            },
        };
    }

    /// An ACK wait concluded (ack, nak or timeout) — same continuation
    /// regardless of outcome, mirroring the reference.
    fn advance_past_ack(&mut self, step: Step, now_ms: u64) {
        self.state = match step {
            Step::FactoryClear => State::Gate {
                step: Step::FullPower,
                earliest_ms: now_ms + POST_CFG_SETTLE_MS,
                snap: None,
            },
            Step::FullPower => State::Gate {
                step: Step::AntennaSupply,
                earliest_ms: now_ms + POST_PMS_SETTLE_MS,
                snap: None,
            },
            Step::AntennaSupply => State::Done,
        };
    }
}

#[cfg(test)]
mod tests;
