//! USB composite device with two CDC-ACM serial ports
//!
//! Port 0 (debug): carries human-readable log output from `info!`/`warn!` macros.
//! Port 1 (transport): Reticulum serial interface with HDLC framing.
//!
//! The host sees two `/dev/ttyACM*` devices. The debug port sends log messages
//! formatted with `\r\n` line endings for terminal compatibility.

extern crate alloc;

use alloc::vec::Vec;
use embassy_time::Duration;

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{peripherals, usb, Peri};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_time::with_timeout;
use embassy_usb::class::cdc_acm::{self, CdcAcmClass, State};
use embassy_usb::control::{OutResponse, Recipient, Request, RequestType};
use embassy_usb::{Builder, Config, Handler, UsbDevice};
use leviculum_core::envelope::{self, ControlAction};
use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};
use static_cell::StaticCell;

use crate::boards::BoardConfig;
use crate::log::{log_fmt, LOG_RING, LOG_SIGNAL};

// Interrupt bindings are centralized in ble.rs (CLOCK_POWER shared with MPSL)
use crate::ble::Irqs;

static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
static MSOS_DESC: StaticCell<[u8; 0]> = StaticCell::new();
static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
static CDC_DEBUG_STATE: StaticCell<State<'static>> = StaticCell::new();
static CDC_RETIC_STATE: StaticCell<State<'static>> = StaticCell::new();
static BAUD_TOUCH: StaticCell<BaudTouchHandler> = StaticCell::new();

/// Channels for serial interface: NodeCore ↔ USB CDC-ACM
static INCOMING_CHANNEL: Channel<CriticalSectionRawMutex, Vec<u8>, 8> = Channel::new();
static OUTGOING_CHANNEL: Channel<CriticalSectionRawMutex, Vec<u8>, 8> = Channel::new();

/// Wall-time injections from the host (#238 envelope, `TYPE_WALL_TIME`).
/// The node core owns the calendar, so the serial task hands the value to
/// the main loop; the main loop answers with the enveloped ack or refusal
/// through the ordinary outgoing channel once the seam has spoken.
static WALL_TIME_CHANNEL: Channel<CriticalSectionRawMutex, u64, 1> = Channel::new();

/// Host-requested announces (#376, `TYPE_ANNOUNCE`). The node core owns
/// the destinations and the clock gate, so the serial task hands the
/// request to the main loop the same way it hands the wall time; the
/// main loop answers with the enveloped ack or the `no-clock` refusal
/// through the ordinary outgoing channel.
static ANNOUNCE_CHANNEL: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();

/// The control-frame types this firmware accepts — what the capability
/// report advertises.
pub const ACCEPTED_CONTROL_TYPES: &[u8] = &[
    envelope::TYPE_RADIO_CONFIG,
    envelope::TYPE_RESET,
    envelope::TYPE_WALL_TIME,
    envelope::TYPE_CAPABILITIES,
    envelope::TYPE_TELEMETRY_TARGET,
    envelope::TYPE_TX_SPACING,
    envelope::TYPE_RADIO_QUERY,
    envelope::TYPE_FIXED_POSITION,
    envelope::TYPE_MEDIA_PROFILE,
    envelope::TYPE_MEDIA_QUERY,
    envelope::TYPE_POSITION_SOURCE_QUERY,
    envelope::TYPE_NODE_NAME,
    envelope::TYPE_NODE_NAME_QUERY,
    envelope::TYPE_IDENTITY_QUERY,
    envelope::TYPE_ANNOUNCE,
    envelope::TYPE_BLE_TX_GAP,
];

/// nRF52840 FICR base address
const FICR_BASE: u32 = 0x1000_0000;
/// DEVICEID[0] register offset
const FICR_DEVICEID0_OFFSET: u32 = 0x060;
/// DEVICEID[1] register offset
const FICR_DEVICEID1_OFFSET: u32 = 0x064;

/// Read the nRF52840 factory-programmed unique device ID from FICR registers
/// and format as a 16-character uppercase hex string (static lifetime).
fn serial_number() -> &'static str {
    static SERIAL: StaticCell<[u8; 16]> = StaticCell::new();
    // SAFETY: FICR registers are read-only factory-programmed values, always safe to read
    let id0 =
        unsafe { core::ptr::read_volatile((FICR_BASE + FICR_DEVICEID0_OFFSET) as *const u32) };
    let id1 =
        unsafe { core::ptr::read_volatile((FICR_BASE + FICR_DEVICEID1_OFFSET) as *const u32) };
    let buf = SERIAL.init([0u8; 16]);
    hex_u32(&mut buf[0..8], id0);
    hex_u32(&mut buf[8..16], id1);
    // buf contains only ASCII hex digits, always valid UTF-8
    match core::str::from_utf8(buf) {
        Ok(s) => s,
        Err(_) => "0000000000000000",
    }
}

