//! SX126x interrupt policy: which IRQs the chip is allowed to LATCH, which
//! of the latched ones drive a DIO line, and when a bounded RX window has to
//! be extended because a frame is still on the air.
//!
//! # Why this lives in core and not in the driver
//!
//! The driver is `leviculum-nrf::sx1262`, which cross-compiles to
//! `thumbv7em-none-eabihf` and has no host test target: nothing in it can be
//! observed failing without a board on the bench. The RX-extend guard was
//! written there, shipped, and was dead for its whole life — its precondition
//! could never be true, because the same function configured the chip to not
//! latch the two bits the precondition tests (Codeberg #144). A guard nobody
//! can run a test against is exactly the defect this module exists to make
//! impossible: the decision is a pure function here, the driver holds only
//! SPI.
//!
//! # The latch mask is not a routing mask
//!
//! `SetDioIrqParams` (opcode 0x08) takes four 16-bit masks. The FIRST one is
//! not a DIO routing mask — it gates whether an interrupt is recorded in the
//! IRQ status register at all (datasheet §13.3.1: an IRQ absent from
//! `IrqMask` is never raised, so `GetIrqStatus` never shows it). The three
//! that follow route the already-latched interrupts to DIO1, DIO2 and DIO3.
//!
//! This is why the reference RNode firmware sets the first mask to `0xFFFF`
//! and only the DIO mask narrowly (`sx126x.cpp:632-635`): its carrier-detect
//! path reads PREAMBLE_DET and HEADER_DET back out of the status register
//! (`sx126x.cpp:502-505`) without ever wanting them to wake a pin. Anything
//! that reads a bit back must enable that bit here first.

/// TxDone.
pub const IRQ_TX_DONE: u16 = 0x0001;
/// RxDone.
pub const IRQ_RX_DONE: u16 = 0x0002;
/// PreambleDetected: a LoRa preamble was seen. Read back, never routed.
pub const IRQ_PREAMBLE_DETECTED: u16 = 0x0004;
/// HeaderValid: an explicit header passed its CRC. Read back, never routed.
pub const IRQ_HEADER_VALID: u16 = 0x0010;
/// CrcErr: the payload CRC failed.
pub const IRQ_CRC_ERR: u16 = 0x0040;
/// CadDone.
pub const IRQ_CAD_DONE: u16 = 0x0080;
/// CadDetected.
pub const IRQ_CAD_DETECTED: u16 = 0x0100;
/// Timeout: the RX or TX hardware timer expired.
pub const IRQ_TIMEOUT: u16 = 0x0200;

/// Every interrupt latches. This is the only correct value for the first
/// `SetDioIrqParams` argument in any operation whose code reads a status bit
/// it does not also route to a DIO line, and it costs nothing otherwise: an
/// interrupt that latches but is routed nowhere cannot wake the MCU.
pub const IRQ_LATCH_ALL: u16 = 0xFFFF;

/// Routed to DIO1 during a transmission.
pub const DIO1_TX: u16 = IRQ_TX_DONE | IRQ_TIMEOUT;
/// Routed to DIO1 during a reception.
pub const DIO1_RX: u16 = IRQ_RX_DONE | IRQ_CRC_ERR | IRQ_TIMEOUT;
/// Routed to DIO1 during a channel-activity detection.
pub const DIO1_CAD: u16 = IRQ_CAD_DONE | IRQ_CAD_DETECTED;

/// The bits [`rx_extend_ms`] reads out of the status register. They are
/// never routed to DIO1 — a preamble must not wake the wait, it must only be
/// visible to it afterwards — so they reach the guard through the latch mask
/// or not at all.
pub const RX_EXTEND_INPUTS: u16 = IRQ_PREAMBLE_DETECTED | IRQ_HEADER_VALID;

/// The eight `SetDioIrqParams` argument bytes, big-endian per mask.
///
/// `latch` is the enable mask (see the module docs); `dio1`, `dio2` and
/// `dio3` route latched interrupts to pins.
pub fn dio_irq_params(latch: u16, dio1: u16, dio2: u16, dio3: u16) -> [u8; 8] {
    [
        (latch >> 8) as u8,
        latch as u8,
        (dio1 >> 8) as u8,
        dio1 as u8,
        (dio2 >> 8) as u8,
        dio2 as u8,
        (dio3 >> 8) as u8,
        dio3 as u8,
    ]
}

/// `SetDioIrqParams` arguments for a transmission.
pub fn tx_irq_params() -> [u8; 8] {
    dio_irq_params(IRQ_LATCH_ALL, DIO1_TX, 0, 0)
}

/// `SetDioIrqParams` arguments for a reception.
///
/// Everything latches so the RX-extend guard can read PreambleDetected and
/// HeaderValid back; only [`DIO1_RX`] wakes the pin.
pub fn rx_irq_params() -> [u8; 8] {
    dio_irq_params(IRQ_LATCH_ALL, DIO1_RX, 0, 0)
}

/// `SetDioIrqParams` arguments for a channel-activity detection.
pub fn cad_irq_params() -> [u8; 8] {
    dio_irq_params(IRQ_LATCH_ALL, DIO1_CAD, 0, 0)
}

/// How much longer to wait for a reception that is still in progress, or
/// `None` to conclude the RX window now.
///
/// The software wait around a bounded RX window is sized from the window,
/// but `SetStopRxTimerOnPreambleDetect` means the window bounds the PREAMBLE
/// wait only: once a preamble arrives the hardware timer stops and the chip
/// receives to the end of the frame however long that takes. At slow
/// spreading factors a single frame is seconds of airtime, so the software
/// wait can expire with the frame still arriving. Concluding there — the
/// pre-#144 behaviour — puts the radio in standby mid-frame and drops a
/// packet that was about to be delivered whole.
///
/// So: if neither RxDone nor Timeout has fired but a preamble or header did
/// latch, the reception is live and gets one more max-single-frame airtime.
/// One extension only, so a preamble from a frame that never completes still
/// terminates the window.
///
/// `bw_hz` is zero before `configure_lora` has run, which is the one state
/// in which the airtime is not computable; the window simply concludes.
///
/// `preamble_symbols` is the programmed preamble of the link (both ends run
/// the same derived value): the frame still on the air carries it, so an
/// extension sized from the preamble-8 formula would expire up to
/// (preamble-8)*t_sym before the frame ends — 328 ms at SF12/BW125.
pub fn rx_extend_ms(
    flags: u16,
    bw_hz: u32,
    sf: u8,
    cr_denom: u8,
    preamble_symbols: u16,
) -> Option<u64> {
    if bw_hz == 0 {
        return None;
    }
    if flags & (IRQ_RX_DONE | IRQ_TIMEOUT) != 0 {
        return None;
    }
    if flags & RX_EXTEND_INPUTS == 0 {
        return None;
    }
    Some(
        crate::rnode::airtime_ms_with_preamble(
            (crate::rnode::MAX_SINGLE_PAYLOAD + 1) as u32,
            bw_hz,
            sf,
            cr_denom,
            preamble_symbols,
        )
        .max(1),
    )
}

/// How long a transmit may be held off by a reception the standing window is
/// already holding, or `None` to key up now.
///
/// Deliberately [`rx_extend_ms`] and not a second arithmetic. The two ask the
/// same question of the same three bits — *how much longer can this frame
/// still be arriving?* — and the answer is the same number: one maximum-size
/// frame at the live modulation. The chip does not expose the in-flight
/// header's length field (`GetRxBufferStatus` is valid only after `RxDone`),
/// so the true remaining airtime is not computable from anything a standing
/// window can be asked, and the largest legal frame is the tightest honest
/// upper bound either caller can have.
///
/// # `preamble=1` and `header=1` earn the same bound, on purpose
///
/// They are not the same evidence. A latched `HeaderValid` means an explicit
/// header passed its own CRC, so a real frame at this modulation is on the
/// air and its end is a fact; a bare `PreambleDetected` means only that
/// something started, and it may be noise. What they earn is nevertheless the
/// same, for three reasons:
///
/// 1. **This is a bound, not a delay.** The wait ends at the terminating IRQ,
///    so on a frame that completes, the tightness of the bound costs nothing.
///    Only a carrier that never completes ever spends it.
/// 2. **Being wrong is asymmetric.** A bound that is too long costs one
///    outgoing packet some latency, once, and only while something is on the
///    air. A bound that is too short costs the packet the deferral exists to
///    save — and a bare preamble, read at an unknown point inside a frame, is
///    exactly where a tight bound is most likely to cut a real reception off.
/// 3. **The alternative is a measurement, not a guess.** Whether a shorter
///    bare-preamble bound is worth its own arithmetic is answered by
///    `[SX_TX_DEFER] reason=preamble outcome=timeout` as a fraction of
///    `reason=preamble`. The reference firmware's false-preamble bound
///    (preamble time + header time, `reference/RNode_Firmware/sx126x.cpp:508`)
///    is the change to make if that fraction turns out to be material — made
///    then on a number rather than on a hunch.
///
/// What the two bits do change is the line: `reason=` carries which of them
/// earned the wait, so the two populations are separable in a capture even
/// though the bound is not.
///
/// `RxDone` or `Timeout` latched returns `None`, inherited from
/// [`rx_extend_ms`]: the window is holding a reception that has already
/// concluded, and there is nothing still arriving to wait for. Harvesting that
/// completed frame instead of tearing it down would be a second behaviour and
/// is deliberately not this one.
pub fn tx_defer_ms(
    flags: u16,
    bw_hz: u32,
    sf: u8,
    cr_denom: u8,
    preamble_symbols: u16,
) -> Option<u64> {
    rx_extend_ms(flags, bw_hz, sf, cr_denom, preamble_symbols)
}

/// Slack added on top of a computed on-air time when sizing the software
/// timeout around a started radio operation (TX completion, CAD completion).
///
/// It has to absorb only what is not airtime once the operation has been
/// keyed: TCXO start (~4 ms, the `SetDIO3AsTcxoCtrl` 0x0000FF timeout), PA
/// ramp (200 µs), the SPI+BUSY cost of the surrounding commands
/// (sub-millisecond each), and executor wake latency after the DIO1 edge.
/// CSMA waiting is out of scope: DIFS and contention backoff complete before
/// the driver enters `transmit()`. 250 ms is an order of magnitude above the
/// sum of those latencies while staying small against one slow-SF frame.
pub const SOFT_TIMEOUT_SLACK_MS: u64 = 250;

/// Bounded software wait when `bw_hz` is 0 (no `configure_lora` yet), the one
/// state in which no airtime is computable. An unconfigured radio cannot have
/// a frame on the air, so nothing real is aborted; this only bounds the wait
/// for a chip that will never raise the IRQ.
pub const UNCONFIGURED_WAIT_MS: u32 = 1_000;

