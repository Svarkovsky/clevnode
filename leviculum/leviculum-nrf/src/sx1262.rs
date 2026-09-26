//! Minimal SX1262 LoRa radio driver for T114.
//!
//! Talks to the SX1262 via SPI using Embassy's async SPI infrastructure.
//! Every function follows the Semtech reference driver (sx126x.c).

use embassy_nrf::gpio::{Input, Output};
use embassy_time::{with_timeout, Duration, Timer};
use embedded_hal_async::spi::{Operation, SpiDevice as SpiDeviceTrait};

/// SX1262 opcodes (from sx126x.h / datasheet §11)
mod opcode {
    pub const GET_STATUS: u8 = 0xC0;
    pub const GET_IRQ_STATUS: u8 = 0x12;
    pub const CLEAR_IRQ_STATUS: u8 = 0x02;
    pub const SET_DIO_IRQ_PARAMS: u8 = 0x08;
    pub const SET_STANDBY: u8 = 0x80;
    pub const SET_TX: u8 = 0x83;
    pub const SET_REGULATOR_MODE: u8 = 0x96;
    pub const SET_DIO3_AS_TCXO_CTRL: u8 = 0x97;
    pub const CLEAR_DEVICE_ERRORS: u8 = 0x07;
    pub const CALIBRATE: u8 = 0x89;
    pub const CALIBRATE_IMAGE: u8 = 0x98;
    pub const SET_DIO2_AS_RF_SWITCH: u8 = 0x9D;
    pub const SET_PACKET_TYPE: u8 = 0x8A;
    pub const SET_RF_FREQUENCY: u8 = 0x86;
    // `SetPaConfig` and `SetTxParams` are not here: they belong to a sequence
    // whose ordering has to be host-testable, so they live beside it in
    // `leviculum_core::sx126x` (`OP_SET_PA_CONFIG`, `OP_SET_TX_PARAMS`). A
    // second copy here would be a second thing to keep in step.
    pub const SET_BUFFER_BASE_ADDRESS: u8 = 0x8F;
    pub const SET_MODULATION_PARAMS: u8 = 0x8B;
    pub const SET_PACKET_PARAMS: u8 = 0x8C;
    pub const WRITE_REGISTER: u8 = 0x0D;
    pub const READ_REGISTER: u8 = 0x1D;
    pub const WRITE_BUFFER: u8 = 0x0E;
    pub const READ_BUFFER: u8 = 0x1E;
    pub const SET_RX: u8 = 0x82;
    pub const GET_RX_BUFFER_STATUS: u8 = 0x13;
    pub const GET_PACKET_STATUS: u8 = 0x14;
    pub const SET_CAD_PARAMS: u8 = 0x88;
    pub const SET_CAD: u8 = 0xC5;
    pub const SET_STOP_RX_TIMER_ON_PREAMBLE: u8 = 0x9F;
}

/// SX1262 register addresses this file reaches for directly (datasheet §15,
/// key register table).
///
/// Together with `leviculum_core::sx126x`'s `REG_*` constants this is meant to
/// be the complete set of registers the driver touches; a register written
/// through a literal instead of a name in one of the two places is a register
/// that is not in any audit. `RX_GAIN` was exactly that gap in the other
/// direction — the reference writes it, we did not, and nothing named it.
///
/// `RxGain` (0x08AC), `IqPolarity` (0x0736) and `TxModulation` (0x0889) are
/// deliberately NOT here: they are touched through
/// `leviculum_core::sx126x::{probe_rx_init, apply_iq_polarity}`, where the
/// address, the value and the read-write-read bracket sit beside a host test
/// that can run them.
mod reg {
    pub const LORA_SYNC_WORD: u16 = 0x0740;
    pub const TX_CLAMP_CONFIG: u16 = 0x08D8;
    pub const RTC_CONTROL: u16 = 0x0902;
    pub const EVENT_MASK: u16 = 0x0944;
}

// IRQ bitmasks and the `SetDioIrqParams` argument builders (datasheet §8.5,
// Table 8-4). The masks and the RX-extend decision live in
// `leviculum_core::sx126x`, which is host-testable; this crate is not, and the
// RX-extend guard in `receive()` was dead for its whole life because nothing
// here could be run against a test.
use leviculum_core::sx126x as irq;

/// Received packet status (RSSI and SNR).
pub struct RxStatus {
    pub rssi: i16,
    pub snr: i16,
}

/// What a frame that failed its payload CRC looked like on the air.
///
/// The payload is discarded — it is corrupt by definition — but the reception
/// itself is a measurement, and it is the measurement of the population we
/// cannot otherwise see: a link that loses 60-100 % of its frames (Codeberg
/// #258) is characterised entirely by the frames that fail, and until this
/// existed they logged the word `Crc` and nothing else.
///
/// `len` is the payload length out of the explicit header. It is trustworthy
/// on this path precisely because the header carries its own CRC: a frame
/// whose header failed raises `HeaderErr`, never `RxDone`, so a `RxDone` with
/// `CrcErr` means the header was decoded cleanly and only the payload is bad.
///
/// **No frequency error.** The obvious fourth field is the one #258 wants,
/// and the SX1262 does not have it. Its packet-status readout
/// (`GetPacketStatus`, opcode 0x14, datasheet Table 13-77) returns RssiPkt,
/// SnrPkt and SignalRssiPkt, and there is no FEI command or register beside
/// them — the SX127x's `RegFei` triplet has no counterpart on this part. The
/// reference RNode firmware reaches the same conclusion in the open:
/// `reference/RNode_Firmware/sx127x.cpp:258` computes the frequency error
/// from those registers, while `reference/RNode_Firmware/sx126x.cpp:575` is a
/// stub returning 0.0 with the comment "TODO: Implement this, no idea how to
/// check it on the sx1262". A substitute quantity
/// under the name `fei` would be worse than the gap.
#[derive(Clone, Copy)]
pub struct CrcErrFrame {
    pub len: u8,
    pub rssi: i16,
    pub snr: i16,
}

/// SX1262 error type
pub enum Error {
    Spi,
    Busy,
    Timeout,
    Crc(CrcErrFrame),
    /// [`Sx1262::await_rx`] was called with no window standing.
    ///
    /// Not reachable through [`leviculum_rx_arming::receive_and_hand_up`],
    /// which arms first; it exists so the arming invariant is *asserted* at
    /// the boundary rather than assumed. Waiting on DIO1 for a receiver that
    /// was never armed looks exactly like a quiet channel, which is the one
    /// failure this driver must not be able to report as a timeout.
    NotArmed,
}

/// Hand-written so `Crc` keeps printing as the bare word it always did.
///
/// Several log lines interpolate this error with `{:?}` — `[T114_SX_ERR]
/// error={:?}` is grepped out of rig captures — and a derive would have
/// widened all of them into `Crc(CrcErrFrame { .. })` the day the variant
/// grew a payload. The numbers belong in the one line that is meant to carry
/// them (`[LORA] RX err: Crc len=..`), not in every line that mentions the
/// error.
impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Spi => "Spi",
            Error::Busy => "Busy",
            Error::Timeout => "Timeout",
            Error::Crc(_) => "Crc",
            Error::NotArmed => "NotArmed",
        })
    }
}