fn hex_u32(buf: &mut [u8], val: u32) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for i in 0..8 {
        buf[i] = HEX[((val >> (28 - i * 4)) & 0xF) as usize];
    }
}

/// Channel endpoints for the main loop to communicate with the serial task.
pub struct SerialChannels {
    /// Receive packets from USB (deframed by serial task)
    pub incoming_rx: Receiver<'static, CriticalSectionRawMutex, Vec<u8>, 8>,
    /// Send packets to USB (serial task frames and writes)
    pub outgoing_tx: Sender<'static, CriticalSectionRawMutex, Vec<u8>, 8>,
    /// Receive host wall-time injections (#238 `TYPE_WALL_TIME`); the main
    /// loop calls the calendar seam and answers via `outgoing_tx`.
    pub wall_time_rx: Receiver<'static, CriticalSectionRawMutex, u64, 1>,
    /// Receive host announce requests (#376 `TYPE_ANNOUNCE`); the main
    /// loop runs the announce — clock gate and all — and answers via
    /// `outgoing_tx`.
    pub announce_rx: Receiver<'static, CriticalSectionRawMutex, (), 1>,
}

/// Initialize USB composite device and spawn driver tasks.
///
/// Returns channel endpoints for the Reticulum serial interface.
///
/// Spawns three tasks:
/// - `usb_task`: drives the USB bus state machine
/// - `debug_writer_task`: drains log channel to CDC-ACM port 0
/// - `retic_serial_task`: HDLC-framed bidirectional I/O on CDC-ACM port 1
pub fn init(
    spawner: &Spawner,
    usbd: Peri<'static, peripherals::USBD>,
    vbus: &'static SoftwareVbusDetect,
    board: &'static BoardConfig,
) -> SerialChannels {
    let driver = usb::Driver::new(usbd, Irqs, vbus);

    // TODO: register a proper PID at https://pid.codes/
    let mut config = Config::new(board.usb_vid, board.usb_pid);
    config.manufacturer = Some(board.usb_manufacturer);
    config.product = Some(board.usb_product);
    config.serial_number = Some(serial_number());
    config.max_power = 100;
    config.composite_with_iads = true;

    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 0]),
        CONTROL_BUF.init([0; 128]),
    );

    // 1200-baud-touch: Adafruit nRF52 bootloader trigger.
    // Registered before any CdcAcmClass so the handler sees SET_LINE_CODING
    // requests for either CDC port. See `BaudTouchHandler` below.
    builder.handler(BAUD_TOUCH.init(BaudTouchHandler));

    // CDC-ACM #0: Debug log output (interfaces 00+01)
    let cdc_debug = CdcAcmClass::new(&mut builder, CDC_DEBUG_STATE.init(State::new()), 64);

    // CDC-ACM #1: Reticulum serial interface (interfaces 02+03)
    let cdc_retic = CdcAcmClass::new(&mut builder, CDC_RETIC_STATE.init(State::new()), 64);

    let usb_dev = builder.build();

    // The debug CDC is split: TX drains the log ring, RX carries the
    // single-byte post-mortem query (`debug_reader_task`).
    let (cdc_debug_tx, cdc_debug_rx) = cdc_debug.split();

    spawner.must_spawn(usb_task(usb_dev));
    spawner.must_spawn(debug_writer_task(cdc_debug_tx));
    spawner.must_spawn(debug_reader_task(cdc_debug_rx));
    spawner.must_spawn(runtime_drain_open_timeout_task());
    spawner.must_spawn(retic_serial_task(
        cdc_retic,
        INCOMING_CHANNEL.sender(),
        OUTGOING_CHANNEL.receiver(),
        crate::lora::config_sender(),
    ));

    SerialChannels {
        incoming_rx: INCOMING_CHANNEL.receiver(),
        outgoing_tx: OUTGOING_CHANNEL.sender(),
        wall_time_rx: WALL_TIME_CHANNEL.receiver(),
        announce_rx: ANNOUNCE_CHANNEL.receiver(),
    }
}

type UsbDriver = usb::Driver<'static, &'static SoftwareVbusDetect>;

/// CDC PSTN spec §6.3.10: SET_LINE_CODING is class request 0x20,
/// addressed to the CDC communication interface. Payload is the 7-byte
/// LINE_CODING structure: u32 dwDTERate (LE), u8 bCharFormat,
/// u8 bParityType, u8 bDataBits.
const REQ_SET_LINE_CODING: u8 = 0x20;

/// USB control-request handler that triggers the Adafruit nRF52 bootloader
/// when the host opens any CDC port at exactly 1200 baud (the
/// "1200-baud-touch" convention). On match: writes the UF2-stay-in-bootloader
/// magic to GPREGRET and issues a soft reset. The bootloader reads GPREGRET
/// on next boot and stays in UF2 mass-storage mode for flashing.
///
/// All non-1200 SET_LINE_CODING requests fall through to the CdcAcm handler
/// untouched. Strict equality only, fuzzy matching would risk spurious
/// resets when terminal apps round 1215 → 1200.
struct BaudTouchHandler;