/// Software timeout for one transmitted frame: its on-air time at the live
/// modulation plus [`SOFT_TIMEOUT_SLACK_MS`].
///
/// Replaces a fixed 5000 ms that predated SF12 support: a 184-byte announce
/// at SF12/BW125/CR4:8/preamble-18 is 10.69 s of airtime, so the driver's
/// expired wait put the chip in standby mid-air and no such frame was ever
/// completed — same family as the fixed 500 ms RX abort (#144).
pub fn tx_timeout_ms(
    frame_len: u32,
    bw_hz: u32,
    sf: u8,
    cr_denom: u8,
    preamble_symbols: u16,
) -> u32 {
    if bw_hz == 0 {
        return UNCONFIGURED_WAIT_MS;
    }
    let air =
        crate::rnode::airtime_ms_with_preamble(frame_len, bw_hz, sf, cr_denom, preamble_symbols);
    (air + SOFT_TIMEOUT_SLACK_MS).min(u32::MAX as u64) as u32
}

/// Software timeout for one channel-activity detection: the programmed
/// listening symbols plus the chip's processing tail (about half a symbol,
/// rounded up to one), plus [`SOFT_TIMEOUT_SLACK_MS`].
///
/// Replaces a per-SF millisecond table that assumed BW125: symbol time
/// doubles with every halving of bandwidth, so at SF12/BW31.25 an 8-symbol
/// CAD (1.05 s) overran the table's 800 ms and was aborted on every attempt.
pub fn cad_timeout_ms(bw_hz: u32, sf: u8, cad_symbols: u8) -> u32 {
    if bw_hz == 0 {
        return UNCONFIGURED_WAIT_MS;
    }
    let t_sym_us = (1u64 << sf) * 1_000_000 / bw_hz as u64;
    let cad_us = (cad_symbols as u64 + 1) * t_sym_us;
    (cad_us.div_ceil(1_000) + SOFT_TIMEOUT_SLACK_MS).min(u32::MAX as u64) as u32
}

/// Whether `SetModulationParams` must enable the low-data-rate optimisation
/// for the given modulation.
///
/// LDRO is keyed to symbol duration, not to a spreading-factor/bandwidth-code
/// pair: the SX126x needs it when a symbol stretches past ~16 ms. Both sides
/// of a link must agree — an LDRO mismatch between ends kills decoding
/// entirely — so this replicates the reference RNode firmware's decision
/// bit for bit (`sx126x.cpp:725-729 handleLowDataRate()`, identical in
/// `sx127x.cpp:457-467`):
///
/// ```c
/// if ( long( (1<<_sf) / (getSignalBandwidth()/1000)) > 16)
/// ```
///
/// i.e. integer-millisecond symbol duration strictly above 16. The integer
/// truncation is part of the wire contract: SF11/BW125 and SF12/BW250 sit at
/// exactly 16.384 ms and the reference (and therefore every RNode peer)
/// runs them with LDRO OFF, while the narrow interleaved SX1262 bandwidths
/// (10.42/20.83/41.67 kHz) push even mid spreading factors far past the
/// threshold. `bw_hz` comes from the chip's bandwidth-code table
/// (`bw_code_to_hz`); the quotient `bw_hz / 1000` is identical for the
/// reference's rounded table values, so the decisions coincide on the whole
/// code domain. Predicates over the raw SX1262 register code cannot express
/// this: the code space is not monotonic in bandwidth (0x08-0x0A are the
/// narrowest bandwidths but the numerically largest codes).
///
/// `bw_hz` of 0 (unconfigured, or an unknown code) reports no LDRO.
pub fn ldro_enabled(bw_hz: u32, sf: u8) -> bool {
    let bw_khz = bw_hz as u64 / 1000;
    if bw_khz == 0 {
        return false;
    }
    (1u64 << sf) / bw_khz > 16
}

// ---------------------------------------------------------------------------
// Transmit power (Codeberg #349)
// ---------------------------------------------------------------------------

/// `SetPaConfig` (datasheet §13.1.14).
pub const OP_SET_PA_CONFIG: u8 = 0x95;
/// `SetTxParams` (datasheet §13.4.4).
pub const OP_SET_TX_PARAMS: u8 = 0x8E;
/// Over-current protection, `REG_OCP` (`reference/RNode_Firmware/sx126x.cpp:62`
/// `REG_OCP_6X`). One byte, 2.5 mA per step.
pub const REG_OCP: u16 = 0x08E7;

/// The one `SetPaConfig` argument block the high-power PA is driven with:
/// `PADutyCycle`, `HPMax`, `DeviceSel`, `PALut`.
///
/// Byte-for-byte the reference's (`reference/RNode_Firmware/sx126x.cpp:722-726`):
/// duty cycle 0x04 and `HPMax` 0x07 are the datasheet's +22 dBm row (Table
/// 13-21), `DeviceSel` 0x00 selects the SX1262 rather than the SX1261, and
/// `PALut` is reserved at 0x01.
///
/// **One config for every power, not four.** The driver used to pick one of
/// the table's four rows from the requested power and then send `SetTxParams`
/// a hardcoded +22, which made the PA row the only thing that decided output
/// — four reachable powers, nothing below 14 dBm, and a request for 2 dBm on
/// the air at roughly 14. The rows exist for PA *efficiency* at a given
/// output, not to set the output; the output is [`OP_SET_TX_PARAMS`]'s first
/// argument, which is what [`plan_tx_power`] now drives. Matching the
/// reference here is what makes an LNode and an RNode radiate the same for
/// the same configured number, which is the property the 23 mixed hardware
/// scenarios are written against.
pub const PA_CONFIG_HIGH_POWER: [u8; 4] = [0x04, 0x07, 0x00, 0x01];

/// Lowest power the SX1262 high-power PA accepts (datasheet §13.4.4, and
/// `sx126x.cpp:728-729` clamps to the same).
pub const TX_POWER_MIN_DBM: i8 = -9;
/// Highest power the SX1262 high-power PA accepts.
pub const TX_POWER_MAX_DBM: i8 = 22;

/// PA ramp time byte for `SetTxParams`: 0x04 is 200 µs (datasheet Table 13-41).
///
/// **A deliberate difference from the reference**, which uses 0x02 (40 µs,
/// `sx126x.cpp:734`). Ours is the slower ramp and it is kept: a longer ramp
/// spreads the switching transient over five times the interval, so it is the
/// quieter of the two spectrally, and nothing about it changes the steady-state
/// output power the acceptance measures — the ramp is over before the preamble
/// is. Changing it would alter what every archived LNode capture was taken
/// with, for no gain this batch can name. Stated here so the next reader does
/// not take it for an oversight.
pub const PA_RAMP_200US: u8 = 0x04;

/// `REG_OCP` value the high-power PA runs with: 0x38, i.e. 140 mA.
///
/// This is the SX1262's own documented default — `SetPaConfig` resets the
/// register to it whenever `DeviceSel` selects the SX1262 — so writing it
/// changes nothing about what the chip does today. It is written anyway,
/// explicitly and after [`OP_SET_PA_CONFIG`], because the alternative is to
/// depend on a reset value for the one limit that can silently swallow the
/// power this batch exists to deliver.
///
/// **A second deliberate difference from the reference**, which writes
/// `OCP_TUNED` = 0x28 = 100 mA (`Boards.h:930-933`, the `#ifndef` fallback
/// every SX1262 board falls through to). 100 mA is *below* the datasheet's own
/// typical supply current at +22 dBm (118 mA), so the reference's value can
/// only limit the top of the range, never extend it. Taking it would be the
/// one way this batch produced less power rather than more. The deviation rule
/// is satisfied on all three counts: nothing about OCP is on the wire, no peer
/// can observe it, and the direction is strictly towards delivering the
/// configured power (Priority 1).
pub const OCP_HIGH_POWER: u8 = 0x38;

/// What the driver programmed the PA with, and what was asked for.
///
/// Returned by [`program_tx_power`] so the caller can state it on the boot
/// log without re-deriving anything: every field here is a value that was
/// actually put on the SPI bus, not a value that was intended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxPowerProgram {
    /// The configured power, as the host or the stored profile stated it.
    pub requested_dbm: i8,
    /// The first `SetTxParams` argument, i.e. what the chip was told to
    /// radiate. Equal to `requested_dbm` unless it was outside the part's
    /// range.
    pub programmed_dbm: i8,
    /// The `SetPaConfig` argument block, as written.
    pub pa_config: [u8; 4],
    /// The second `SetTxParams` argument.
    pub ramp: u8,
    /// The `REG_OCP` byte, as written.
    pub ocp: u8,
    /// Whether the request had to be brought into range.
    pub clamped: bool,
}

/// Decide what to program for a requested output power.
///
/// The whole decision is the clamp to
/// [`TX_POWER_MIN_DBM`]`..=`[`TX_POWER_MAX_DBM`], because the part takes every
/// value in that range and the request is passed straight through
/// (`sx126x.cpp:728-735` does exactly this). There is no rounding and no
/// profile table any more: the four-point table was an artefact of deriving
/// output from `SetPaConfig`, and deriving it from `SetTxParams` instead makes
/// all 32 points reachable — including the negatives `lnflash --radio-txpower`
/// has always accepted (`lnflash/src/radio.rs:76`).
///
/// **A value the chip cannot do is clamped and announced, never refused.**
/// The announcement is the caller's `[SX_TX_POWER]` line, and
/// [`TxPowerProgram::clamped`] is what it reports.
pub fn plan_tx_power(requested_dbm: i8) -> TxPowerProgram {
    let programmed_dbm = requested_dbm.clamp(TX_POWER_MIN_DBM, TX_POWER_MAX_DBM);
    TxPowerProgram {
        requested_dbm,
        programmed_dbm,
        pa_config: PA_CONFIG_HIGH_POWER,
        ramp: PA_RAMP_200US,
        ocp: OCP_HIGH_POWER,
        clamped: programmed_dbm != requested_dbm,
    }
}

/// The command half of the SPI port, for sequences this module owns.
///
/// [`RegisterBus`] covers the register accesses; a power change is a
/// three-op sequence of two opcodes and one register write, and the order
/// among them matters — `SetPaConfig` resets `REG_OCP`, so an OCP written
/// before it would be thrown away. That ordering is exactly the kind of thing
/// a host test must be able to watch, which is why the sequence lives here
/// and not in the driver.
pub trait CommandBus: RegisterBus {
    /// Issue an opcode with its argument bytes.
    fn write_cmd(
        &mut self,
        opcode: u8,
        args: &[u8],
    ) -> impl core::future::Future<Output = Result<(), Self::Error>>;
}

/// Program the PA for `requested_dbm` and report what was written.
///
/// Three operations, in the reference's order
/// (`reference/RNode_Firmware/sx126x.cpp:722-735`):
///
/// 1. `SetPaConfig` with [`PA_CONFIG_HIGH_POWER`] — also the point at which
///    the chip resets `REG_OCP`;
/// 2. `REG_OCP` with [`OCP_HIGH_POWER`], which is why it comes after;
/// 3. `SetTxParams` with the clamped power and [`PA_RAMP_200US`].
pub async fn program_tx_power<B: CommandBus>(
    bus: &mut B,
    requested_dbm: i8,
) -> Result<TxPowerProgram, B::Error> {
    let plan = plan_tx_power(requested_dbm);
    bus.write_cmd(OP_SET_PA_CONFIG, &plan.pa_config).await?;
    bus.write_reg(REG_OCP, plan.ocp).await?;
    bus.write_cmd(OP_SET_TX_PARAMS, &[plan.programmed_dbm as u8, plan.ramp])
        .await?;
    Ok(plan)
}