/// Parsed SX1262 status byte
#[derive(Clone, Copy)]
pub struct ChipStatus {
    pub raw: u8,
    /// Chip mode: 0x2=STBY_RC, 0x3=STBY_XOSC, 0x4=FS, 0x5=RX, 0x6=TX
    pub mode: u8,
    /// Command status: 0x2=data_avail, 0x3=cmd_timeout, 0x4=cmd_error, 0x5=exec_fail, 0x6=tx_done
    pub cmd: u8,
}

impl ChipStatus {
    fn from_raw(raw: u8) -> Self {
        Self {
            raw,
            mode: (raw >> 4) & 0x07,
            cmd: (raw >> 1) & 0x07,
        }
    }
}

/// Encode a u32 as 3-byte big-endian (24-bit). Used for SX1262 timeout fields.
fn u24_be(val: u32) -> [u8; 3] {
    [
        ((val >> 16) & 0xFF) as u8,
        ((val >> 8) & 0xFF) as u8,
        (val & 0xFF) as u8,
    ]
}

/// Convert an SX1262 bandwidth register code back to bandwidth in Hz
/// (inverse of the table in `RadioConfig::from_wire`, datasheet Table 14-47).
/// Returns 0 for an unknown code so callers can treat it as "not configured".
fn bw_code_to_hz(code: u8) -> u32 {
    match code {
        0x00 => 7_810,
        0x08 => 10_420,
        0x01 => 15_630,
        0x09 => 20_830,
        0x02 => 31_250,
        0x0A => 41_670,
        0x03 => 62_500,
        0x04 => 125_000,
        0x05 => 250_000,
        0x06 => 500_000,
        _ => 0,
    }
}

/// SX1262 driver, generic over the SPI device type.
pub struct Sx1262<SPI> {
    spi: SPI,
    reset: Output<'static>,
    busy: Input<'static>,
    dio1: Input<'static>,
    preamble_len: u16,
    /// SetDIO3AsTcxoCtrl voltage select byte (0x02 = 1.8 V).
    /// Board-specific; stored at construction so `init_radio` is parameter-free.
    tcxo_voltage_reg: u8,
    /// Cached human-readable modulation params from the last `configure_lora`,
    /// used only to size the RX software-wait extension in `receive()`. `bw_hz`
    /// is 0 until the radio is configured (guards the airtime computation).
    rx_ext_sf: u8,
    rx_ext_bw_hz: u32,
    rx_ext_cr_denom: u8,
    /// True once the `[SX_REG_IQ]` read-back has been emitted this boot.
    ///
    /// `set_packet_params` runs once per transmitted frame, and the read-back
    /// it carries costs two extra SPI reads. One boot, one line: this is what
    /// keeps a diagnostic from becoming a per-frame tax.
    iq_probe_done: bool,
    /// Carries the end of one RX window into the next arming so `[SX_RX_ARM]`
    /// can report the gap between them. Lives here rather than in the caller
    /// because both instants are taken inside `receive()`, on either side of
    /// the DIO1 wait; a caller could only bracket the whole call.
    rx_arm: leviculum_core::sx126x::RxArmClock,
    /// Whether a listening window is standing, and what it was programmed
    /// with. The single source of truth for "the chip is armed"; every path
    /// that leaves RX consults it and spends at most one standby.
    rx_state: leviculum_rx_arming::RxArmState<ArmedWindow>,
}

/// What `SetRx` was programmed with, carried from the arming half to the
/// awaiting half.
///
/// `hw_timeout` is kept alongside `timeout_ms` rather than recomputed: the
/// erratum-15.3 workaround is applied on exactly the register value that was
/// written, and a second derivation is a second place for the `0`/`0xFFFFFF`
/// special cases to drift.
#[derive(Clone, Copy)]
struct ArmedWindow {
    timeout_ms: u32,
    hw_timeout: u32,
    /// The site that opened this window. Half of the window's identity for
    /// the adoption decision, and the reason two windows of equal length
    /// opened by different callers are not the same window: adopting across
    /// them would keep listening correctly and lose the loop's decision trail
    /// from the capture, which is the only thing that makes a run readable.
    site: leviculum_core::sx126x::RxSite,
    /// Uptime in milliseconds at the moment the window was recorded, i.e.
    /// immediately before `SetRx` goes out. Stamped there rather than after
    /// the command so a window dropped inside the SPI transaction still has
    /// an honest start; the error is one short transaction and it errs
    /// toward reporting the window as slightly older than it is.
    armed_at_ms: u64,
}

impl<SPI: SpiDeviceTrait> Sx1262<SPI> {
    /// Create a new SX1262 driver. Does NOT initialize the radio, call `reset()` + init_radio() first.
    ///
    /// `tcxo_voltage_reg` is the SetDIO3AsTcxoCtrl voltage select byte (0x02 = 1.8 V
    /// on T114 and RAK4631; see datasheet §13.3.6 Table 13-35).
    pub fn new(
        spi: SPI,
        reset: Output<'static>,
        busy: Input<'static>,
        dio1: Input<'static>,
        tcxo_voltage_reg: u8,
    ) -> Self {
        Self {
            spi,
            reset,
            busy,
            dio1,
            preamble_len: 24,
            tcxo_voltage_reg,
            rx_ext_sf: 0,
            rx_ext_bw_hz: 0,
            rx_ext_cr_denom: 0,
            iq_probe_done: false,
            rx_arm: leviculum_core::sx126x::RxArmClock::new(),
            rx_state: leviculum_rx_arming::RxArmState::new(),
        }
    }

    /// Wait for BUSY pin LOW via GPIOTE interrupt, with timeout.
    async fn wait_busy_ms(&mut self, timeout_ms: u32) -> Result<(), Error> {
        if self.busy.is_low() {
            return Ok(());
        }
        with_timeout(
            Duration::from_millis(timeout_ms as u64),
            self.busy.wait_for_low(),
        )
        .await
        .map_err(|_| Error::Busy)
    }

    /// Wait for BUSY pin to go LOW (radio ready for commands). 100ms timeout.
    pub async fn wait_busy(&mut self) -> Result<(), Error> {
        self.wait_busy_ms(100).await
    }

    /// Reset the SX1262 via the reset pin.
    pub async fn reset(&mut self) {
        self.reset.set_low();
        Timer::after_millis(10).await;
        self.reset.set_high();
        Timer::after_millis(10).await;
    }

    /// Write a command to the SX1262 (opcode + optional params).
    /// Waits for BUSY before sending.
    pub async fn write_command(&mut self, opcode: u8, params: &[u8]) -> Result<(), Error> {
        self.wait_busy().await?;
        let mut buf = [0u8; 16];
        buf[0] = opcode;
        let len = 1 + params.len();
        buf[1..len].copy_from_slice(params);
        self.spi
            .transaction(&mut [Operation::Write(&buf[..len])])
            .await
            .map_err(|_| Error::Spi)
    }

    /// Read from the SX1262 (opcode + NOP byte, then read response).
    /// Returns the status byte.
    pub async fn read_command(&mut self, opcode: u8, response: &mut [u8]) -> Result<u8, Error> {
        self.wait_busy().await?;
        let cmd = [opcode];
        let mut status_buf = [0u8; 1];
        if response.is_empty() {
            self.spi
                .transaction(&mut [Operation::Write(&cmd), Operation::Read(&mut status_buf)])
                .await
                .map_err(|_| Error::Spi)?;
        } else {
            self.spi
                .transaction(&mut [
                    Operation::Write(&cmd),
                    Operation::Read(&mut status_buf),
                    Operation::Read(response),
                ])
                .await
                .map_err(|_| Error::Spi)?;
        }
        Ok(status_buf[0])
    }