impl Handler for BaudTouchHandler {
    fn control_out(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        if req.request_type == RequestType::Class
            && req.recipient == Recipient::Interface
            && req.request == REQ_SET_LINE_CODING
            && data.len() >= 7
        {
            let data_rate = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            if data_rate == 1200 {
                // Hot path: no `await`, no allocation, no logging between the
                // GPREGRET write and `sys_reset()`. Any preemption here would
                // risk the magic being lost before the reset fires.
                const DFU_MAGIC_UF2_RESET: u32 = 0x57;
                // GPREGRET lives in the POWER peripheral, which the S140
                // SoftDevice owns once `Softdevice::enable` has run: a direct
                // write post-enable is an MWU MEMACC (info bit 0, POWER) and
                // records one bogus panic per touch-flash (Codeberg #249).
                // Post-enable the write must go through the SoC syscalls;
                // pre-enable (this handler can fire before `ble::init` on a
                // fast host) the syscalls fail, and the direct write is legal.
                let mut sd_enabled: u8 = 0;
                unsafe {
                    // Legal in every SD state, always returns NRF_SUCCESS.
                    nrf_softdevice_s140::sd_softdevice_is_enabled(&mut sd_enabled);
                }
                if sd_enabled != 0 {
                    unsafe {
                        // GPREGRET syscalls are bit-mask ops: clear all bits,
                        // then set the UF2 magic.
                        nrf_softdevice_s140::sd_power_gpregret_clr(0, 0xFF);
                        nrf_softdevice_s140::sd_power_gpregret_set(0, DFU_MAGIC_UF2_RESET);
                    }
                } else {
                    // SAFETY: pre-`Softdevice::enable` the POWER peripheral is
                    // unprotected; a direct GPREGRET write is legal only in
                    // this window.
                    unsafe {
                        const GPREGRET: *mut u32 = 0x4000_051C as *mut u32;
                        core::ptr::write_volatile(GPREGRET, DFU_MAGIC_UF2_RESET);
                    }
                }
                cortex_m::peripheral::SCB::sys_reset();
                // sys_reset() is `-> !` and never returns.
            }
        }
        // Pass-through for everything else: CdcAcm's own handler will accept
        // the SET_LINE_CODING and update its cached line_coding, or reject as
        // appropriate.
        None
    }
}

#[embassy_executor::task]
async fn usb_task(mut usb: UsbDevice<'static, UsbDriver>) {
    usb.run().await;
}

#[embassy_executor::task]
async fn debug_writer_task(mut cdc: cdc_acm::Sender<'static, UsbDriver>) {
    use embassy_time::{with_timeout, Duration};

    let mut chunk = [0u8; 64];
    loop {
        // Wait for new data or check every 100ms (catches DTR changes)
        let _ = with_timeout(Duration::from_millis(100), LOG_SIGNAL.wait()).await;

        // Only send if host has the port open
        if !cdc.dtr() {
            continue;
        }
        // First DTR-assert observation opens the runtime-drain gate so
        // the LoRa task / tracing subscriber stop dropping their output.
        crate::log::open_runtime_drain();

        // Drain ring buffer to USB
        loop {
            let n = LOG_RING.read(&mut chunk);
            if n == 0 {
                break;
            }
            match with_timeout(Duration::from_millis(50), cdc.write_packet(&chunk[..n])).await {
                Ok(Ok(())) => {}
                _ => break, // USB error or timeout
            }
        }
    }
}

/// RX side of the debug CDC port. The debug console was historically
/// write-only; two single-byte commands are implemented here:
///
/// - `p` — post-mortem query. Replays `[PANIC_COUNT]` and the stored
///   post-mortem block through the normal log path (host helper:
///   `scripts/lnode-panic-query.sh`).
/// - `s` — stack high-water reset (#255 phase A). Emits `[STACK]
///   tag=pre-reset` (the peak of the phase just ended) and then
///   `tag=reset`, and restarts the measurement from here. Sending `s`
///   at each phase boundary of a load run turns the one boot-dominated
///   number into a per-phase peak; see
///   [`crate::reset_stack_watermark`].
///
/// All other bytes are ignored, so terminal line endings and stray input
/// are harmless.
#[embassy_executor::task]
async fn debug_reader_task(mut cdc: cdc_acm::Receiver<'static, UsbDriver>) {
    let mut buf = [0u8; 64];
    loop {
        cdc.wait_connection().await;
        // On read error (USB disconnect) fall back to the outer re-wait.
        while let Ok(n) = cdc.read_packet(&mut buf).await {
            if buf[..n].contains(&b'p') {
                crate::postmortem_query();
            }
            if buf[..n].contains(&b's') {
                // SAFETY: this task's frame is the deepest live one (the
                // executor polls task futures on this single stack), and
                // everything below it is dead. See the fn docs for the
                // interrupt-nesting caveat the margin covers.
                unsafe { crate::reset_stack_watermark() };
            }
        }
    }
}