/// Decode a LoRa `GetPacketStatus` (opcode 0x14) response into
/// `(rssi_dbm, snr_db)`.
///
/// Datasheet §13.5.3, Table 13-77: byte 0 is `RssiPkt` in -0.5 dB steps,
/// byte 1 is `SnrPkt` as a signed value in 0.25 dB steps. Byte 2
/// (`SignalRssiPkt`, the RSSI of the despread signal) is not decoded here —
/// no caller reads it yet.
///
/// Both results are rounded to whole dB. The rounding is the driver's
/// historical one and is deliberately kept: `snr` adds 2 before dividing by 4,
/// i.e. rounds half away from zero for positive values and towards zero for
/// negative ones. Every SNR ever logged by an LNode used this, so changing it
/// would put a step in the middle of the archived measurements.
///
/// Here rather than in the driver because the driver cross-compiles to
/// `thumbv7em-none-eabihf` and has no host test target (see the module docs);
/// this is arithmetic on three bytes and wants to be checked without a board.
/// It is decoded on both the CRC-pass and the CRC-fail path, so a drift
/// between the two would silently make the failing population look different
/// from the passing one — which is exactly the comparison it exists for.
pub fn packet_status_dbm(buf: [u8; 3]) -> (i16, i16) {
    let rssi = -(buf[0] as i16) / 2;
    let snr = ((buf[1] as i8) as i16 + 2) / 4;
    (rssi, snr)
}

/// `RxGain`, the SX1262's receive-path gain selector (datasheet §15 key
/// register table, transcribed in `docs/src/sx1262-datasheet-reference.md`).
///
/// There is no AGC on this part — the comment the reference firmware attaches
/// to the same address, `sx126x.cpp:63` ("No agc in sx1262") — so whichever of
/// the two documented values is written at bring-up is the gain the receiver
/// runs at for the whole session.
pub const REG_RX_GAIN: u16 = 0x08AC;

/// `RxGain` power-saving value: the chip's own reset default.
///
/// Present so the boosted value has something to be distinguished from, and so
/// the choice between the two is a named one rather than a bare literal. The
/// driver never writes it — leaving the register untouched has the same
/// effect, which is precisely how this setting stayed invisible.
pub const RX_GAIN_POWER_SAVING: u8 = 0x94;

/// `RxGain` boosted value, what the reference RNode firmware writes
/// unconditionally at bring-up (`sx126x.cpp:361`, `writeRegister(REG_LNA_6X,
/// 0x96)`) and what our own datasheet transcription prescribes as step 18 of
/// the init sequence.
///
/// Boosted gain trades receiver current for sensitivity. The size of both
/// halves of that trade is a documented gap: no Semtech PDF is in this tree
/// and the local transcription gives the two values without their electrical
/// characteristics.
pub const RX_GAIN_BOOSTED: u8 = 0x96;

/// `IqPolarity` (errata 15.4). Not a configuration register in its own right:
/// `SetPacketParams` writes it, and the erratum is that it writes it wrongly,
/// so every `SetPacketParams` has to be followed by a correction.
pub const REG_IQ_POLARITY: u16 = 0x0736;

/// The bit within [`REG_IQ_POLARITY`] that errata 15.4 corrects.
pub const IQ_POLARITY_BIT: u8 = 0x04;

/// Errata 15.4: the value [`REG_IQ_POLARITY`] must hold after a
/// `SetPacketParams`, given what it holds now.
///
/// Bit 2 is SET for standard IQ and CLEARED for inverted IQ
/// (`docs/src/sx1262-datasheet-reference.md` §15.4; the reference firmware
/// applies exactly this at `sx126x.cpp:291-298`, after every
/// `SetPacketParams`). Every other bit is left as read — the register holds
/// state this driver has no business rewriting.
///
/// Read-modify-write rather than a constant because the bits beside bit 2 are
/// not documented in the local transcription. A blind write would be a guess
/// about seven bits in exchange for knowing one.
pub fn iq_polarity_value(current: u8, inverted_iq: bool) -> u8 {
    if inverted_iq {
        current & !IQ_POLARITY_BIT
    } else {
        current | IQ_POLARITY_BIT
    }
}

/// `TxModulation` (errata 15.1). Read here, never written.
///
/// Bit 2 must be CLEAR at 500 kHz bandwidth and SET at every other bandwidth
/// (`docs/src/sx1262-datasheet-reference.md` §15.1). The reference firmware
/// corrects it from `optimizeModemSensitivity` (`sx126x.cpp:796-803`) on every
/// bandwidth change; we do not, and whether that is a real gap or a formality
/// depends on the value the chip already holds — which is why the address is
/// here at all. The register is on the transmit side, and the transmit side
/// stays untouched until the batch that has the attenuator in line: a read is
/// not a change.
pub const REG_TX_MODULATION: u16 = 0x0889;

/// The two SPI primitives the register probes below need.
///
/// # Why a trait
///
/// The probes are read-write-read brackets, and a bracket is a *sequence* —
/// the one kind of thing this module exists to keep out of the driver. The
/// premise for moving the register constants here was that "a constant that
/// lives only there is a constant nobody can check"
/// (`leviculum-nrf/src/sx1262.rs`); the same is true one level up. A sequence
/// that lives only in `leviculum-nrf::sx1262` cannot be run against a test,
/// so nothing can distinguish a probe that reads the chip twice from one that
/// reads it once and reports a constant for the other half. With the sequence
/// behind this trait, a host test drives it against a fake register file and
/// that distinction is a `cargo test` away.
///
/// The driver implements it in two forwarding methods over its existing
/// `read_register`/`write_register`. Nothing else in the driver changes.
pub trait RegisterBus {
    /// Whatever the transport fails with. The driver's is `sx1262::Error`.
    type Error;

    /// Read one register byte.
    fn read_reg(
        &mut self,
        addr: u16,
    ) -> impl core::future::Future<Output = Result<u8, Self::Error>>;

    /// Write one register byte.
    fn write_reg(
        &mut self,
        addr: u16,
        value: u8,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>>;
}

/// What the boot-time register probe found, in the order the log line prints
/// it.
///
/// [`Display`](core::fmt::Display) is the log line's body, so the shape the
/// host greps is pinned by a host test rather than by a `format_args!` in a
/// crate that has no test target.
pub struct RxInitProbe {
    /// [`REG_RX_GAIN`] as the chip held it before our write.
    pub rx_gain_before: u8,
    /// [`REG_RX_GAIN`] re-read from the chip after our write. Read back rather
    /// than assumed: the written value would only say what we sent.
    pub rx_gain_after: u8,
    /// [`REG_TX_MODULATION`] as found. Never written, and read before
    /// `SetModulationParams` has run, so this is the chip's own value.
    pub tx_modulation: u8,
}

impl core::fmt::Display for RxInitProbe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "rxgain_before=0x{:02X} rxgain_after=0x{:02X} txmod=0x{:02X}",
            self.rx_gain_before, self.rx_gain_after, self.tx_modulation
        )
    }
}

/// What the first `SetPacketParams` of a boot found.
///
/// Separate from [`RxInitProbe`] because the erratum-15.4 correction lives in
/// `SetPacketParams`, which has not run yet when `init_radio` ends: there is
/// no "after" to report there. See [`apply_iq_polarity`].
pub struct PacketParamsProbe {
    /// [`REG_IQ_POLARITY`] as `SetPacketParams` left it — the value errata
    /// 15.4 says does not follow from the IQ setting the command was given.
    pub iq_before: u8,
    /// [`REG_IQ_POLARITY`] re-read from the chip after the correction.
    pub iq_after: u8,
    /// [`REG_TX_MODULATION`] as found, this time *after*
    /// `SetModulationParams` has run. The pair of readings — this one and
    /// [`RxInitProbe::tx_modulation`] — is what says whether the modulation
    /// command disturbs the register errata 15.1 is about.
    pub tx_modulation: u8,
}

impl core::fmt::Display for PacketParamsProbe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "iq_before=0x{:02X} iq_after=0x{:02X} txmod=0x{:02X}",
            self.iq_before, self.iq_after, self.tx_modulation
        )
    }
}

/// Which call site armed the receiver.
///
/// The vocabulary is the LoRa loop's own, not a new one: each variant names
/// the window `lora_task` already names, in the log line that bounds it or in
/// the comment that explains it. A capture is then readable without
/// cross-referencing line numbers, which is the only reason the field exists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RxSite {
    /// Outgoing queue empty: single-mode continuous listen, no hardware
    /// timeout. This is the only site that arms with `timeout_ms == 0`.
    Idle,
    /// The bounded window after a transmission, waiting for the peer's
    /// ack or reply.
    Ack,
    /// Listening through a CSMA backoff, after CAD reported the channel
    /// busy (`[LORA_CAD] busy=true`).
    Csma,
    /// Listening through the randomised acquisition jitter before the
    /// first CAD of a channel acquisition (`[LORA_JITTER]`).
    Jitter,
    /// Listening while the regulatory airtime lock holds a queued frame
    /// (`[LORA_AIRTIME_LOCK] ... holding`).
    Hold,
    /// The peer-turn yield after consecutive empty ack windows
    /// (`[T114_PEER_YIELD]`).
    Yield,
}

impl RxSite {
    /// The stable tag the log line carries. Short on purpose: this field is
    /// on every arm, and the arm is the most frequent line the firmware has.
    pub const fn tag(self) -> &'static str {
        match self {
            RxSite::Idle => "idle",
            RxSite::Ack => "ack",
            RxSite::Csma => "csma",
            RxSite::Jitter => "jitter",
            RxSite::Hold => "hold",
            RxSite::Yield => "yield",
        }
    }
}

/// Which call site stood a listening window down.
///
/// The other half of [`RxSite`]: that one names the window, this one names the
/// path that ended it. `[SX_RX_TEARDOWN]` carries this rather than the
/// window's own tag because the window's tag is recoverable from a capture —
/// it is whatever the last `[SX_RX_ARM]` said — and the path is not.
///
/// The vocabulary is deliberately every caller and not just the key-ups. A
/// batch that instrumented only the three transmit paths, on the reasoning
/// that a re-arm is "the same window continuing", missed the site that
/// mattered: the re-arm's own head issues a real `SetStandby`, and a standby
/// during a frame's airtime ends that reception.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RxTeardownBy {
    /// An arming that wants different parameters than the standing window has.
    /// Not a key-up, and the one this vocabulary exists to make countable.
    Arm,
    /// The idle `select`'s outgoing arm: the daemon has data, so the receive
    /// future was dropped and the window it left standing is spent here.
    Select,
    /// The CAD that opens the CSMA path.
    Cad,
    /// A transmission, on the path that reaches it without a CAD.
    Tx,
    /// The software wait around a window expired with neither terminating IRQ
    /// set, so the chip may still be in RX and is stood down explicitly.
    RxWait,
    /// A reconfiguration of the modulation, which cannot run from RX.
    Config,
}