    /// Read the SX1262 status register (GetStatus, opcode 0xC0).
    pub async fn get_status(&mut self) -> Result<ChipStatus, Error> {
        self.wait_busy().await?;
        let tx = [opcode::GET_STATUS, 0x00];
        let mut rx = [0u8; 2];
        self.spi
            .transaction(&mut [Operation::Transfer(&mut rx, &tx)])
            .await
            .map_err(|_| Error::Spi)?;
        Ok(ChipStatus::from_raw(rx[1]))
    }

    /// Read the SX1262 IRQ status (GetIrqStatus, opcode 0x12).
    pub async fn get_irq_status(&mut self) -> Result<u16, Error> {
        let mut buf = [0u8; 2];
        let _status = self.read_command(opcode::GET_IRQ_STATUS, &mut buf).await?;
        Ok(((buf[0] as u16) << 8) | buf[1] as u16)
    }

    /// Set standby mode (STBY_RC).
    pub async fn set_standby_rc(&mut self) -> Result<(), Error> {
        self.write_command(opcode::SET_STANDBY, &[0x00]).await
    }

    /// Configure DIO3 as TCXO control. Timeout is 24-bit in units of 15.625µs.
    pub async fn set_dio3_as_tcxo_ctrl(&mut self, voltage: u8, timeout: u32) -> Result<(), Error> {
        let t = u24_be(timeout);
        self.write_command(
            opcode::SET_DIO3_AS_TCXO_CTRL,
            &[voltage & 0x07, t[0], t[1], t[2]],
        )
        .await
    }

    /// Clear device errors (required before TCXO setup after cold boot).
    pub async fn clear_device_errors(&mut self) -> Result<(), Error> {
        self.write_command(opcode::CLEAR_DEVICE_ERRORS, &[0x00, 0x00])
            .await
    }

    /// Run full calibration (mask 0x7F = all blocks). BUSY is high during calibration.
    pub async fn calibrate(&mut self, mask: u8) -> Result<(), Error> {
        self.write_command(opcode::CALIBRATE, &[mask]).await?;
        self.wait_busy_ms(500).await
    }

    /// Calibrate image for a specific frequency band.
    pub async fn calibrate_image(&mut self, freq_hz: u32) -> Result<(), Error> {
        let (f1, f2) = if freq_hz > 900_000_000 {
            (0xE1, 0xE9)
        } else if freq_hz > 850_000_000 {
            (0xD7, 0xDB)
        } else if freq_hz > 770_000_000 {
            (0xC1, 0xC5)
        } else if freq_hz > 460_000_000 {
            (0x75, 0x81)
        } else {
            (0x6B, 0x6F)
        };
        self.write_command(opcode::CALIBRATE_IMAGE, &[f1, f2])
            .await?;
        self.wait_busy_ms(500).await
    }

    /// Full initialization sequence (SX1262 + TCXO + LDO regulator).
    /// Order follows the datasheet init summary (§9.2.1, §13).
    /// Call after reset().
    pub async fn init_radio(&mut self, freq_hz: u32) -> Result<ChipStatus, Error> {
        self.set_standby_rc().await?;
        self.write_command(opcode::SET_REGULATOR_MODE, &[0x00])
            .await?; // LDO: RNode firmware sx126x.cpp begin() never calls
                     // OP_REGULATOR_MODE, leaving the SX1262 at its LDO default.
        self.write_command(opcode::SET_DIO2_AS_RF_SWITCH, &[0x01])
            .await?;
        self.clear_device_errors().await?;
        // Datasheet §9.2.1: calibration must be done AFTER SetDIO3AsTcxoCtrl.
        // Timeout 0x0000FF (~4 ms) is the value RNode firmware uses.
        self.set_dio3_as_tcxo_ctrl(self.tcxo_voltage_reg, 0x0000FF)
            .await?;
        self.calibrate(0x7F).await?;
        self.calibrate_image(freq_hz).await?;
        // LoRa packet type.
        self.write_command(opcode::SET_PACKET_TYPE, &[0x01]).await?;
        // Boosted receive gain, and the read-back that makes it observable.
        //
        // There is no AGC on this part, so the value written here is the gain
        // for the whole session, and until now we wrote neither value and ran
        // at the chip's power-saving default — the one register the reference
        // firmware sets that we did not even name (`sx126x.cpp:361`). After
        // Calibrate, because calibration rewrites receiver trim.
        //
        // The write itself lives in `probe_rx_init` so the read-write-read
        // bracket has a host test; the line it returns is the only evidence a
        // capture can carry that the write happened, because the alternative
        // — an unwritten register — looks identical in every other log.
        //
        // Not repeated in `configure_lora`: no command in that path resets it.
        // It IS lost across a sleep/warm-start, which this driver never does;
        // if a sleep is ever added, this write has to move or be repeated (or
        // the address added to the chip's retention list at 0x029F).
        //
        // `log_fmt_critical`, not `log_fmt`: init runs long before DTR-assert
        // opens the runtime drain, so a gated line would be counted and
        // dropped.
        let probe = leviculum_core::sx126x::probe_rx_init(self).await?;
        crate::log::log_fmt_critical("[SX_REG] ", format_args!("{}", probe));
        self.get_status().await
    }

    // Frequency, modulation, packet params
    /// Write a register (datasheet §13.2.1).
    pub async fn write_register(&mut self, addr: u16, data: &[u8]) -> Result<(), Error> {
        self.wait_busy().await?;
        let mut buf = [0u8; 19];
        buf[0] = opcode::WRITE_REGISTER;
        buf[1] = (addr >> 8) as u8;
        buf[2] = (addr & 0xFF) as u8;
        let len = 3 + data.len();
        buf[3..len].copy_from_slice(data);
        self.spi
            .transaction(&mut [Operation::Write(&buf[..len])])
            .await
            .map_err(|_| Error::Spi)
    }

    /// Read a register (datasheet §13.2.2). See also the single-byte
    /// [`RegisterBus`](leviculum_core::sx126x::RegisterBus) forwarding below.
    pub async fn read_register(&mut self, addr: u16, data: &mut [u8]) -> Result<(), Error> {
        self.wait_busy().await?;
        let cmd = [
            opcode::READ_REGISTER,
            (addr >> 8) as u8,
            (addr & 0xFF) as u8,
        ];
        let mut status = [0u8; 1];
        self.spi
            .transaction(&mut [
                Operation::Write(&cmd),
                Operation::Read(&mut status),
                Operation::Read(data),
            ])
            .await
            .map_err(|_| Error::Spi)
    }

    /// Set RF frequency (datasheet §13.4.1). RF_Freq = freq_hz * 2^25 / 32_000_000
    pub async fn set_rf_frequency(&mut self, freq_hz: u32) -> Result<(), Error> {
        let rf_freq = ((freq_hz as u64 * (1u64 << 25)) / 32_000_000) as u32;
        self.write_command(
            opcode::SET_RF_FREQUENCY,
            &[
                (rf_freq >> 24) as u8,
                (rf_freq >> 16) as u8,
                (rf_freq >> 8) as u8,
                rf_freq as u8,
            ],
        )
        .await
    }