/// Headless-fallback opener for the runtime-drain gate. If no host
/// asserts DTR within 30 s of boot, open the gate anyway so field
/// deployments without a debug host don't accumulate dropped output
/// indefinitely.
#[embassy_executor::task]
pub(crate) async fn runtime_drain_open_timeout_task() {
    use embassy_time::{Duration, Timer};
    Timer::after(Duration::from_secs(30)).await;
    crate::log::open_runtime_drain();
}

/// Lightweight log helpers for serial task diagnostics (no format_args overhead)
fn log(msg: &str) {
    log_fmt("[SER ] ", format_args!("{}", msg));
}

fn log_u32(msg: &str, val: u32) {
    log_fmt("[SER ] ", format_args!("{} {}", msg, val));
}

/// Serial HW_MTU (matches Python SerialInterface)
const SERIAL_HW_MTU: usize = 564;

/// Write one packet on the transport CDC without letting an absent host
/// wedge the task.
///
/// A CDC IN write only completes when the host polls the endpoint, and
/// the host only polls while a process holds the port open. A plain
/// `write_packet().await` therefore blocks forever once the host closes
/// the port — and because this task services reads and writes from one
/// select loop, a wedged write also stops the OUT endpoint being read:
/// the board goes deaf on if02 while everything else keeps running. The
/// debug port has guarded against this (DTR check plus timeout) since it
/// existed; the transport port did not, and one unread control answer or
/// one announce queued while no host was attached was enough.
///
/// The guard: never start a write while DTR is down, and while a write
/// is pending, wake on control-line changes (`ControlChanged::changed`
/// latches, so a drop between the check and the select is still seen)
/// and abandon the frame when DTR went away. A pending write is never
/// re-issued after an unrelated control change unless DTR is still up,
/// so a present host cannot receive a duplicate packet from this path.
async fn write_packet_host_gated(
    tx: &mut cdc_acm::Sender<'static, UsbDriver>,
    control: &cdc_acm::ControlChanged<'static>,
    chunk: &[u8],
) -> bool {
    loop {
        if !tx.dtr() {
            return false;
        }
        match select(tx.write_packet(chunk), control.control_changed()).await {
            Either::First(Ok(())) => return true,
            Either::First(Err(_)) => return false,
            // Control lines changed mid-write: loop to re-check DTR. If
            // the host is still there the write is simply retried; if it
            // closed the port the frame is abandoned.
            Either::Second(()) => {}
        }
    }
}

/// HDLC-frame `payload` into `frame_buf` and write it out in 64-byte
/// chunks, with a ZLP when the framed length lands exactly on the packet
/// size. Returns `false` on a USB write error or when no host is reading
/// the port (see [`write_packet_host_gated`]).
async fn write_framed(
    tx: &mut cdc_acm::Sender<'static, UsbDriver>,
    control: &cdc_acm::ControlChanged<'static>,
    payload: &[u8],
    frame_buf: &mut Vec<u8>,
) -> bool {
    frame(payload, frame_buf);
    for chunk in frame_buf.chunks(64) {
        if !write_packet_host_gated(tx, control, chunk).await {
            return false;
        }
    }
    if !frame_buf.is_empty()
        && frame_buf.len().is_multiple_of(64)
        && !write_packet_host_gated(tx, control, &[]).await
    {
        return false;
    }
    true
}

/// What became of a host radio config (see [`apply_radio_config`]).
#[derive(PartialEq)]
enum ConfigDelivery {
    /// Handed to the LoRa task and persisted.
    Applied,
    /// A config whose bandwidth or coding rate has no SX1262 register code.
    Invalid,
    /// The config channel would not take it within the grace period. On a
    /// board whose boot held LoRa down this is the steady state: the task
    /// that would drain the channel was never spawned, so the first config
    /// parks in the depth-1 channel forever and every later one is
    /// undeliverable.
    Undeliverable,
}

/// How long a radio config may wait for the LoRa task to drain the
/// previous one before it is refused. A running LoRa task polls the
/// channel every loop turn, so a live consumer clears it in milliseconds;
/// only a consumer that does not exist (LoRa held down at boot) or is
/// itself stuck runs the clock out. Well inside lnflash's 3.5 s answer
/// window so the refusal still reaches the host.
const CONFIG_DELIVER_WITHIN: Duration = Duration::from_millis(500);