impl RxTeardownBy {
    /// The stable tag the log line carries.
    pub const fn tag(self) -> &'static str {
        match self {
            RxTeardownBy::Arm => "arm",
            RxTeardownBy::Select => "select",
            RxTeardownBy::Cad => "cad",
            RxTeardownBy::Tx => "tx",
            RxTeardownBy::RxWait => "rxwait",
            RxTeardownBy::Config => "config",
        }
    }
}

/// What `dark_ms` renders when no previous window end is on record.
///
/// A word, not a number. At boot there is no previous window, and any digit
/// printed there — a zero, or an uptime-sized gap — reads downstream as a
/// measurement of something that was never measured.
pub const DARK_MS_FIRST: &str = "first";

/// One arming of the receiver, in the order the log line prints it.
///
/// [`Display`](core::fmt::Display) is the line's body, so the shape the host
/// greps is pinned by a host test rather than by a `format_args!` in a crate
/// that has no test target.
pub struct RxArm {
    /// Which call site armed the radio.
    pub site: RxSite,
    /// The timeout this window was given, as passed to `receive()`.
    /// Zero is single mode: no hardware timeout, the radio listens until a
    /// packet arrives. Every bounded caller clamps to at least 1 ms
    /// (`post_tx_rx_window_ms`, and the CSMA backoff's own clamp), so a zero
    /// here is never a bounded window that happened to round down.
    pub timeout_ms: u32,
    /// Milliseconds between the previous window's end and this arming — the
    /// span in which the radio was NOT listening. `None` when no previous
    /// window end is on record; see [`DARK_MS_FIRST`].
    pub dark_ms: Option<u64>,
}

impl core::fmt::Display for RxArm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "site={} timeout_ms={} dark_ms=",
            self.site.tag(),
            self.timeout_ms
        )?;
        match self.dark_ms {
            Some(ms) => write!(f, "{ms}"),
            None => f.write_str(DARK_MS_FIRST),
        }
    }
}

/// Carries the instant the receiver stopped listening from one window to the
/// next arming, so the gap between them is computed on the board.
///
/// The gap cannot be derived from a capture afterwards. The completion line's
/// `duration_ms` brackets the whole `receive()` call — the IRQ setup before
/// the `SetRx` and the buffer readout after the IRQ — so `t - duration_ms` is
/// not the instant the radio was armed, and a run of windows says nothing
/// about whether they abut.
///
/// The two instants are deliberately taken from opposite ends of a window:
/// [`window_ended`](Self::window_ended) at the terminating IRQ,
/// [`arm`](Self::arm) at the next `SetRx`. Feeding either end of the same
/// window into both is the mistake this type exists to make impossible to
/// write by accident, and `dark_ms_is_measured_from_the_window_end` is the
/// control that catches it.
pub struct RxArmClock {
    prev_window_end_ms: Option<u64>,
}

impl RxArmClock {
    /// No window has ended yet: the next arming reports [`DARK_MS_FIRST`].
    pub const fn new() -> Self {
        Self {
            prev_window_end_ms: None,
        }
    }

    /// Record `now_ms` as the instant the receiver stopped listening.
    ///
    /// Called when the terminating IRQ is observed, before any of the SPI
    /// readout that follows it: that readout is dark time and belongs to the
    /// next gap, not to the window.
    ///
    /// Idempotent-by-overwrite, which is what the RX extension needs: the
    /// first call marks the end of the software wait, and if the reception
    /// turns out to still be live the extended wait supersedes it.
    pub fn window_ended(&mut self, now_ms: u64) {
        self.prev_window_end_ms = Some(now_ms);
    }

    /// Report an arming at `now_ms`, consuming the recorded window end.
    ///
    /// Consuming, not peeking: if a window ends without reaching
    /// [`window_ended`](Self::window_ended) — an SPI error returning early —
    /// the next arming has no honest end to measure from, and says
    /// [`DARK_MS_FIRST`] rather than silently measuring from a stale one.
    pub fn arm(&mut self, site: RxSite, timeout_ms: u32, now_ms: u64) -> RxArm {
        let dark_ms = self
            .prev_window_end_ms
            .take()
            .map(|end| now_ms.saturating_sub(end));
        RxArm {
            site,
            timeout_ms,
            dark_ms,
        }
    }
}

impl Default for RxArmClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Write [`RX_GAIN_BOOSTED`] and report what the register held on either side
/// of the write, plus [`REG_TX_MODULATION`] as found.
///
/// Called once, at the end of the driver's `init_radio`. The write is the one
/// this function exists to make observable: putting it here rather than in the
/// driver is what lets a host test watch it happen, and what makes "skip the
/// write and the two values are equal" a control somebody can run.
///
/// Three reads and one write, once per boot.
pub async fn probe_rx_init<B: RegisterBus>(bus: &mut B) -> Result<RxInitProbe, B::Error> {
    let rx_gain_before = bus.read_reg(REG_RX_GAIN).await?;
    bus.write_reg(REG_RX_GAIN, RX_GAIN_BOOSTED).await?;
    let rx_gain_after = bus.read_reg(REG_RX_GAIN).await?;
    let tx_modulation = bus.read_reg(REG_TX_MODULATION).await?;
    Ok(RxInitProbe {
        rx_gain_before,
        rx_gain_after,
        tx_modulation,
    })
}