    /// Set LoRa packet params (datasheet §13.4.6), then apply errata 15.4.
    ///
    /// The erratum is that `SetPacketParams` leaves `IqPolarity` (0x0736) in a
    /// state that does not follow from the IQ setting it was just given, so
    /// bit 2 has to be corrected afterwards — every time, which is why this
    /// sits inside the same function rather than beside its callers. Bit 2 is
    /// SET for the standard IQ this driver always programs (byte 5 = 0x00
    /// below); the reference firmware branches on that same byte at
    /// `sx126x.cpp:291-298`.
    ///
    /// Its absence here was not a symptom we had measured: we interoperate
    /// with RNode peers today, which is evidence that the chip already leaves
    /// bit 2 set in the standard-IQ case. The change removes a divergence
    /// whose harmlessness rested on that inference rather than on the
    /// datasheet — and it costs a register read plus a register write per
    /// call, which on the TX path is one per frame.
    ///
    /// The first call of a boot pays two further reads and emits
    /// `[SX_REG_IQ]`, which is what settles whether the correction changes
    /// anything on this chip: equal before- and after-values confirm the
    /// inference above, unequal ones mean the divergence was real. Every
    /// later call is exactly as expensive as it was before the read-back
    /// existed.
    pub async fn set_packet_params(&mut self, payload_len: u8) -> Result<(), Error> {
        self.write_command(
            opcode::SET_PACKET_PARAMS,
            &[
                (self.preamble_len >> 8) as u8,
                self.preamble_len as u8,
                0x00, // explicit header
                payload_len,
                0x01, // CRC on
                0x00, // standard IQ
            ],
        )
        .await?;
        let probe = !self.iq_probe_done;
        if let Some(line) = leviculum_core::sx126x::apply_iq_polarity(self, false, probe).await? {
            self.iq_probe_done = true;
            crate::log::log_fmt_critical("[SX_REG_IQ] ", format_args!("{}", line));
        }
        Ok(())
    }

    /// Apply TX PA clamp workaround (datasheet §15.2).
    async fn apply_tx_clamp_workaround(&mut self) -> Result<(), Error> {
        let mut val = [0u8; 1];
        self.read_register(reg::TX_CLAMP_CONFIG, &mut val).await?;
        val[0] |= 0x1E;
        self.write_register(reg::TX_CLAMP_CONFIG, &val).await
    }

    /// Configure radio for LoRa TX/RX with specific parameters.
    /// Call after init_radio(). Sets frequency, PA, modulation, packet params, sync word.
    pub async fn configure_lora(
        &mut self,
        freq_hz: u32,
        sf: u8,
        bw: u8, // SX1262 bandwidth code: 0x04 = 125kHz
        cr: u8, // SX1262 coding rate code: 0x01 = 4/5
        power_dbm: i8,
        preamble_len: u16,
    ) -> Result<(), Error> {
        // The third path that leaves RX, and the least obvious one: the loop
        // takes a runtime config override at the top of an iteration, which
        // can be the iteration right after a reception re-armed the receiver
        // provisionally. Every command below expects STBY_RC.
        self.disarm_rx(leviculum_core::sx126x::RxTeardownBy::Config)
            .await?;
        self.preamble_len = preamble_len;
        // Cache the modulation profile so `receive()` can size its software-wait
        // extension. `cr` is the SX1262 code (denominator - 4); `bw` is the
        // register code (see `bw_code_to_hz`).
        self.rx_ext_sf = sf;
        self.rx_ext_bw_hz = bw_code_to_hz(bw);
        self.rx_ext_cr_denom = cr.saturating_add(4);
        self.set_rf_frequency(freq_hz).await?;
        // Transmit power: one PA config for every output, the configured value
        // passed through to `SetTxParams`, clamped to what the part can do
        // (Codeberg #349). The sequence — and the ordering inside it, which
        // matters because `SetPaConfig` resets the OCP register — is
        // `sx126x::program_tx_power` in core, where a fake SPI port can watch
        // the ops; this driver holds only the bus.
        //
        // What it replaces: the requested power never reached `SetTxParams`,
        // which was sent a literal +22 while a four-row PA table decided the
        // real output. Four reachable powers, nothing below 14 dBm, and a
        // configured 2 dBm on the air at roughly 14 — about 12 dB louder than
        // the RNode leg of the same scenario, driven from the same `txpower`
        // line.
        //
        // The read-back goes out through `facts`, unconditionally, on the sink
        // that survives a boot nobody was attached for: `[LORA] active config`
        // reports the *request*, so without this nothing an operator can read
        // names the power the chip was actually given.
        let programmed = leviculum_core::sx126x::program_tx_power(self, power_dbm).await?;
        leviculum_log_line::facts::tx_power_programmed(
            &mut crate::lora::FirmwareLog,
            &leviculum_log_line::facts::TxPowerProgrammed {
                requested_dbm: programmed.requested_dbm,
                programmed_dbm: programmed.programmed_dbm,
                pa_config: programmed.pa_config,
                ramp: programmed.ramp,
                ocp: programmed.ocp,
                clamped: programmed.clamped,
            },
        );
        self.write_command(opcode::SET_BUFFER_BASE_ADDRESS, &[0x00, 0x00])
            .await?;
        // LDRO is keyed to symbol duration; the decision lives in core
        // (`sx126x::ldro_enabled`) because the bandwidth-code space is not
        // monotonic and both link ends must agree.
        let ldro = if leviculum_core::sx126x::ldro_enabled(self.rx_ext_bw_hz, sf) {
            1
        } else {
            0
        };
        self.write_command(
            opcode::SET_MODULATION_PARAMS,
            &[sf, bw, cr, ldro, 0, 0, 0, 0],
        )
        .await?;
        self.set_packet_params(0xFF).await?;
        // Private network sync word (matches RNode)
        self.write_register(reg::LORA_SYNC_WORD, &[0x14, 0x24])
            .await?;
        self.apply_tx_clamp_workaround().await?;
        // StopRxTimerOnPreambleDetect: the SetRx hardware timeout only bounds the
        // preamble wait. Once a preamble is detected the timer stops and RX runs
        // to packet completion regardless of length. Without this a slow-SF
        // packet whose airtime exceeds the 500ms idle-loop timeout (e.g. SF10
        // path requests ~887ms, announces ~2590ms) is aborted mid-packet, so
        // the LNode receives almost nothing at slow spreading factors.
        // Persistent setting, re-asserted on every reconfig via this path.
        self.write_command(opcode::SET_STOP_RX_TIMER_ON_PREAMBLE, &[0x01])
            .await
    }