/// Hand a parsed radio configuration to the LoRa task and persist it.
///
/// The handoff is bounded (local-4modem-wedge): `config_tx` is a depth-1
/// channel whose only consumer is the LoRa task, and a board booted with
/// `lora=off` never spawned it. An unbounded `send` there blocks this
/// task forever once the channel holds one config — which took the whole
/// transport port deaf until the next reboot, with the write side and
/// main loop running on as if nothing happened. Undeliverable is answered
/// as busy, the same contract [`crate::lora::deliver_tx_spacing`] already
/// has for its consumerless channel.
async fn apply_radio_config(
    config_tx: &Sender<'static, CriticalSectionRawMutex, crate::lora::RadioConfig, 1>,
    wire: leviculum_core::rnode::RadioConfigWire,
) -> ConfigDelivery {
    let Some(cfg) = crate::lora::RadioConfig::from_wire_config(wire) else {
        log("SER: invalid config frame");
        return ConfigDelivery::Invalid;
    };
    if let Err(embassy_sync::channel::TrySendError::Full(cfg)) = config_tx.try_send(cfg) {
        // Full channel: give a live LoRa task one grace period to drain
        // the previous config before refusing. A timed-out `send` drops
        // the value with the future, so a refusal can never also deliver.
        if with_timeout(CONFIG_DELIVER_WITHIN, config_tx.send(cfg))
            .await
            .is_err()
        {
            log("SER: radio config undeliverable, refused");
            return ConfigDelivery::Undeliverable;
        }
    }
    log("SER: radio config received");
    // Persist what we just applied, so a reset comes back on the
    // host's frequency instead of the compiled default.
    // Non-blocking: the store task does the read-compare-write and
    // skips flash entirely if nothing changed (lnsd re-sends this
    // frame on every connect).
    crate::radio_store::request_save(&wire);
    ConfigDelivery::Applied
}

/// Persist the record this control frame set, and wait until it is on the
/// page — the #358 clause of every answer on the persist path.
///
/// `took_it` is the frame's own accept decision (capability, and delivery
/// where a channel is involved). When it is false nothing was applied, so
/// nothing is owed to flash and no request is made: the answer is already
/// a refusal on an earlier clause, and this outcome is not what decides
/// it.
///
/// The save is requested *here*, on the task that answers the frame,
/// rather than on the main loop that applies the node-side half. Both
/// records used to be persisted from the main loop's arm, unconditionally
/// and with the same value this task already holds — so moving the request
/// changes nothing about what reaches the page, and it is what lets the
/// answer name the outcome of its own write instead of guessing at one it
/// has no handle on.
async fn persist_outcome(
    took_it: bool,
    request: impl FnOnce() -> crate::telemetry::PendingSave,
) -> envelope::Persist {
    if !took_it {
        return envelope::Persist::Durable;
    }
    crate::telemetry::confirm(request()).await
}

/// Incomplete frame timeout. Python uses 100ms but also sets low_latency mode
/// so frames arrive as bulk USB packets. Without low_latency, byte-by-byte USB
/// delivery needs more time. 500ms gives 33x margin for a 167-byte frame at 115200.
const FRAME_TIMEOUT_MS: u64 = 500;