/// Apply errata 15.4 to [`REG_IQ_POLARITY`], optionally reporting what the
/// register held on either side.
///
/// `probe` is false on all but the first call of a boot, and it is what keeps
/// the diagnostic off the per-frame path: `SetPacketParams` runs once per
/// transmitted frame, so the correction's cost stays the one read and one
/// write it was, and the two extra reads are paid once.
///
/// Returns `None` when `probe` is false — there is nothing to report, not a
/// report of nothing.
pub async fn apply_iq_polarity<B: RegisterBus>(
    bus: &mut B,
    inverted_iq: bool,
    probe: bool,
) -> Result<Option<PacketParamsProbe>, B::Error> {
    let iq_before = bus.read_reg(REG_IQ_POLARITY).await?;
    bus.write_reg(REG_IQ_POLARITY, iq_polarity_value(iq_before, inverted_iq))
        .await?;
    if !probe {
        return Ok(None);
    }
    let iq_after = bus.read_reg(REG_IQ_POLARITY).await?;
    let tx_modulation = bus.read_reg(REG_TX_MODULATION).await?;
    Ok(Some(PacketParamsProbe {
        iq_before,
        iq_after,
        tx_modulation,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SF10 / 62.5 kHz / 4:5 — the slow end of the lab rig, where a single
    /// frame is seconds of airtime and the guard matters.
    const SLOW: (u32, u8, u8) = (62_500, 10, 5);
    /// The rig's derived programmed preamble at the SLOW profile
    /// (`rnode::derive_preamble_symbols(10, 5, 62_500)`).
    const SLOW_PREAMBLE: u16 = 18;

    fn latch_mask(params: [u8; 8]) -> u16 {
        u16::from_be_bytes([params[0], params[1]])
    }

    fn dio1_mask(params: [u8; 8]) -> u16 {
        u16::from_be_bytes([params[2], params[3]])
    }

    /// The reachability test. It is the whole point of the module: the guard
    /// reads PreambleDetected and HeaderValid, so the reception's latch mask
    /// has to admit them. Against the pre-#144 mask (`DIO1_RX` in both
    /// positions) this fails, which is the first time that guard's
    /// precondition was ever checked for being satisfiable at all.
    #[test]
    fn rx_latch_mask_admits_what_the_extend_guard_reads() {
        let latch = latch_mask(rx_irq_params());
        assert_eq!(
            latch & RX_EXTEND_INPUTS,
            RX_EXTEND_INPUTS,
            "rx latch mask {latch:#06x} drops bits {RX_EXTEND_INPUTS:#06x}, \
             which rx_extend_ms tests: the guard could never fire"
        );
    }

    /// The other half of the same fact: those bits must not wake DIO1. If
    /// they did, the software wait would return on the preamble instead of
    /// on the frame, and the extension would be measuring the wrong thing.
    #[test]
    fn rx_extend_inputs_are_not_routed_to_dio1() {
        assert_eq!(dio1_mask(rx_irq_params()) & RX_EXTEND_INPUTS, 0);
        assert_eq!(dio1_mask(rx_irq_params()), DIO1_RX);
    }

    #[test]
    fn tx_and_cad_latch_everything_and_route_narrowly() {
        assert_eq!(latch_mask(tx_irq_params()), IRQ_LATCH_ALL);
        assert_eq!(dio1_mask(tx_irq_params()), DIO1_TX);
        assert_eq!(latch_mask(cad_irq_params()), IRQ_LATCH_ALL);
        assert_eq!(dio1_mask(cad_irq_params()), DIO1_CAD);
    }

    #[test]
    fn dio2_and_dio3_are_never_routed() {
        for params in [tx_irq_params(), rx_irq_params(), cad_irq_params()] {
            assert_eq!(&params[4..8], &[0, 0, 0, 0]);
        }
    }

    #[test]
    fn params_are_big_endian_per_mask() {
        assert_eq!(
            dio_irq_params(0x1234, 0x5678, 0x9abc, 0xdef0),
            [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]
        );
    }

    /// The guard firing, observed: a preamble latched, nothing concluded the
    /// window, so the wait is extended by one max-single-frame airtime.
    #[test]
    fn preamble_alone_extends_the_window() {
        let (bw, sf, cr) = SLOW;
        let extend = rx_extend_ms(IRQ_PREAMBLE_DETECTED, bw, sf, cr, SLOW_PREAMBLE)
            .expect("guard must fire");
        assert_eq!(
            extend,
            crate::rnode::airtime_ms_with_preamble(
                (crate::rnode::MAX_SINGLE_PAYLOAD + 1) as u32,
                bw,
                sf,
                cr,
                SLOW_PREAMBLE
            )
        );
        // Worth stating as a number: at SF10/62.5k this is seconds, which is
        // why expiring the software wait mid-frame lost whole packets.
        assert!(
            extend > 1_000,
            "expected a multi-second frame, got {extend}ms"
        );
    }

    #[test]
    fn header_alone_extends_the_window() {
        let (bw, sf, cr) = SLOW;
        assert!(rx_extend_ms(IRQ_HEADER_VALID, bw, sf, cr, SLOW_PREAMBLE).is_some());
    }

    #[test]
    fn a_concluded_window_is_not_extended() {
        let (bw, sf, cr) = SLOW;
        // RxDone wins even with a preamble still latched from the same frame.
        assert_eq!(
            rx_extend_ms(
                IRQ_PREAMBLE_DETECTED | IRQ_RX_DONE,
                bw,
                sf,
                cr,
                SLOW_PREAMBLE
            ),
            None
        );
        assert_eq!(
            rx_extend_ms(
                IRQ_PREAMBLE_DETECTED | IRQ_TIMEOUT,
                bw,
                sf,
                cr,
                SLOW_PREAMBLE
            ),
            None
        );
    }

    #[test]
    fn a_silent_channel_is_not_extended() {
        let (bw, sf, cr) = SLOW;
        assert_eq!(rx_extend_ms(0, bw, sf, cr, SLOW_PREAMBLE), None);
        // CAD bits are not evidence of an inbound frame.
        assert_eq!(
            rx_extend_ms(IRQ_CAD_DETECTED, bw, sf, cr, SLOW_PREAMBLE),
            None
        );
    }

    #[test]
    fn an_unconfigured_radio_is_not_extended() {
        assert_eq!(
            rx_extend_ms(IRQ_PREAMBLE_DETECTED, 0, 10, 5, SLOW_PREAMBLE),
            None
        );
    }

    /// The transmit deferral is the receive extension, on every input, and
    /// that identity is the claim rather than a coincidence: two functions
    /// answering "how much longer can this frame be arriving?" with different
    /// arithmetic is how a guard and the window it guards drift apart.
    #[test]
    fn the_transmit_deferral_is_the_receive_extension() {
        let (bw, sf, cr) = SLOW;
        for flags in [
            0,
            IRQ_PREAMBLE_DETECTED,
            IRQ_HEADER_VALID,
            IRQ_PREAMBLE_DETECTED | IRQ_HEADER_VALID,
            IRQ_PREAMBLE_DETECTED | IRQ_RX_DONE,
            IRQ_PREAMBLE_DETECTED | IRQ_TIMEOUT,
            IRQ_HEADER_VALID | IRQ_RX_DONE | IRQ_CRC_ERR,
            IRQ_CAD_DETECTED,
            IRQ_LATCH_ALL,
        ] {
            assert_eq!(
                tx_defer_ms(flags, bw, sf, cr, SLOW_PREAMBLE),
                rx_extend_ms(flags, bw, sf, cr, SLOW_PREAMBLE),
                "flags={flags:#06x}"
            );
        }
        // And on the one input that has no airtime at all.
        assert_eq!(
            tx_defer_ms(IRQ_PREAMBLE_DETECTED, 0, sf, cr, SLOW_PREAMBLE),
            None
        );
    }

    /// A bare preamble and a decoded header earn the same bound. Asserted
    /// rather than left implicit: a later "tighten the preamble case" edit
    /// that does not also state its measurement has to go red here first.
    #[test]
    fn a_bare_preamble_and_a_decoded_header_earn_the_same_bound() {
        let (bw, sf, cr) = SLOW;
        let preamble =
            tx_defer_ms(IRQ_PREAMBLE_DETECTED, bw, sf, cr, SLOW_PREAMBLE).expect("defers");
        let header = tx_defer_ms(
            IRQ_PREAMBLE_DETECTED | IRQ_HEADER_VALID,
            bw,
            sf,
            cr,
            SLOW_PREAMBLE,
        )
        .expect("defers");
        assert_eq!(preamble, header);
    }

    /// The starvation bound, as a number, at the profile the sweep runs
    /// (SF8/BW125/CR4:5, 18-symbol derived preamble).
    ///
    /// One deferral is spent per transmit at the one site that defers, so this
    /// figure IS the worst case a busy channel can impose on an outgoing
    /// packet — a bounded, single-frame wait, not an unbounded hold. It is the
    /// same order as the 672 ms the board reports for its own frames at this
    /// profile, which is the point: the transmitter waits about as long as the
    /// frame it is waiting for.
    #[test]
    fn the_deferral_bound_is_one_frame_and_it_is_a_number() {
        let bound =
            tx_defer_ms(IRQ_PREAMBLE_DETECTED, 125_000, 8, 5, 18).expect("a preamble defers");
        assert_eq!(bound, 728);
        // The same shape at the slow end of the rig, where it is seconds and
        // still finite.
        let (bw, sf, cr) = SLOW;
        let slow = tx_defer_ms(IRQ_PREAMBLE_DETECTED, bw, sf, cr, SLOW_PREAMBLE).expect("defers");
        assert_eq!(slow, 4_756);
    }

    /// The defect instance: a 184-byte announce at SF12/BW125/CR4:8 with the
    /// rig's programmed 18-symbol preamble is 10.69 s on the air. The fixed
    /// 5000 ms wait aborted every one of them (six 2026-07-30 captures, 10/10
    /// `TX err frame 0: Timeout`, zero `TX done`). The timeout must cover the
    /// frame, with nothing on top but the stated slack.
    #[test]
    fn tx_timeout_covers_the_sf12_announce() {
        let air = crate::rnode::airtime_ms_with_preamble(184, 125_000, 12, 8, 18);
        assert!(air >= 10_690, "premise drifted: announce airtime {air}ms");
        assert_eq!(
            tx_timeout_ms(184, 125_000, 12, 8, 18) as u64,
            air + SOFT_TIMEOUT_SLACK_MS
        );
    }

    /// The other direction: at SF7 the same frame is ~308 ms, and the timeout
    /// must not smuggle in a multi-second wait that would mask a wedged radio.
    #[test]
    fn tx_timeout_stays_sane_at_sf7() {
        let t = tx_timeout_ms(184, 125_000, 7, 5, 18);
        assert!(t < 1_000, "wedged-radio detection took {t}ms");
    }

    /// Why the defect only ever surfaced at SF12: the largest SF10 rig frame
    /// (131 B) is ~2 s of airtime, comfortably inside the old fixed 5000 ms.
    #[test]
    fn sf10_frames_fit_the_old_fixed_timeout() {
        let air = crate::rnode::airtime_ms_with_preamble(131, 125_000, 10, 8, 18);
        assert!(
            air > 1_000 && air < 5_000,
            "boundary premise drifted: {air}ms"
        );
        assert!(tx_timeout_ms(131, 125_000, 10, 8, 18) as u64 >= air + SOFT_TIMEOUT_SLACK_MS);
    }

    /// The programmed preamble is charged: 10 extra symbols at SF12/BW125 are
    /// 327 ms, more than the whole slack, so a preamble-blind timeout would
    /// already have spent its margin before the payload started.
    #[test]
    fn tx_timeout_charges_the_programmed_preamble() {
        let pre18 = tx_timeout_ms(184, 125_000, 12, 8, 18) as u64;
        let pre8 = tx_timeout_ms(184, 125_000, 12, 8, 8) as u64;
        assert!(pre18 - pre8 >= 327, "preamble delta {}ms", pre18 - pre8);
    }

    #[test]
    fn an_unconfigured_radio_gets_a_bounded_wait() {
        assert_eq!(tx_timeout_ms(184, 0, 12, 8, 18), UNCONFIGURED_WAIT_MS);
        assert_eq!(cad_timeout_ms(0, 12, 8), UNCONFIGURED_WAIT_MS);
    }

    /// CAD listening time is symbols × symbol time, so it scales with SF AND
    /// bandwidth. The table this replaces assumed BW125: 8 CAD symbols at
    /// SF12/BW31.25 are 1.05 s against the table's 800 ms.
    #[test]
    fn cad_timeout_tracks_symbol_time() {
        let narrow = cad_timeout_ms(31_250, 12, 8) as u64;
        assert!(
            narrow >= 1_049 + SOFT_TIMEOUT_SLACK_MS,
            "SF12/BW31.25 CAD window {narrow}ms cannot cover 8 symbols"
        );
        // At the profile the table was built for, the derived value stays at
        // or under the old entry (295 ms of symbols + slack vs 800).
        let sf12_bw125 = cad_timeout_ms(125_000, 12, 8) as u64;
        assert!(
            sf12_bw125 <= 800,
            "SF12/BW125 grew past the old table: {sf12_bw125}ms"
        );
        // Fast SF: symbols are ~1 ms each, the slack dominates, and a wedged
        // radio is still detected in well under a second.
        let sf7 = cad_timeout_ms(125_000, 7, 4) as u64;
        assert!(sf7 < 1_000, "SF7 CAD window {sf7}ms");
    }

    /// The extension tracks the settings rather than being a constant: the
    /// fast end of the rig is tens of milliseconds where the slow end is
    /// seconds. `max(1)` is a floor against a rounded-to-zero airtime turning
    /// the extension into an immediate re-read; no configuration in the
    /// corpus reaches it, which is why it is asserted and not relied on.
    #[test]
    fn the_extension_tracks_the_settings() {
        // 94 is the derived programmed preamble at SF5 — 98.25 on-air symbols
        // (6.3 ms at BW500), on top of the ~34 ms max-frame payload.
        let fast = rx_extend_ms(IRQ_PREAMBLE_DETECTED, 500_000, 5, 5, 94).expect("guard must fire");
        let (bw, sf, cr) = SLOW;
        let slow = rx_extend_ms(IRQ_PREAMBLE_DETECTED, bw, sf, cr, SLOW_PREAMBLE)
            .expect("guard must fire");
        assert_eq!(fast, 41);
        assert!(slow > 100 * fast, "slow={slow}ms fast={fast}ms");
        assert!(fast >= 1);
    }

    /// The defect instance (#150): the SX1262 interleaved bandwidth codes
    /// 0x08/0x09/0x0A are 10.42/20.83/41.67 kHz — the narrowest bandwidths
    /// in the table, where an SF11/12 symbol is 49-393 ms, far above the
    /// 16 ms LDRO threshold. The old predicate compared the register code
    /// (`bw <= 0x04`) as if the code space were monotonic in bandwidth, so
    /// exactly these configurations ran with LDRO off while any conforming
    /// peer (the reference firmware derives LDRO from symbol duration) has
    /// it on — and an LDRO mismatch between ends kills decoding entirely.
    #[test]
    fn ldro_covers_the_interleaved_bandwidth_codes() {
        for bw_hz in [10_420u32, 20_830, 41_670] {
            for sf in [11u8, 12] {
                assert!(
                    ldro_enabled(bw_hz, sf),
                    "SF{sf}/BW{bw_hz}: symbol far above 16 ms, LDRO must be on"
                );
            }
        }
    }

    /// The regime the old predicate got right must stay put: SF12/BW125 is
    /// LDRO-on, SF10/BW125 off.
    #[test]
    fn ldro_bw125_regime_is_unchanged() {
        assert!(ldro_enabled(125_000, 12), "SF12/BW125 must stay LDRO-on");
        assert!(!ldro_enabled(125_000, 10), "SF10/BW125 must stay LDRO-off");
    }

    /// The full decision matches the reference firmware on every bandwidth
    /// code in the SX1262 table (`long((1<<sf)/(bw/1000)) > 16`,
    /// RNode_Firmware sx126x.cpp:725-729). The interesting members:
    /// SF11/BW125 and SF12/BW250 sit at exactly 16 integer-ms and the
    /// reference runs them LDRO-OFF (matching every deployed RNode peer),
    /// while narrow bandwidths need LDRO well below SF11 — down to SF7 at
    /// 7.81 kHz.
    #[test]
    fn ldro_matches_the_reference_decision_on_the_whole_code_table() {
        // (bw_hz, reference getSignalBandwidth() value)
        const BW_TABLE: [(u32, u32); 10] = [
            (7_810, 7_800),
            (10_420, 10_400),
            (15_630, 15_600),
            (20_830, 20_800),
            (31_250, 31_250),
            (41_670, 41_700),
            (62_500, 62_500),
            (125_000, 125_000),
            (250_000, 250_000),
            (500_000, 500_000),
        ];
        for (bw_hz, ref_bw) in BW_TABLE {
            for sf in 5u8..=12 {
                let reference = (1u64 << sf) / (ref_bw as u64 / 1000) > 16;
                assert_eq!(
                    ldro_enabled(bw_hz, sf),
                    reference,
                    "SF{sf}/BW{bw_hz} disagrees with the reference firmware"
                );
            }
        }
        // The boundary members, stated as facts rather than derived:
        assert!(!ldro_enabled(125_000, 11), "SF11/BW125 is reference-OFF");
        assert!(!ldro_enabled(250_000, 12), "SF12/BW250 is reference-OFF");
        assert!(ldro_enabled(7_810, 7), "SF7/BW7.81k is reference-ON");
    }

    /// Unconfigured (bw 0) and sub-kHz values report no LDRO instead of
    /// dividing by zero.
    #[test]
    fn ldro_unconfigured_is_off() {
        assert!(!ldro_enabled(0, 12));
        assert!(!ldro_enabled(999, 12));
    }

    /// The extension charges the programmed preamble of the frame still on
    /// the air: at the SLOW profile the derived 18 symbols are 10 symbols
    /// (164 ms) more than the modem-default 8 the old formula assumed.
    #[test]
    fn the_extension_charges_the_programmed_preamble() {
        let (bw, sf, cr) = SLOW;
        let pre18 = rx_extend_ms(IRQ_PREAMBLE_DETECTED, bw, sf, cr, SLOW_PREAMBLE)
            .expect("guard must fire");
        let pre8 = rx_extend_ms(IRQ_PREAMBLE_DETECTED, bw, sf, cr, 8).expect("guard must fire");
        assert!(pre18 - pre8 >= 163, "preamble delta {}ms", pre18 - pre8);
    }

    /// Every power the part accepts is planned as itself, negatives included.
    ///
    /// The table rather than three examples: the defect this replaces was a
    /// four-entry match, and a spot check of three values is how a
    /// four-entry match survives a rewrite. Sub-14 dBm is the half that used
    /// to be unreachable at all — a configured 2 transmitted at roughly 14.
    #[test]
    fn every_power_in_range_is_planned_as_itself() {
        for requested in TX_POWER_MIN_DBM..=TX_POWER_MAX_DBM {
            let plan = plan_tx_power(requested);
            assert_eq!(plan.programmed_dbm, requested, "{requested} dBm");
            assert!(!plan.clamped, "{requested} dBm reported as clamped");
        }
    }

    /// Out of range in either direction is clamped, and says it was.
    ///
    /// Never refused: a value the chip cannot do is brought into range and
    /// announced. `37` is `rnode::MAX_TX_POWER`, the widest thing the wire's
    /// field can carry, so it is the value a host can actually send.
    #[test]
    fn a_power_out_of_range_is_clamped_and_says_so() {
        let high = plan_tx_power(37);
        assert_eq!(high.programmed_dbm, TX_POWER_MAX_DBM);
        assert!(high.clamped);

        let low = plan_tx_power(-40);
        assert_eq!(low.programmed_dbm, TX_POWER_MIN_DBM);
        assert!(low.clamped);

        assert_eq!(plan_tx_power(i8::MIN).programmed_dbm, TX_POWER_MIN_DBM);
        assert_eq!(plan_tx_power(i8::MAX).programmed_dbm, TX_POWER_MAX_DBM);
    }

    /// The clamp is the reference's, bound to the reference's own numbers.
    ///
    /// `sx126x.cpp:728-729` — `if (level > 22) { level = 22; } else if
    /// (level < -9) { level = -9; }`.
    #[test]
    fn the_clamp_matches_the_reference() {
        assert_eq!(TX_POWER_MAX_DBM, 22);
        assert_eq!(TX_POWER_MIN_DBM, -9);
    }

    /// A real `GetPacketStatus` response off the rig: `rssi=-21 snr=10` is a
    /// line the T114 logged on 2026-08-19, and the two raw bytes that produce
    /// it are what the driver used to decode inline.
    #[test]
    fn packet_status_decodes_a_reception_from_the_rig() {
        // RssiPkt 42 = -21 dBm, SnrPkt 40 = +10.0 dB.
        assert_eq!(packet_status_dbm([42, 40, 42]), (-21, 10));
    }

    /// A frame at the sensitivity limit — the population the CRC-error line
    /// exists to characterise — is below the noise floor, so SnrPkt is
    /// negative and must stay negative through the decode. An unsigned read
    /// of that byte would report +54 dB.
    #[test]
    fn a_negative_snr_stays_negative() {
        // SnrPkt 0xE0 = raw -32 = -8.0 dB; the +2 rounding reports -7.
        assert_eq!(packet_status_dbm([250, 0xE0, 250]), (-125, -7));
        // The extremes of the byte, so the sign handling is pinned at both
        // ends: raw -128 is -32.0 dB and raw 127 is +31.75 dB.
        assert_eq!(packet_status_dbm([0, 0x80, 0]).1, -31);
        assert_eq!(packet_status_dbm([0, 0x7F, 0]).1, 32);
    }

    /// RSSI is reported in -0.5 dB steps, so the decode is monotonically
    /// non-increasing in the raw byte and never positive. `SignalRssiPkt`
    /// (byte 2) must not leak into either result.
    #[test]
    fn rssi_is_never_positive_and_ignores_the_third_byte() {
        let mut prev = i16::MAX;
        for raw in 0u8..=255 {
            let (rssi, _) = packet_status_dbm([raw, 0, 0]);
            assert!(rssi <= 0, "raw {raw} decoded to a positive RSSI {rssi}");
            assert!(rssi <= prev, "raw {raw} is not monotonic");
            prev = rssi;
        }
        assert_eq!(
            packet_status_dbm([42, 40, 0]),
            packet_status_dbm([42, 40, 255])
        );
    }

    /// The two RX-gain register values are the ones the reference firmware and
    /// the local datasheet transcription name, and they are distinct.
    ///
    /// A register address is the one thing in this module that cannot be
    /// derived or sanity-checked at runtime: write 0x08AD instead of 0x08AC
    /// and the chip accepts it silently. The value of pinning it here is that
    /// the transposition has to survive a diff of this file, where the
    /// citation sits beside it.
    #[test]
    fn rx_gain_constants_match_the_reference() {
        // reference/RNode_Firmware/sx126x.cpp:63 — REG_LNA_6X 0x08AC.
        assert_eq!(REG_RX_GAIN, 0x08AC);
        // reference/RNode_Firmware/sx126x.cpp:361 — writeRegister(REG_LNA_6X, 0x96).
        assert_eq!(RX_GAIN_BOOSTED, 0x96);
        // docs/src/sx1262-datasheet-reference.md key register table:
        // "0x94=power saving (default), 0x96=boosted gain".
        assert_eq!(RX_GAIN_POWER_SAVING, 0x94);
        assert_ne!(RX_GAIN_BOOSTED, RX_GAIN_POWER_SAVING);
    }

    /// Standard IQ sets bit 2, inverted IQ clears it, and neither disturbs any
    /// other bit of the register.
    ///
    /// The whole-byte sweep is the point: the erratum is applied to a register
    /// whose remaining seven bits are undocumented in the only transcription
    /// we hold, so "leaves everything else alone" is the property that keeps
    /// the correction from being a blind write.
    #[test]
    fn iq_polarity_touches_only_bit_2() {
        for raw in 0u8..=255 {
            let standard = iq_polarity_value(raw, false);
            let inverted = iq_polarity_value(raw, true);
            assert_eq!(standard & IQ_POLARITY_BIT, IQ_POLARITY_BIT, "raw {raw}");
            assert_eq!(inverted & IQ_POLARITY_BIT, 0, "raw {raw}");
            assert_eq!(standard & !IQ_POLARITY_BIT, raw & !IQ_POLARITY_BIT);
            assert_eq!(inverted & !IQ_POLARITY_BIT, raw & !IQ_POLARITY_BIT);
        }
    }

    /// Applying the correction twice is applying it once.
    ///
    /// The driver runs this after every `SetPacketParams`, which is once per
    /// transmitted frame, so a correction that drifted on repetition would
    /// drift in the field and nowhere else.
    #[test]
    fn iq_polarity_is_idempotent() {
        for raw in 0u8..=255 {
            for inverted in [false, true] {
                let once = iq_polarity_value(raw, inverted);
                assert_eq!(iq_polarity_value(once, inverted), once, "raw {raw}");
            }
        }
    }

    /// The standard-IQ value is what the reference computes for the packet
    /// params this driver actually sends.
    ///
    /// `set_packet_params` hardcodes byte 5 (invertIQ) to 0x00, and the
    /// reference branches on exactly that byte
    /// (`sx126x.cpp:292`: `if (buf[5] == 0x00) writeRegister(0x0736, iqreg | 0x04)`).
    /// This ties our `inverted_iq: false` to that branch so the two cannot be
    /// read apart.
    #[test]
    fn standard_iq_matches_the_reference_branch() {
        for raw in 0u8..=255 {
            assert_eq!(iq_polarity_value(raw, false), raw | 0x04);
            assert_eq!(iq_polarity_value(raw, true), raw & !0x04);
        }
    }

    /// The `TxModulation` address is the one errata 15.1 and the reference
    /// firmware name. Read-only in this tree, which is exactly why the address
    /// has to be pinned: a transposed read is a wrong number in a report
    /// rather than a chip that misbehaves, and a wrong number in a report is
    /// believed.
    #[test]
    fn tx_modulation_address_matches_the_reference() {
        // docs/src/sx1262-datasheet-reference.md §15.1 and its key register
        // table: "0x0889 | TxModulation | BW500 workaround (bit 2)".
        assert_eq!(REG_TX_MODULATION, 0x0889);
        // reference/RNode_Firmware/sx126x.cpp:797 — readRegister(0x0889).
        assert_ne!(REG_TX_MODULATION, REG_RX_GAIN);
        assert_ne!(REG_TX_MODULATION, REG_IQ_POLARITY);
    }
}

/// The read-write-read brackets, driven against a register file.
///
/// This is the module the mutation control runs in. Delete the `write_reg`
/// call inside [`probe_rx_init`] and
/// `the_boot_probe_brackets_the_rx_gain_write` fails printing the line with
/// `rxgain_after` equal to `rxgain_before` — which is the whole point of
/// reporting both. Nothing else in this tree can show that without a board.
#[cfg(test)]
mod probe_tests {
    extern crate std;

    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::format;
    use alloc::vec::Vec;

    /// One access to the chip, in the order it happened.
    #[derive(Debug, PartialEq, Eq)]
    enum Op {
        Read(u16),
        Write(u16, u8),
        /// An opcode and its argument bytes, exactly as they went out.
        Cmd(u8, Vec<u8>),
    }

    /// A register file that answers reads and remembers writes.
    ///
    /// `RX_GAIN` starts at [`RX_GAIN_POWER_SAVING`] because that is the chip's
    /// documented reset default — the state the whole batch is about. The
    /// other two start at values that are this fixture's and nothing else's
    /// (0xA1, 0x5A): a test that asserted a plausible-looking datasheet value
    /// here would be inventing one, and the real values are what the board
    /// prints. 0xA1 has bit 2 clear and six other bits set, so a correction
    /// that touched anything but bit 2 shows up as a different byte.
    struct FakeChip {
        regs: BTreeMap<u16, u8>,
        ops: Vec<Op>,
    }

    impl FakeChip {
        fn new() -> Self {
            let mut regs = BTreeMap::new();
            regs.insert(REG_RX_GAIN, RX_GAIN_POWER_SAVING);
            regs.insert(REG_IQ_POLARITY, 0xA1); // bit 2 clear
            regs.insert(REG_TX_MODULATION, 0x5A);
            Self {
                regs,
                ops: Vec::new(),
            }
        }

        fn with(mut self, addr: u16, value: u8) -> Self {
            self.regs.insert(addr, value);
            self
        }

        fn reads(&self) -> usize {
            self.ops
                .iter()
                .filter(|op| matches!(op, Op::Read(_)))
                .count()
        }
    }

    impl RegisterBus for FakeChip {
        type Error = ();

        async fn read_reg(&mut self, addr: u16) -> Result<u8, ()> {
            self.ops.push(Op::Read(addr));
            Ok(self.regs.get(&addr).copied().unwrap_or(0))
        }

        async fn write_reg(&mut self, addr: u16, value: u8) -> Result<(), ()> {
            self.ops.push(Op::Write(addr, value));
            self.regs.insert(addr, value);
            Ok(())
        }
    }

    impl CommandBus for FakeChip {
        async fn write_cmd(&mut self, opcode: u8, args: &[u8]) -> Result<(), ()> {
            self.ops.push(Op::Cmd(opcode, args.to_vec()));
            // `SetPaConfig` resets REG_OCP to the part's default. Modelled so
            // an OCP written on the wrong side of it is visible here rather
            // than only on a board.
            if opcode == OP_SET_PA_CONFIG {
                self.regs.insert(REG_OCP, OCP_HIGH_POWER);
            }
            Ok(())
        }
    }

    /// Run a probe to completion.
    ///
    /// `FakeChip`'s futures never pend — every access resolves in the poll
    /// that starts it — so one poll is the whole execution and a `Pending`
    /// here would mean the probe grew a wait this harness cannot see.
    fn run<F: core::future::Future>(fut: F) -> F::Output {
        let mut fut = core::pin::pin!(fut);
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        match fut.as_mut().poll(&mut cx) {
            core::task::Poll::Ready(v) => v,
            core::task::Poll::Pending => panic!("the fake register file never pends"),
        }
    }

    /// The boot line reports the register on both sides of our write.
    ///
    /// **This is the positive control.** Remove the `write_reg` from
    /// `probe_rx_init` and this fails with `rxgain_after=0x94` — equal to
    /// `rxgain_before`, which is what "the write did not happen" looks like on
    /// the wire. A probe that reported a constant for either half would pass
    /// this test only by accident and fail
    /// `the_before_value_follows_the_chip_not_the_code` below.
    #[test]
    fn the_boot_probe_brackets_the_rx_gain_write() {
        let mut chip = FakeChip::new();
        let probe = run(probe_rx_init(&mut chip)).expect("the fake chip never errors");
        assert_eq!(
            format!("{probe}"),
            "rxgain_before=0x94 rxgain_after=0x96 txmod=0x5A"
        );
        // The bracket, in order: the before-read must precede the write and
        // the after-read must follow it, or the two values are not a bracket.
        assert_eq!(
            chip.ops,
            [
                Op::Read(REG_RX_GAIN),
                Op::Write(REG_RX_GAIN, RX_GAIN_BOOSTED),
                Op::Read(REG_RX_GAIN),
                Op::Read(REG_TX_MODULATION),
            ]
        );
    }

    /// `before` is read off the chip, not baked into the probe.
    ///
    /// A board that already holds the boosted value — which is what a warm
    /// start or a second `init_radio` looks like — must print `0x96` for
    /// both. If `rxgain_before` were the constant `RX_GAIN_POWER_SAVING` in
    /// disguise, this is the test that says so.
    #[test]
    fn the_before_value_follows_the_chip_not_the_code() {
        let mut chip = FakeChip::new().with(REG_RX_GAIN, RX_GAIN_BOOSTED);
        let probe = run(probe_rx_init(&mut chip)).expect("the fake chip never errors");
        assert_eq!(
            format!("{probe}"),
            "rxgain_before=0x96 rxgain_after=0x96 txmod=0x5A"
        );
    }

    /// The `txmod` field is a read and only a read.
    ///
    /// Errata 15.1 is reserved for the batch that has the attenuator in line;
    /// this probe must not start correcting it by accident.
    #[test]
    fn the_probe_never_writes_tx_modulation() {
        let mut chip = FakeChip::new();
        let _ = run(probe_rx_init(&mut chip)).expect("the fake chip never errors");
        assert!(!chip
            .ops
            .iter()
            .any(|op| matches!(op, Op::Write(REG_TX_MODULATION, _))));
        assert_eq!(chip.regs.get(&REG_TX_MODULATION).copied(), Some(0x5A));
        let mut chip = FakeChip::new();
        let _ = run(apply_iq_polarity(&mut chip, false, true)).expect("the fake chip never errors");
        assert!(!chip
            .ops
            .iter()
            .any(|op| matches!(op, Op::Write(REG_TX_MODULATION, _))));
    }

    /// The IQ line reports the register on both sides of the correction.
    ///
    /// Same control as the RX-gain one: drop the `write_reg` from
    /// `apply_iq_polarity` and `iq_after` collapses onto `iq_before`.
    #[test]
    fn the_iq_probe_brackets_the_correction() {
        let mut chip = FakeChip::new();
        let probe = run(apply_iq_polarity(&mut chip, false, true))
            .expect("the fake chip never errors")
            .expect("probe requested");
        // 0xA1 | 0x04 == 0xA5: bit 2 set, the other seven untouched.
        assert_eq!(
            format!("{probe}"),
            "iq_before=0xA1 iq_after=0xA5 txmod=0x5A"
        );
        assert_eq!(
            chip.ops,
            [
                Op::Read(REG_IQ_POLARITY),
                Op::Write(REG_IQ_POLARITY, 0xA5),
                Op::Read(REG_IQ_POLARITY),
                Op::Read(REG_TX_MODULATION),
            ]
        );
    }

    /// The outcome the interop evidence predicts, and what it looks like.
    ///
    /// If the chip already leaves bit 2 set in the standard-IQ case — the
    /// inference `35fdd87` rested on — the two values are equal and the
    /// correction is confirmed a no-op. That reading is a *result*, not a
    /// broken indicator, and this test is what tells the two apart: here the
    /// equality is expected, in the mutation control it is the failure.
    #[test]
    fn an_already_correct_register_reports_before_equal_to_after() {
        let mut chip = FakeChip::new().with(REG_IQ_POLARITY, 0xA5);
        let probe = run(apply_iq_polarity(&mut chip, false, true))
            .expect("the fake chip never errors")
            .expect("probe requested");
        assert_eq!(
            format!("{probe}"),
            "iq_before=0xA5 iq_after=0xA5 txmod=0x5A"
        );
    }

    /// Off the per-frame path: with `probe` false the correction costs exactly
    /// what it cost before this batch — one read, one write.
    ///
    /// `SetPacketParams` runs once per transmitted frame at 4 MHz SPI, so this
    /// is the assertion that keeps a boot diagnostic from becoming a per-frame
    /// tax.
    #[test]
    fn the_unprobed_call_costs_one_read_and_one_write() {
        let mut chip = FakeChip::new();
        let probe = run(apply_iq_polarity(&mut chip, false, false)).expect("never errors");
        assert!(probe.is_none());
        assert_eq!(
            chip.ops,
            [Op::Read(REG_IQ_POLARITY), Op::Write(REG_IQ_POLARITY, 0xA5),]
        );
        assert_eq!(chip.reads(), 1);
    }

    /// Inverted IQ clears the bit and the probe reports that too.
    ///
    /// The driver only ever asks for standard IQ, so this is the arm nothing
    /// on the board exercises — which is precisely why it needs a host test.
    #[test]
    fn inverted_iq_is_reported_the_same_way() {
        let mut chip = FakeChip::new().with(REG_IQ_POLARITY, 0xA5);
        let probe = run(apply_iq_polarity(&mut chip, true, true))
            .expect("the fake chip never errors")
            .expect("probe requested");
        assert_eq!(
            format!("{probe}"),
            "iq_before=0xA5 iq_after=0xA1 txmod=0x5A"
        );
    }

    // -----------------------------------------------------------------------
    // Transmit power (Codeberg #349)
    // -----------------------------------------------------------------------

    /// Every power the part accepts reaches `SetTxParams` unchanged, on the
    /// wire, as the byte the chip reads.
    ///
    /// The whole table, and against the recorded ops rather than against
    /// `plan_tx_power`: the defect was not in the decision, it was that the
    /// decision never reached the bus — `SetTxParams` was sent a literal 22
    /// for every configured value. A test that only checked the plan would
    /// have passed on the broken driver.
    #[test]
    fn every_power_in_range_reaches_set_tx_params_unchanged() {
        for requested in TX_POWER_MIN_DBM..=TX_POWER_MAX_DBM {
            let mut chip = FakeChip::new();
            let plan = run(program_tx_power(&mut chip, requested)).expect("fake never errors");
            assert_eq!(plan.programmed_dbm, requested, "{requested} dBm");
            assert!(!plan.clamped, "{requested} dBm reported as clamped");
            let tx_params = chip
                .ops
                .iter()
                .find_map(|op| match op {
                    Op::Cmd(OP_SET_TX_PARAMS, args) => Some(args.clone()),
                    _ => None,
                })
                .expect("SetTxParams was never issued");
            assert_eq!(
                tx_params,
                Vec::from([requested as u8, PA_RAMP_200US]),
                "{requested} dBm went out as {tx_params:?}"
            );
        }
    }

    /// Out of range in either direction is clamped on the bus too, and the
    /// clamp is reported back to the caller that has to announce it.
    #[test]
    fn a_power_out_of_range_is_clamped_on_the_wire_and_reported() {
        for (requested, expected) in [(37i8, 22i8), (23, 22), (-10, -9), (i8::MIN, -9)] {
            let mut chip = FakeChip::new();
            let plan = run(program_tx_power(&mut chip, requested)).expect("fake never errors");
            assert_eq!(plan.programmed_dbm, expected, "{requested} dBm");
            assert!(plan.clamped, "{requested} dBm did not report its clamp");
            assert_eq!(plan.requested_dbm, requested);
            assert!(
                chip.ops.contains(&Op::Cmd(
                    OP_SET_TX_PARAMS,
                    Vec::from([expected as u8, PA_RAMP_200US])
                )),
                "{requested} dBm: ops were {:?}",
                chip.ops
            );
        }
    }

    /// **The control.** The PA-config bytes are the reference's, for every
    /// power, and they are the same block every time.
    ///
    /// Asserted as exact bytes rather than as "some PA config was sent":
    /// these four bytes are what decides whether the number in `SetTxParams`
    /// means what the datasheet says it means, and a future edit that
    /// reintroduced a per-power table would otherwise change what we radiate
    /// without changing a single test.
    #[test]
    fn the_pa_config_is_the_references_for_every_power() {
        // reference/RNode_Firmware/sx126x.cpp:722-725.
        assert_eq!(PA_CONFIG_HIGH_POWER, [0x04, 0x07, 0x00, 0x01]);
        for requested in -20i8..=30 {
            let mut chip = FakeChip::new();
            run(program_tx_power(&mut chip, requested)).expect("fake never errors");
            assert!(
                chip.ops.contains(&Op::Cmd(
                    OP_SET_PA_CONFIG,
                    Vec::from([0x04u8, 0x07, 0x00, 0x01])
                )),
                "{requested} dBm sent a different PA config: {:?}",
                chip.ops
            );
        }
    }

    /// The three operations happen in the reference's order, and the OCP
    /// survives.
    ///
    /// The ordering is not cosmetic: `SetPaConfig` resets `REG_OCP`, which
    /// the fake models, so an OCP written first would be back at the default
    /// by the time the PA is keyed. Reading the register out at the end is
    /// what turns "we wrote it" into "it is in force".
    #[test]
    fn the_ocp_is_written_after_the_pa_config_and_survives_it() {
        let mut chip = FakeChip::new();
        run(program_tx_power(&mut chip, 14)).expect("fake never errors");
        assert_eq!(
            chip.ops,
            Vec::from([
                Op::Cmd(OP_SET_PA_CONFIG, Vec::from([0x04u8, 0x07, 0x00, 0x01])),
                Op::Write(REG_OCP, OCP_HIGH_POWER),
                Op::Cmd(OP_SET_TX_PARAMS, Vec::from([14u8, PA_RAMP_200US])),
            ])
        );
        assert_eq!(chip.regs.get(&REG_OCP).copied(), Some(OCP_HIGH_POWER));
    }

    /// The OCP we write is at or above the reference's, never below.
    ///
    /// The reference tunes it down to 0x28 (100 mA); the datasheet's own
    /// typical current at +22 dBm is 118 mA, so that value can only limit the
    /// top of the range. This is the assertion that keeps a later "match the
    /// reference exactly" edit from quietly capping our output.
    #[test]
    fn the_ocp_is_not_below_the_references() {
        // 56 steps x 2.5 mA = 140 mA
        assert_eq!(OCP_HIGH_POWER, 0x38);
        // reference/RNode_Firmware/Boards.h:932. Both operands are compile-time
        // constants, so the comparison belongs in a const block:
        // clippy::assertions_on_constants rejects the runtime form, and the
        // const one fails the BUILD rather than a test run — which is what an
        // invariant over a constant should do.
        const { assert!(OCP_HIGH_POWER > 0x28) };
    }
}

/// The receiver-arming report, driven against a fake clock.
///
/// The clock is the point: `dark_ms` is a difference between two instants the
/// board takes at opposite ends of a window, and the only way to assert it is
/// the right difference is to supply both instants by hand. A board test can
/// see the number is plausible; only this can see it is correct.
///
/// This module is where the mutation control runs. Change
/// [`RxArmClock::arm`] to record its own `now_ms` as the window end — i.e.
/// measure the gap from the window START — and
/// `dark_ms_is_measured_from_the_window_end` fails.
#[cfg(test)]
mod rx_arm_tests {
    extern crate std;

    use super::*;
    use alloc::format;

    /// Every site's tag, spelled once so a rename shows up as a diff here.
    ///
    /// The tags go into captures and into the greps read against them; they
    /// are an interface, not an internal name.
    #[test]
    fn every_site_has_its_stable_tag() {
        assert_eq!(RxSite::Idle.tag(), "idle");
        assert_eq!(RxSite::Ack.tag(), "ack");
        assert_eq!(RxSite::Csma.tag(), "csma");
        assert_eq!(RxSite::Jitter.tag(), "jitter");
        assert_eq!(RxSite::Hold.tag(), "hold");
        assert_eq!(RxSite::Yield.tag(), "yield");
        // Distinct, or two windows are indistinguishable in a capture.
        let tags = [
            RxSite::Idle.tag(),
            RxSite::Ack.tag(),
            RxSite::Csma.tag(),
            RxSite::Hold.tag(),
            RxSite::Yield.tag(),
        ];
        for (i, a) in tags.iter().enumerate() {
            for b in &tags[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// Every teardown caller's tag, spelled once, and all of them distinct.
    ///
    /// `[SX_RX_TEARDOWN] site=` is read as a rate per caller — how many of the
    /// windows this path takes down were holding a frame — so two callers
    /// sharing a tag do not merely read badly, they sum two populations into
    /// one number.
    #[test]
    fn every_teardown_caller_has_its_stable_tag() {
        assert_eq!(RxTeardownBy::Arm.tag(), "arm");
        assert_eq!(RxTeardownBy::Select.tag(), "select");
        assert_eq!(RxTeardownBy::Cad.tag(), "cad");
        assert_eq!(RxTeardownBy::Tx.tag(), "tx");
        assert_eq!(RxTeardownBy::RxWait.tag(), "rxwait");
        assert_eq!(RxTeardownBy::Config.tag(), "config");
        let tags = [
            RxTeardownBy::Arm.tag(),
            RxTeardownBy::Select.tag(),
            RxTeardownBy::Cad.tag(),
            RxTeardownBy::Tx.tag(),
            RxTeardownBy::RxWait.tag(),
            RxTeardownBy::Config.tag(),
        ];
        for (i, a) in tags.iter().enumerate() {
            for b in &tags[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// The whole body, field by field, in the order the line prints them.
    #[test]
    fn the_line_body_has_the_shape_the_host_greps() {
        let mut clock = RxArmClock::new();
        clock.window_ended(1_000);
        let arm = clock.arm(RxSite::Ack, 2_590, 1_007);
        assert_eq!(format!("{arm}"), "site=ack timeout_ms=2590 dark_ms=7");
    }

    /// Single mode renders its timeout as the zero the driver passes.
    ///
    /// Unambiguous because no bounded caller can produce it: both bounded
    /// window sizes clamp to at least 1 ms before they reach `receive()`.
    #[test]
    fn single_mode_renders_a_zero_timeout() {
        let mut clock = RxArmClock::new();
        clock.window_ended(500);
        let arm = clock.arm(RxSite::Idle, 0, 503);
        assert_eq!(format!("{arm}"), "site=idle timeout_ms=0 dark_ms=3");
    }

    /// The boot case says it has no previous window, rather than printing a
    /// number that reads like one.
    #[test]
    fn the_first_arm_of_a_boot_reports_no_previous_window() {
        let mut clock = RxArmClock::new();
        let arm = clock.arm(RxSite::Idle, 0, 4_211);
        assert_eq!(arm.dark_ms, None);
        assert_eq!(format!("{arm}"), "site=idle timeout_ms=0 dark_ms=first");
        // Not a zero and not the uptime: both would be read as measurements.
        assert!(!format!("{arm}").contains("dark_ms=0"));
        assert!(!format!("{arm}").contains("dark_ms=4211"));
    }

    /// **The control.** `dark_ms` spans the previous window's END to this
    /// arming, and nothing else.
    ///
    /// The three instants are deliberately far apart and unequal, so every
    /// wrong pairing produces a different number:
    ///
    /// | measured from            | would print |
    /// |--------------------------|-------------|
    /// | window end (correct)     | 7           |
    /// | window start             | 507         |
    /// | previous arming's `now`  | 507         |
    ///
    /// Make `arm` store its own `now_ms` as the window end (measuring from
    /// the window START) and this asserts 507 against the expected 7.
    #[test]
    fn dark_ms_is_measured_from_the_window_end() {
        let mut clock = RxArmClock::new();
        // A window armed at 1000, listening for 500 ms.
        let first = clock.arm(RxSite::Idle, 0, 1_000);
        assert_eq!(first.dark_ms, None);
        clock.window_ended(1_500);
        // Re-armed 7 ms later.
        let second = clock.arm(RxSite::Csma, 400, 1_507);
        assert_eq!(second.dark_ms, Some(7), "dark_ms must span 1500 -> 1507");
        assert_ne!(
            second.dark_ms,
            Some(507),
            "dark_ms was measured from the window START (1000), not its end"
        );
    }

    /// A long dark span is reported as long, not clamped or wrapped.
    ///
    /// The gaps worth finding are the big ones; a helper that quietly
    /// saturated at some small width would hide exactly them.
    #[test]
    fn a_long_gap_is_reported_in_full() {
        let mut clock = RxArmClock::new();
        clock.window_ended(1_000);
        let arm = clock.arm(RxSite::Hold, 10_000, 1_000 + 3_600_000);
        assert_eq!(
            format!("{arm}"),
            "site=hold timeout_ms=10000 dark_ms=3600000"
        );
    }

    /// Back-to-back windows with no code in between read as zero dark.
    ///
    /// Zero is a legitimate measurement here — it is only illegitimate as a
    /// stand-in for "unknown", which is why that case has its own word.
    #[test]
    fn abutting_windows_report_zero_dark() {
        let mut clock = RxArmClock::new();
        clock.window_ended(2_000);
        let arm = clock.arm(RxSite::Yield, 5_180, 2_000);
        assert_eq!(format!("{arm}"), "site=yield timeout_ms=5180 dark_ms=0");
    }

    /// A window that never records an end leaves the next arming with
    /// nothing to measure from, and it says so.
    ///
    /// Reachable: `receive()` propagates an SPI error out of the IRQ readout.
    /// Measuring from the window before it would over-state the gap by a
    /// whole window and look like a finding.
    #[test]
    fn a_missing_window_end_is_not_measured_from_a_stale_one() {
        let mut clock = RxArmClock::new();
        clock.window_ended(1_000);
        assert_eq!(clock.arm(RxSite::Idle, 0, 1_010).dark_ms, Some(10));
        // Second window errors out before window_ended.
        let after_error = clock.arm(RxSite::Idle, 0, 9_999);
        assert_eq!(after_error.dark_ms, None);
        assert!(format!("{after_error}").ends_with("dark_ms=first"));
    }

    /// The extension supersedes the first end: the radio was still listening
    /// through it, so the gap starts where the extended wait finished.
    #[test]
    fn the_rx_extension_moves_the_window_end_forward() {
        let mut clock = RxArmClock::new();
        clock.window_ended(1_000); // software wait expired...
        clock.window_ended(3_700); // ...but the frame was still arriving.
        assert_eq!(clock.arm(RxSite::Idle, 0, 3_705).dark_ms, Some(5));
    }

    /// A clock that went backwards yields zero rather than an underflow.
    ///
    /// `embassy_time::Instant` is monotonic so this should be unreachable;
    /// it is asserted because the alternative in release mode is a wrapped
    /// u64 printed as a 19-digit gap.
    #[test]
    fn a_backwards_clock_does_not_wrap() {
        let mut clock = RxArmClock::new();
        clock.window_ended(5_000);
        assert_eq!(clock.arm(RxSite::Idle, 0, 4_000).dark_ms, Some(0));
    }
}