    // TX
    /// Transmit a packet. Blocks until TxDone or timeout.
    /// Call configure_lora() first to set frequency/modulation/power.
    pub async fn transmit(&mut self, data: &[u8], timeout_ms: u32) -> Result<(), Error> {
        // Leave RX before anything is programmed. Since the receive path
        // re-arms provisionally to cover the hand-off, a window can be
        // standing when the loop decides to key — and `SetPacketParams` on a
        // listening chip is the "armed while transmitting" case the arming
        // state exists to make impossible.
        //
        // The standby carries the instrument with it: `disarm_rx` reads what
        // the window had latched before it ends it. On the ordinary CSMA path
        // the CAD has already taken that standby and this finds nothing
        // standing, so one key-up still produces at most one
        // `[SX_RX_TEARDOWN]`.
        self.disarm_rx(leviculum_core::sx126x::RxTeardownBy::Tx)
            .await?;
        self.set_packet_params(data.len() as u8).await?;
        self.write_command(opcode::SET_DIO_IRQ_PARAMS, &irq::tx_irq_params())
            .await?;
        self.write_command(opcode::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])
            .await?;

        // Write payload, use two SPI operations to avoid 258-byte stack buffer
        self.wait_busy().await?;
        let header = [opcode::WRITE_BUFFER, 0x00];
        self.spi
            .transaction(&mut [Operation::Write(&header), Operation::Write(data)])
            .await
            .map_err(|_| Error::Spi)?;

        // Start TX (no hardware timeout, we use our own)
        let t = u24_be(0);
        self.write_command(opcode::SET_TX, &t).await?;