/// Reticulum serial interface task: HDLC-framed bidirectional I/O on USB CDC-ACM.
///
/// Read path: CDC read → HDLC deframe → incoming channel → NodeCore
/// Write path: NodeCore → outgoing channel → HDLC frame → CDC write
#[embassy_executor::task]
async fn retic_serial_task(
    cdc: CdcAcmClass<'static, UsbDriver>,
    incoming_tx: Sender<'static, CriticalSectionRawMutex, Vec<u8>, 8>,
    outgoing_rx: Receiver<'static, CriticalSectionRawMutex, Vec<u8>, 8>,
    config_tx: Sender<'static, CriticalSectionRawMutex, crate::lora::RadioConfig, 1>,
) {
    let mut deframer = Deframer::with_max_frame(SERIAL_HW_MTU);
    let mut read_buf = [0u8; 64];
    let mut frame_buf = Vec::with_capacity(1200);

    // Split so writes can watch the control lines: every write on this
    // port goes through `write_packet_host_gated`, because an unguarded
    // CDC write pends forever once the host stops reading, and a wedged
    // write here takes the read side down with it.
    let (mut tx, mut rx, control) = cdc.split_with_control();

    loop {
        log("SER: wait_connection");
        rx.wait_connection().await;
        log("SER: connected");
        deframer.reset();

        loop {
            // Conditional timeout: 100ms when mid-frame, ~infinite when idle
            let timeout_dur = if deframer.is_in_frame() {
                Duration::from_millis(FRAME_TIMEOUT_MS)
            } else {
                Duration::from_secs(3600)
            };

            match select(
                with_timeout(timeout_dur, rx.read_packet(&mut read_buf)),
                outgoing_rx.receive(),
            )
            .await
            {
                // Read succeeded within timeout
                Either::First(Ok(Ok(n))) => {
                    log_u32("SER: USB read", n as u32);
                    let results = deframer.process(&read_buf[..n]);
                    for r in results {
                        if let DeframeResult::Frame(ref data) = r {
                            match envelope::classify_control_frame(data, ACCEPTED_CONTROL_TYPES) {
                                ControlAction::NotControl => {
                                    log_u32("SER: frame complete", data.len() as u32);
                                    incoming_tx.send(data.clone()).await;
                                }
                                // Host-requested reboot: ACK, let the ACK
                                // drain, then full system reset. The
                                // duty-cycle histogram, radio config and
                                // queues all restart from scratch on the
                                // next boot.
                                action @ (ControlAction::LegacyReset | ControlAction::Reset) => {
                                    crate::log::log_fmt_critical(
                                        "[INFO!] ",
                                        format_args!("[RESET] host-requested reboot"),
                                    );
                                    let acked = if action == ControlAction::LegacyReset {
                                        write_framed(
                                            &mut tx,
                                            &control,
                                            &crate::lora::RESET_ACK,
                                            &mut frame_buf,
                                        )
                                        .await
                                    } else {
                                        let ack = envelope::encode_ack(envelope::TYPE_RESET);
                                        write_framed(&mut tx, &control, &ack, &mut frame_buf).await
                                    };
                                    if !acked {
                                        log("SER: reset ACK write failed");
                                    }
                                    // Give the host time to read the ACK off
                                    // the wire before the USB device
                                    // disappears.
                                    embassy_time::Timer::after(Duration::from_millis(100)).await;
                                    cortex_m::peripheral::SCB::sys_reset();
                                }
                                ControlAction::LegacyRadioConfig(wire) => {
                                    // The legacy contract: ACK on success,
                                    // silence on a config the driver cannot
                                    // take. Audible refusals begin with the
                                    // envelope.
                                    if apply_radio_config(&config_tx, wire).await
                                        == ConfigDelivery::Applied
                                        && !write_framed(
                                            &mut tx,
                                            &control,
                                            &crate::lora::CONFIG_ACK,
                                            &mut frame_buf,
                                        )
                                        .await
                                    {
                                        log("SER: config ACK write failed");
                                    }
                                }
                                ControlAction::RadioConfig(wire) => {
                                    let answer = match apply_radio_config(&config_tx, wire).await {
                                        ConfigDelivery::Applied => {
                                            envelope::encode_ack(envelope::TYPE_RADIO_CONFIG)
                                        }
                                        ConfigDelivery::Invalid => envelope::encode_refusal(
                                            envelope::TYPE_RADIO_CONFIG,
                                            envelope::REFUSE_VALUE,
                                        ),
                                        ConfigDelivery::Undeliverable => envelope::encode_refusal(
                                            envelope::TYPE_RADIO_CONFIG,
                                            envelope::REFUSE_BUSY,
                                        ),
                                    };
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: config answer write failed");
                                    }
                                }
                                ControlAction::LegacyRadioConfigInvalid => {
                                    log("SER: invalid config frame");
                                }
                                ControlAction::WallTime(unix_secs) => {
                                    // The node core owns the calendar: the
                                    // main loop runs the seam and answers via
                                    // the outgoing channel. A full channel
                                    // means an injection is already pending —
                                    // refuse audibly, the host retries.
                                    if WALL_TIME_CHANNEL.try_send(unix_secs).is_err() {
                                        let refusal = envelope::encode_refusal(
                                            envelope::TYPE_WALL_TIME,
                                            envelope::REFUSE_BUSY,
                                        );
                                        if !write_framed(
                                            &mut tx,
                                            &control,
                                            &refusal,
                                            &mut frame_buf,
                                        )
                                        .await
                                        {
                                            log("SER: wall-time refusal write failed");
                                        }
                                    }
                                }
                                ControlAction::TelemetryTarget(target) => {
                                    // The main loop owns the node, so it is
                                    // the one place a target can be looked
                                    // up against the identity store. The
                                    // ack is gated on the binary's declared
                                    // reporter: this shared layer must never
                                    // ack a capability the binary does not
                                    // have. With a reporter, an undelivered
                                    // frame is a full channel — refuse
                                    // audibly, the host retries.
                                    let wired = crate::telemetry::reporter_wired();
                                    let delivered =
                                        wired && crate::telemetry::deliver_target(target);
                                    let persist = persist_outcome(delivered, || {
                                        crate::telemetry::request_save(&target)
                                    })
                                    .await;
                                    let answer = envelope::telemetry_target_answer(
                                        wired, delivered, persist,
                                    );
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: telemetry answer write failed");
                                    }
                                }
                                ControlAction::FixedPosition(position) => {
                                    // Same capability gate as the target:
                                    // only the reporter reads the fixed
                                    // position, so a reporter-less binary
                                    // refuses by name rather than acking
                                    // a pin nothing will ever report.
                                    let wired = crate::telemetry::reporter_wired();
                                    let delivered =
                                        wired && crate::telemetry::deliver_fixed_position(position);
                                    let persist = persist_outcome(delivered, || {
                                        crate::telemetry::request_save_fixed_position(position)
                                    })
                                    .await;
                                    let answer =
                                        envelope::fixed_position_answer(wired, delivered, persist);
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: fixed-position answer write failed");
                                    }
                                }
                                ControlAction::MediaProfile(profile) => {
                                    // Applied here rather than handed to
                                    // the main loop: the apply is two
                                    // atomic stores and a save request,
                                    // and nothing about it needs the
                                    // node. So the report written back is
                                    // the state already in force, not a
                                    // prediction — and `running` differing
                                    // from `configured` in it is the
                                    // board saying "that carrier did not
                                    // come up this boot and cannot be
                                    // started now", which is the honest
                                    // answer where an ack would lie.
                                    let wired = crate::media::media_wired();
                                    let persist =
                                        persist_outcome(wired, || crate::media::apply(profile))
                                            .await;
                                    let answer = envelope::media_profile_answer(
                                        wired,
                                        wired,
                                        persist,
                                        crate::media::running(),
                                        crate::media::configured(),
                                    );
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: media answer write failed");
                                    }
                                }
                                ControlAction::MediaQuery => {
                                    // Read-only, like the radio query:
                                    // safe to send to a board
                                    // mid-measurement, which is exactly
                                    // when a run wants to re-verify what
                                    // the node declared.
                                    let answer = envelope::media_query_answer(
                                        crate::media::media_wired(),
                                        crate::media::running(),
                                        crate::media::configured(),
                                    );
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: media report write failed");
                                    }
                                }
                                ControlAction::PositionSourceQuery => {
                                    // Answered here from the published
                                    // flags rather than by asking the main
                                    // loop: read-only, and both writers of
                                    // the value are one atomic store, so
                                    // what goes back is the state already
                                    // in force. Same capability gate as
                                    // the target frame — a binary with no
                                    // reporter must not answer "no
                                    // position source", which reads as a
                                    // promise that setting one would help.
                                    let answer = envelope::position_source_query_answer(
                                        crate::telemetry::reporter_wired(),
                                        crate::telemetry::position_source_flags(),
                                    );
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: position-source report write failed");
                                    }
                                }
                                ControlAction::NodeName(chosen) => {
                                    // Applied here rather than handed to
                                    // the main loop, like the media
                                    // profile: the apply is one guarded
                                    // store and a save request, and
                                    // nothing about it needs the node. So
                                    // the report written back describes
                                    // the state already in force — the
                                    // mesh name the next announce will
                                    // carry — and its `ble_pending` flag
                                    // is the board saying "the
                                    // advertisement was built with the
                                    // old name and cannot be rebuilt
                                    // before the next reset", which is
                                    // the honest answer where an ack
                                    // would claim both surfaces followed.
                                    let wired = crate::name::name_wired();
                                    // A board that cannot yet state the
                                    // result must not apply the frame
                                    // either, or the host's `busy` retry
                                    // would be a second write of a name
                                    // already stored.
                                    let ready = wired && crate::name::report().is_some();
                                    let persist =
                                        persist_outcome(ready, || crate::name::apply(chosen)).await;
                                    let report = crate::name::report();
                                    let answer = envelope::node_name_answer(
                                        wired,
                                        persist,
                                        report
                                            .as_ref()
                                            .map(|(flags, mesh, ble)| (*flags, mesh, ble)),
                                    );
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: node-name answer write failed");
                                    }
                                }
                                ControlAction::NodeNameQuery => {
                                    // Read-only, like the media query:
                                    // safe to send to a board
                                    // mid-measurement, and the way a host
                                    // learns both effective names without
                                    // having to derive either itself.
                                    let report = crate::name::report();
                                    let answer = envelope::node_name_query_answer(
                                        crate::name::name_wired(),
                                        report
                                            .as_ref()
                                            .map(|(flags, mesh, ble)| (*flags, mesh, ble)),
                                    );
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: node-name report write failed");
                                    }
                                }
                                ControlAction::IdentityQuery => {
                                    // Read-only. `None` is the boot-order
                                    // window before the node published its
                                    // hashes: REFUSE_BUSY, the host
                                    // retries — the node-name query's
                                    // readiness clause.
                                    let report = crate::identity::report();
                                    let answer = envelope::identity_query_answer(report.as_ref());
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: identity report write failed");
                                    }
                                }
                                ControlAction::AnnounceNow => {
                                    // The node core owns the destinations
                                    // and the clock gate, so the main loop
                                    // runs the announce and answers, like
                                    // the wall time. A full channel means
                                    // a request is already pending —
                                    // refuse audibly, the host retries.
                                    if ANNOUNCE_CHANNEL.try_send(()).is_err() {
                                        let refusal = envelope::encode_refusal(
                                            envelope::TYPE_ANNOUNCE,
                                            envelope::REFUSE_BUSY,
                                        );
                                        if !write_framed(
                                            &mut tx,
                                            &control,
                                            &refusal,
                                            &mut frame_buf,
                                        )
                                        .await
                                        {
                                            log("SER: announce refusal write failed");
                                        }
                                    }
                                }
                                ControlAction::BleTxGap(gap_ms) => {
                                    // The BLE interface owns the knob, as
                                    // the LoRa interface owns the transmit
                                    // spacing: the gap is a property of
                                    // the medium, served per connection in
                                    // the drain, and no other layer learns
                                    // of it. One atomic store, already
                                    // bounded by the classifier — nothing
                                    // here can fail.
                                    crate::ble::set_tx_gap_ms(gap_ms);
                                    let answer = envelope::encode_ack(envelope::TYPE_BLE_TX_GAP);
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: ble-tx-gap answer write failed");
                                    }
                                }
                                ControlAction::TxSpacing(spacing_ms) => {
                                    // The LoRa interface owns the knob:
                                    // the gap is a property of the medium
                                    // and is applied at key-up, so the
                                    // value goes straight there and no
                                    // other layer learns of it. A full
                                    // channel means the task has not read
                                    // the previous value yet — refuse
                                    // audibly so a sweep never believes a
                                    // point it did not get.
                                    let answer = if crate::lora::deliver_tx_spacing(spacing_ms) {
                                        envelope::encode_ack(envelope::TYPE_TX_SPACING)
                                    } else {
                                        envelope::encode_refusal(
                                            envelope::TYPE_TX_SPACING,
                                            envelope::REFUSE_BUSY,
                                        )
                                    };
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: tx-spacing answer write failed");
                                    }
                                }
                                ControlAction::CapabilityQuery => {
                                    let report =
                                        envelope::encode_capability_report(ACCEPTED_CONTROL_TYPES);
                                    if !write_framed(&mut tx, &control, &report, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: capability report write failed");
                                    }
                                }
                                ControlAction::RadioQuery => {
                                    // Answered from what the LoRa task
                                    // actually configured, never from the
                                    // flash page or the compiled default:
                                    // those describe the board a reset would
                                    // produce, and a host that reads one and
                                    // writes it back would apply settings
                                    // this board is not on. Before the radio
                                    // has come up there is no honest answer,
                                    // so the query is refused as busy and the
                                    // host retries.
                                    let answer = match crate::lora::running_config() {
                                        Some(wire) => envelope::encode_radio_report(&wire),
                                        None => envelope::encode_refusal(
                                            envelope::TYPE_RADIO_QUERY,
                                            envelope::REFUSE_BUSY,
                                        ),
                                    };
                                    if !write_framed(&mut tx, &control, &answer, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: radio report write failed");
                                    }
                                }
                                ControlAction::Refuse {
                                    refused_type,
                                    reason,
                                } => {
                                    log_u32(
                                        "SER: control frame refused, type",
                                        refused_type as u32,
                                    );
                                    let refusal = envelope::encode_refusal(refused_type, reason);
                                    if !write_framed(&mut tx, &control, &refusal, &mut frame_buf)
                                        .await
                                    {
                                        log("SER: refusal write failed");
                                    }
                                }
                            }
                        } else if matches!(r, DeframeResult::Oversized) {
                            // HW_MTU enforcement lives in the deframer now.
                            log("SER: HW_MTU exceeded, frame discarded");
                        }
                    }
                }
                // USB disconnect
                Either::First(Ok(Err(_))) => {
                    log("SER: USB disconnect");
                    break;
                }
                // Frame timeout
                Either::First(Err(_)) => {
                    if deframer.is_in_frame() {
                        log("SER: frame timeout, reset");
                        deframer.reset();
                    }
                }
                // Outgoing packet to send
                Either::Second(data) => {
                    // Stage 5/6: log first 8 bytes of payload being sent to lnsd
                    let n = data.len().min(8);
                    let mut p8 = [0u8; 8];
                    p8[..n].copy_from_slice(&data[..n]);
                    crate::log::log_fmt(
                        "[T114_SERIAL_TX] ",
                        format_args!(
                            "pkt_hash8={:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x} len={}",
                            p8[0],
                            p8[1],
                            p8[2],
                            p8[3],
                            p8[4],
                            p8[5],
                            p8[6],
                            p8[7],
                            data.len()
                        ),
                    );
                    log_u32("SER: TX", data.len() as u32);
                    frame(&data, &mut frame_buf);
                    log_u32("SER: HDLC framed", frame_buf.len() as u32);
                    // Host-gated like every other write on this port: a
                    // frame queued while no host reads if02 (an announce
                    // after the port was closed) is dropped here instead
                    // of pending forever and deafening the read side.
                    let mut write_ok = true;
                    for chunk in frame_buf.chunks(64) {
                        if !write_packet_host_gated(&mut tx, &control, chunk).await {
                            log("SER: TX dropped, no host reading");
                            write_ok = false;
                            break;
                        }
                    }
                    // ZLP if last chunk was exactly 64 bytes
                    if write_ok && !frame_buf.is_empty() && frame_buf.len() % 64 == 0 {
                        let _ = write_packet_host_gated(&mut tx, &control, &[]).await;
                    }
                }
            }
        }
    }
}