        // Wait for DIO1 high (TxDone) via GPIOTE interrupt
        match with_timeout(
            Duration::from_millis(timeout_ms.max(100) as u64),
            self.dio1.wait_for_high(),
        )
        .await
        {
            Ok(()) => {
                let flags = self.get_irq_status().await?;
                self.write_command(opcode::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])
                    .await?;
                if flags & irq::IRQ_TX_DONE != 0 {
                    return Ok(());
                }
                let _ = self.set_standby_rc().await;
                Err(Error::Timeout)
            }
            Err(_) => {
                let _ = self.set_standby_rc().await;
                Err(Error::Timeout)
            }
        }
    }

    // RX
    /// Get RX buffer status: payload length and start pointer (datasheet §13.5.2).
    async fn get_rx_buffer_status(&mut self) -> Result<(u8, u8), Error> {
        let mut buf = [0u8; 2];
        let _status = self
            .read_command(opcode::GET_RX_BUFFER_STATUS, &mut buf)
            .await?;
        Ok((buf[0], buf[1]))
    }

    /// Read data from the RX buffer (datasheet §13.2.4).
    async fn read_buffer(&mut self, offset: u8, data: &mut [u8]) -> Result<(), Error> {
        self.wait_busy().await?;
        let cmd = [opcode::READ_BUFFER, offset];
        let mut status = [0u8; 1];
        self.spi
            .transaction(&mut [
                Operation::Write(&cmd),
                Operation::Read(&mut status),
                Operation::Read(data),
            ])
            .await
            .map_err(|_| Error::Spi)
    }

    /// Get packet status: RSSI and SNR (datasheet §13.5.3).
    async fn get_packet_status(&mut self) -> Result<RxStatus, Error> {
        let mut buf = [0u8; 3];
        let _status = self
            .read_command(opcode::GET_PACKET_STATUS, &mut buf)
            .await?;
        let (rssi, snr) = irq::packet_status_dbm(buf);
        Ok(RxStatus { rssi, snr })
    }

    /// Apply workaround 15.3: stop RTC after Rx with timeout (datasheet §15.3).
    async fn apply_rx_timeout_workaround(&mut self) -> Result<(), Error> {
        self.write_register(reg::RTC_CONTROL, &[0x00]).await?;
        let mut val = [0u8; 1];
        self.read_register(reg::EVENT_MASK, &mut val).await?;
        val[0] |= 0x02;
        self.write_register(reg::EVENT_MASK, &val).await
    }

    /// Put the chip in standby if a window is standing, with no instrument
    /// attached.
    ///
    /// The one place a `SetStandby` is spent on account of RX. Private, and
    /// reached only through [`disarm_rx`](Self::disarm_rx) or the
    /// `RxPort::disarm` that [`leviculum_rx_arming::stand_down`] calls after
    /// its read: an uninstrumented teardown in the firmware is a hole in the
    /// rate.
    ///
    /// The order inside matters. The state is cleared only *after* the standby
    /// command has completed, so a future dropped inside that command still
    /// owes a standby and the next caller spends it. Clearing first would let
    /// a dropped disarm leave a listening chip that nothing believes is
    /// listening — and the next thing that path does is `SetTx`.
    async fn standby_rx(&mut self) -> Result<(), Error> {
        if !self.rx_state.standby_owed() {
            return Ok(());
        }
        self.set_standby_rc().await?;
        self.rx_state.disarmed();
        // The receiver stopped here, so the next arming's `dark_ms` is
        // measured from this instant. Without it the gap instrument would
        // charge the next window with the time the radio spent listening on a
        // provisional arm, which is the opposite of what it measures.
        self.rx_arm
            .window_ended(embassy_time::Instant::now().as_millis());
        Ok(())
    }

    /// Stand the receiver down, if a window is standing, and record what was
    /// on the air when it went down.
    ///
    /// What every path that leaves RX calls: it costs nothing when no window
    /// is standing, exactly one command when one is, and one
    /// `[SX_RX_TEARDOWN]` line either way round. `by` names the caller — see
    /// [`leviculum_rx_arming::RxTeardown`] for why that and not the window's
    /// own tag.
    ///
    /// One implementation for all six callers rather than one for the key-ups
    /// and another for the rest. The previous batch instrumented only the
    /// three key-ups, reasoning that a re-arm is "the same window continuing";
    /// the sweep refuted it, and a second implementation would have been a
    /// second place for that reasoning to hide.
    ///
    /// A failed read is reported on its own line rather than folded into the
    /// teardown line or swallowed: it is a lost sample, and a rate computed
    /// from a population with invisible holes is wrong in the direction that
    /// says "no problem here".
    pub async fn disarm_rx(
        &mut self,
        by: leviculum_core::sx126x::RxTeardownBy,
    ) -> Result<(), Error> {
        leviculum_rx_arming::stand_down(self, by.tag()).await
    }

    /// Stand the receiver down for a transmit, waiting first if the window is
    /// holding a frame that is still arriving.
    ///
    /// [`disarm_rx`](Self::disarm_rx) with the deferral in front of it, and
    /// the reason it is a separate method rather than a flag: exactly one
    /// caller may defer. The bound is per call, so a second site reaching for
    /// this would turn "one frame's airtime, once" into a wait that compounds,
    /// and the starvation argument would stop holding. The sequence itself is
    /// [`leviculum_rx_arming::stand_down_for_tx`], where a fake radio asserts
    /// it.
    ///
    /// `buf` and `sink` are what a reception the wait catches goes through —
    /// the same buffer and the same `lora::CoreHandoff` any other
    /// window's frame takes, so a frame delivered from a deferral is
    /// indistinguishable downstream from one delivered from `rx_once`.
    pub async fn disarm_rx_for_tx<S>(
        &mut self,
        by: leviculum_core::sx126x::RxTeardownBy,
        buf: &mut [u8],
        sink: &mut S,
    ) -> Result<(), Error>
    where
        S: leviculum_rx_arming::FrameSink<Meta = RxStatus>,
    {
        leviculum_rx_arming::stand_down_for_tx(self, by.tag(), buf, sink).await
    }

    /// Arm the receiver: the chip starts listening. `timeout_ms == 0` is
    /// single mode (no hardware timeout, listen until a packet arrives).
    ///
    /// The arming half of what used to be `receive()`. Split out so it can be
    /// issued *before* a frame is handed upward instead of after — see
    /// [`leviculum_rx_arming::receive_and_hand_up`], which is the only caller
    /// that pairs it with [`await_rx`](Self::await_rx).
    ///
    /// **Unconditional.** Whether a `SetRx` is wanted at all is
    /// [`leviculum_rx_arming::ensure_armed`]'s decision, and this is what it
    /// calls when the answer is yes; a window that is already standing with
    /// these exact parameters never reaches here.
    ///
    /// `site` names the caller's window in the `[SX_RX_ARM]` line. It is a
    /// parameter rather than something the driver could infer: the driver sees
    /// only a duration, and two windows of the same length mean entirely
    /// different things to whoever reads the capture. It is also half of the
    /// window's identity for the adoption decision, so a window is adopted
    /// only if the capture would call it the same thing.
    pub async fn arm_rx(
        &mut self,
        timeout_ms: u32,
        site: leviculum_core::sx126x::RxSite,
    ) -> Result<(), Error> {
        // Never armed twice: a standing window is stood down first — and
        // counted, because this `SetStandby` is the one the sweep found
        // ending receptions mid-air. It is a no-op on the ordinary path (the
        // chip left RX at the terminating IRQ) and one command where the
        // loop wants a window different from the one standing.
        self.disarm_rx(leviculum_core::sx126x::RxTeardownBy::Arm)
            .await?;

        self.write_command(opcode::SET_DIO_IRQ_PARAMS, &irq::rx_irq_params())
            .await?;
        self.write_command(opcode::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])
            .await?;

        // Convert timeout_ms to RTC steps (15.625µs per step = 64 steps/ms)
        let hw_timeout = if timeout_ms == 0 {
            0x000000 // single mode
        } else {
            (timeout_ms as u64 * 64).min(0xFFFFFF) as u32
        };
        // Recorded before the command goes out, not after: the idle branch
        // runs this inside a `select` and drops it the instant the daemon has
        // outgoing data. A drop inside the SPI transaction below leaves a chip
        // that may or may not be listening, and the safe reading of "may" is
        // that a standby is owed.
        self.rx_state.arming(ArmedWindow {
            timeout_ms,
            hw_timeout,
            site,
            armed_at_ms: embassy_time::Instant::now().as_millis(),
        });
        let t = u24_be(hw_timeout);
        self.write_command(opcode::SET_RX, &t).await?;

        // The receiver is live from here. Stamped AFTER the command returns,
        // not before: the BUSY wait and SPI transaction inside it are dark,
        // and crediting them to the window would under-report the gap. The
        // error is bounded by one short SPI transaction and it errs the safe
        // way — toward reporting more dark time than there was.
        //
        // Everything from the previous window's end to here is the gap: that
        // window's buffer readout, the loop's decision, and the two IRQ
        // commands above. Since the re-arm moved ahead of the hand-off, what
        // this no longer contains is the hand-off itself — which is the whole
        // measurement the batch exists to move.
        let arm = self
            .rx_arm
            .arm(site, timeout_ms, embassy_time::Instant::now().as_millis());
        crate::log::log_fmt("[SX_RX_ARM] ", format_args!("{arm}"));
        Ok(())
    }

    /// Wait for the standing window's terminating IRQ and read the frame out
    /// of the chip's buffer. Returns (bytes_written, RxStatus) on success.
    ///
    /// The awaiting half of what used to be `receive()`. The chip has left RX
    /// when this returns: on `RxDone` and on the hardware timeout it returns
    /// to STBY_RC by itself, and the one branch where it might not — the
    /// software wait expiring with neither IRQ set — forces a standby.
    ///
    /// **Waits on an edge, so it must not be entered with a terminating IRQ
    /// already latched.** On an adopted window that IRQ can have fired while
    /// nobody was waiting, and DIO1 will not rise a second time for it.
    /// [`leviculum_rx_arming::await_window`] is what keeps that case away from
    /// here, by asking [`take_latched_frame`](Self::take_latched_frame) first.
    pub async fn await_rx(&mut self, buf: &mut [u8]) -> Result<(u8, RxStatus), Error> {
        let Some(&ArmedWindow { timeout_ms, .. }) = self.rx_state.window() else {
            return Err(Error::NotArmed);
        };

        // Wait for DIO1 high via GPIOTE interrupt (hw timeout + margin)
        let sw_timeout_ms = if timeout_ms == 0 {
            60_000u64
        } else {
            timeout_ms as u64 + 500
        };
        let _ = with_timeout(
            Duration::from_millis(sw_timeout_ms),
            self.dio1.wait_for_high(),
        )
        .await;
        // The terminating IRQ — RxDone or the hardware timeout — has been
        // observed, so the radio has stopped listening. Recorded before the
        // status read below, which is dark time and belongs to the next gap,
        // and before that read's `?`: a window that ends in an SPI error still
        // ended, and the next arm should measure from here.
        self.rx_arm
            .window_ended(embassy_time::Instant::now().as_millis());

        let flags = self.get_irq_status().await?;
        self.finish_rx(flags, buf).await
    }

    /// Take a reception the chip has already completed, without waiting on any
    /// edge. `Ok(None)` when no terminating IRQ is latched, in which case
    /// nothing has been consumed or cleared and the caller may wait.
    ///
    /// The hazard of adoption, handled where it can be: a window that was
    /// adopted rather than armed may already have run to `RxDone` — that is
    /// the whole point of adopting it — and the frame is sitting in the chip's
    /// buffer with DIO1 already high and no second edge coming. Reading the
    /// status first costs one short transaction on a freshly armed window,
    /// whose status the arming just cleared.
    async fn take_latched_frame(
        &mut self,
        buf: &mut [u8],
    ) -> Result<Option<(u8, RxStatus)>, Error> {
        if !self.rx_state.standby_owed() {
            return Err(Error::NotArmed);
        }
        let flags = self.get_irq_status().await?;
        if flags & (irq::IRQ_RX_DONE | irq::IRQ_TIMEOUT) == 0 {
            return Ok(None);
        }
        // The window ended when that IRQ fired, which was before anybody
        // looked; "now" is the earliest instant we can honestly claim, and it
        // is the one the next arming's `dark_ms` measures from.
        self.rx_arm
            .window_ended(embassy_time::Instant::now().as_millis());
        self.finish_rx(flags, buf).await.map(Some)
    }

    /// Classify a terminated window and read out whatever it caught.
    ///
    /// The tail both entries into the window share — the wait in
    /// [`await_rx`](Self::await_rx) and the latched take above — so a
    /// reception is completed identically however it was noticed. `flags` is
    /// the status as read, not re-read: on the adopted path a second read
    /// after the first would be a second chance to race the clear below.
    async fn finish_rx(&mut self, mut flags: u16, buf: &mut [u8]) -> Result<(u8, RxStatus), Error> {
        let Some(&ArmedWindow { hw_timeout, .. }) = self.rx_state.window() else {
            return Err(Error::NotArmed);
        };

        // The software wait (timeout_ms + 500) can expire while a slow-SF frame
        // is still on the air; see `sx126x::rx_extend_ms` for the mechanism and
        // for why the decision is not written here. The IRQ status is
        // deliberately NOT cleared before the extension so a pending RxDone
        // survives into the extended wait.
        if let Some(extend_ms) = irq::rx_extend_ms(
            flags,
            self.rx_ext_bw_hz,
            self.rx_ext_sf,
            self.rx_ext_cr_denom,
            self.preamble_len,
        ) {
            crate::log::log_fmt(
                "[SX_RX_EXTEND] ",
                format_args!(
                    "flags={:#06x} extend_ms={} sf={} bw_hz={}",
                    flags, extend_ms, self.rx_ext_sf, self.rx_ext_bw_hz
                ),
            );
            let _ = with_timeout(Duration::from_millis(extend_ms), self.dio1.wait_for_high()).await;
            // The radio kept listening through the extension, so the window
            // ended here and not where the software wait expired. Supersedes
            // the mark taken above.
            self.rx_arm
                .window_ended(embassy_time::Instant::now().as_millis());
            flags = self.get_irq_status().await?;
        }

        // The two terminating IRQs both return the chip to STBY_RC on their
        // own (`RxDone` in single mode, and the hardware timeout), so the
        // window is over and no standby is owed for it. Recorded here, before
        // the readout below and its `?`: a readout that fails on SPI does not
        // put the chip back in RX. The remaining case — neither flag set, the
        // software wait simply expired — is the one where the chip may still
        // be listening, and it is stood down explicitly in the last arm.
        if flags & (irq::IRQ_RX_DONE | irq::IRQ_TIMEOUT) != 0 {
            self.rx_state.chip_left_rx();
        }

        self.write_command(opcode::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])
            .await?;

        // Workaround 15.3 for timed RX
        if hw_timeout != 0 && hw_timeout != 0xFFFFFF {
            let _ = self.apply_rx_timeout_workaround().await;
        }

        if flags & irq::IRQ_RX_DONE != 0 {
            if flags & irq::IRQ_CRC_ERR != 0 {
                // Characterise the frame before dropping it (Codeberg #258).
                // These are the same two commands the success path issues five
                // lines below, at the same point in the sequence, so this costs
                // one SPI transaction less than a good reception does: the
                // corrupt payload is deliberately NOT read out of the buffer.
                // Both are status reads of values the chip latched when the
                // reception ended; neither waits on the air.
                //
                // `?` rather than a fallback value: if the SPI read itself
                // fails, `Spi` is the honest error, and inventing an rssi to
                // keep the CRC classification would put a fabricated number in
                // the population this exists to measure.
                let (len, _ptr) = self.get_rx_buffer_status().await?;
                let status = self.get_packet_status().await?;
                return Err(Error::Crc(CrcErrFrame {
                    len,
                    rssi: status.rssi,
                    snr: status.snr,
                }));
            }
            let (len, ptr) = self.get_rx_buffer_status().await?;
            let read_len = (len as usize).min(buf.len());
            self.read_buffer(ptr, &mut buf[..read_len]).await?;
            let status = self.get_packet_status().await?;
            Ok((read_len as u8, status))
        } else if flags & irq::IRQ_TIMEOUT != 0 {
            Err(Error::Timeout)
        } else {
            // Neither terminating IRQ: the software wait expired on a chip
            // that may still be in RX. `disarm_rx` rather than a bare
            // `set_standby_rc` so the state and the gap clock agree with the
            // command — the window is over exactly once, here — and so this
            // teardown lands in the same population as the others. It is one
            // that can genuinely destroy a reception: a preamble whose frame
            // outlasted even the extension is still on the air.
            let _ = self
                .disarm_rx(leviculum_core::sx126x::RxTeardownBy::RxWait)
                .await;
            Err(Error::Timeout)
        }
    }

    // CAD (Channel Activity Detection)
    /// Perform a Channel Activity Detection. Returns true if a LoRa preamble
    /// was detected (channel busy), false if clear. Blocks until CadDone IRQ.
    /// Exit mode 0x00 leaves the chip in STBY_RC regardless of result.
    pub async fn cad(&mut self, sf: u8) -> Result<bool, Error> {
        // Datasheet Table 13-81 recommended cadDetPeak values per SF.
        // cadSymbolNum: 0x02 = 4 symbols, 0x03 = 8 symbols. For sf>=10 the
        // packet airtime is long (SF10 ~2.7s) and a 4-symbol CAD window usually
        // misses an in-progress peer transmission, so both nodes transmit and
        // collide. Listen over 8 symbols at slow SF to span more of the peer's
        // airtime and detect "busy" reliably. cadDetPeak unchanged.
        let (cad_sym_num, cad_det_peak) = match sf {
            7 | 8 => (0x02, 0x16),
            9 => (0x02, 0x17),
            10 => (0x03, 0x18),
            11 => (0x03, 0x19),
            12 => (0x03, 0x1A),
            _ => (0x02, 0x16),
        };
        let cad_det_min = 0x0A;
        let cad_exit_mode = 0x00; // CAD-only, return to STBY_RC
        let cad_timeout = [0u8; 3];

        // Same reason as `transmit`: the CSMA path can reach here with a
        // provisional window standing, and `SetCad` expects STBY_RC. This is
        // the first of the two commands a key-up issues, so on the CSMA path
        // it is where the teardown is measured.
        self.disarm_rx(leviculum_core::sx126x::RxTeardownBy::Cad)
            .await?;

        self.write_command(
            opcode::SET_CAD_PARAMS,
            &[
                cad_sym_num,
                cad_det_peak,
                cad_det_min,
                cad_exit_mode,
                cad_timeout[0],
                cad_timeout[1],
                cad_timeout[2],
            ],
        )
        .await?;

        self.write_command(opcode::SET_DIO_IRQ_PARAMS, &irq::cad_irq_params())
            .await?;
        self.write_command(opcode::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])
            .await?;

        self.write_command(opcode::SET_CAD, &[]).await?;

        // Timeout sized from the CAD's listening symbols at the live symbol
        // time (SF and BW); a fixed per-SF table here assumed BW125.
        let cad_symbols: u8 = if cad_sym_num == 0x03 { 8 } else { 4 };
        let timeout_ms = irq::cad_timeout_ms(self.rx_ext_bw_hz, sf, cad_symbols);
        match with_timeout(
            Duration::from_millis(timeout_ms as u64),
            self.dio1.wait_for_high(),
        )
        .await
        {
            Ok(()) => {
                let flags = self.get_irq_status().await?;
                self.write_command(opcode::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])
                    .await?;
                Ok((flags & irq::IRQ_CAD_DETECTED) != 0)
            }
            Err(_) => {
                let _ = self.set_standby_rc().await;
                Err(Error::Timeout)
            }
        }
    }
}

/// The receive half of the driver, as the ordering in
/// [`leviculum_rx_arming`] sees it.
///
/// Three forwarding methods and no logic: the arming order lives in that
/// crate, where a fake port can assert it, for the same reason
/// `RegisterBus`'s logic lives in core — this crate cross-compiles to
/// `thumbv7em-none-eabihf` and has no test target, so a sequence written here
/// would be a sequence nobody could run.
impl<SPI: SpiDeviceTrait> leviculum_rx_arming::RxPort for Sx1262<SPI> {
    /// The `SetRx` duration and the site tag the `[SX_RX_ARM]` line carries.
    type Window = (u32, leviculum_core::sx126x::RxSite);
    type Meta = RxStatus;
    type Error = Error;

    async fn arm(&mut self, window: Self::Window) -> Result<(), Error> {
        self.arm_rx(window.0, window.1).await
    }

    async fn await_frame(&mut self, buf: &mut [u8]) -> Result<(u8, RxStatus), Error> {
        self.await_rx(buf).await
    }

    async fn disarm(&mut self) -> Result<(), Error> {
        // The uninstrumented standby: this is what `stand_down` calls *after*
        // its own read, so routing it back through `disarm_rx` would read the
        // status twice and recurse.
        self.standby_rx().await
    }
}

/// What the arming decision and the teardown instrument ask the chip and the
/// arming state, as [`leviculum_rx_arming::ensure_armed`] and
/// [`leviculum_rx_arming::stand_down`] ask it.
///
/// Observations and one log side-channel, no logic, for the same reason the
/// port above is three forwarding methods: the sequences — adopt or replace,
/// read before the standby, take a latched reception before waiting on an
/// edge — live where a fake radio can assert them.
impl<SPI: SpiDeviceTrait> leviculum_rx_arming::RxWindowProbe for Sx1262<SPI> {
    fn standing_window(
        &self,
    ) -> Option<leviculum_rx_arming::StandingWindow<(u32, leviculum_core::sx126x::RxSite)>> {
        self.rx_state.window().map(|w| {
            let stood_ms = embassy_time::Instant::now()
                .as_millis()
                .saturating_sub(w.armed_at_ms);
            leviculum_rx_arming::StandingWindow {
                window: (w.timeout_ms, w.site),
                // A window standing for 49 days is not a reading anybody needs
                // to distinguish; the clamp keeps the field a `u32` and the
                // line a fixed width.
                stood_ms: stood_ms.min(u32::MAX as u64) as u32,
            }
        })
    }

    /// A `SetRx` duration of zero is single mode: the chip listens until a
    /// frame arrives and the window never concludes on its own. Every other
    /// duration is a hardware timeout, so the window names its own end.
    fn window_ends_itself(&self, window: &(u32, leviculum_core::sx126x::RxSite)) -> bool {
        window.0 != 0
    }

    async fn latched(&mut self) -> Result<leviculum_rx_arming::RxLatch, Error> {
        // `GetIrqStatus` reads; `ClearIrqStatus` is a separate opcode and is
        // deliberately not issued here. The window's latch mask is
        // `IRQ_LATCH_ALL` (see `irq::rx_irq_params`), so the preamble and
        // header bits are readable back even though only `DIO1_RX` ever raised
        // the pin — and on an adopted window a clear here would consume the
        // `RxDone` the awaiting half is about to take.
        let flags = self.get_irq_status().await?;
        Ok(leviculum_rx_arming::RxLatch {
            raw: flags,
            preamble: flags & irq::IRQ_PREAMBLE_DETECTED != 0,
            header: flags & irq::IRQ_HEADER_VALID != 0,
            rxdone: flags & irq::IRQ_RX_DONE != 0,
        })
    }

    async fn take_latched_frame(
        &mut self,
        buf: &mut [u8],
    ) -> Result<Option<(u8, RxStatus)>, Error> {
        self.take_latched_frame(buf).await
    }

    /// Forwards to `sx126x::tx_defer_ms` against the modulation the last
    /// `configure_lora` programmed — the same three cached fields
    /// [`finish_rx`](Sx1262::finish_rx) sizes its RX extension from, because
    /// it is the same question. `rx_ext_bw_hz` is 0 before the radio is
    /// configured, which is the one state in which no airtime exists and the
    /// answer is "do not wait".
    fn defer_ms(&self, latch: &leviculum_rx_arming::RxLatch) -> Option<u64> {
        irq::tx_defer_ms(
            latch.raw,
            self.rx_ext_bw_hz,
            self.rx_ext_sf,
            self.rx_ext_cr_denom,
            self.preamble_len,
        )
    }

    /// The DIO1 wait, bounded, with the chip left in RX for its whole
    /// duration — no command goes out here, which is the entire point.
    ///
    /// `DIO1_RX` routes only the terminating interrupts (`RxDone`, `CrcErr`,
    /// `Timeout`), so the pin cannot rise for the preamble or header bits that
    /// earned the wait; and the deferral is only ever entered with none of the
    /// terminating three latched, so the edge this waits on is genuinely still
    /// to come rather than one that has already passed.
    ///
    /// The elapsed time is measured across the wait rather than assumed to be
    /// the bound: it is the numerator of the question `[SX_TX_DEFER]` exists
    /// to answer.
    async fn wait_for_frame(&mut self, wait_ms: u64) -> u64 {
        let started = embassy_time::Instant::now();
        let _ = with_timeout(Duration::from_millis(wait_ms), self.dio1.wait_for_high()).await;
        started.elapsed().as_millis()
    }

    fn report(&mut self, event: leviculum_rx_arming::RxEvent<'_, Error>) {
        match event {
            leviculum_rx_arming::RxEvent::Adopted(adopt) => {
                crate::log::log_fmt("[SX_RX_ADOPT] ", format_args!("{adopt}"));
            }
            leviculum_rx_arming::RxEvent::TornDown(teardown) => {
                crate::log::log_fmt("[SX_RX_TEARDOWN] ", format_args!("{teardown}"));
            }
            leviculum_rx_arming::RxEvent::Deferred(defer) => {
                crate::log::log_fmt("[SX_TX_DEFER] ", format_args!("{defer}"));
            }
            leviculum_rx_arming::RxEvent::ProbeFailed { at, error } => {
                crate::log::log_fmt(
                    "[SX_RX_PROBE_ERR] ",
                    format_args!("at={} error={:?}", at, error),
                );
            }
        }
    }
}

/// Single-byte register access for `leviculum_core::sx126x`'s register probes.
///
/// Two forwarding methods and no logic. The logic — which register, which
/// value, and in which order relative to the write — is in core, because that
/// is where a test can run it: this crate cross-compiles to
/// `thumbv7em-none-eabihf` and has no test target, so a read-write-read
/// sequence written here would be a sequence nobody could distinguish from a
/// single read plus a hardcoded second value.
impl<SPI: SpiDeviceTrait> leviculum_core::sx126x::RegisterBus for Sx1262<SPI> {
    type Error = Error;

    async fn read_reg(&mut self, addr: u16) -> Result<u8, Error> {
        let mut byte = [0u8; 1];
        self.read_register(addr, &mut byte).await?;
        Ok(byte[0])
    }

    async fn write_reg(&mut self, addr: u16, value: u8) -> Result<(), Error> {
        self.write_register(addr, &[value]).await
    }
}

/// The command half, for the transmit-power sequence (Codeberg #349).
///
/// One forwarding method, for the same reason as the two above: the sequence
/// `SetPaConfig` -> `REG_OCP` -> `SetTxParams` has an order that matters and
/// bytes that decide what goes on the air, and neither can be checked in a
/// crate with no test target.
impl<SPI: SpiDeviceTrait> leviculum_core::sx126x::CommandBus for Sx1262<SPI> {
    async fn write_cmd(&mut self, opcode: u8, args: &[u8]) -> Result<(), Error> {
        self.write_command(opcode, args).await
    }
}
