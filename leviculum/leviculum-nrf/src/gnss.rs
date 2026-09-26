//! GNSS driver task: baud sweep, presence tri-state, NMEA fold.
//!
//! One task, two boards, two receivers. UARTE0 on P0.15/P0.16 carries
//! the WisMesh Pocket V2's u-blox ZOE-M8Q (RAK19026 VC baseboard); on
//! P1.07/P1.05 it carries the Heltec Mesh Node T114's Quectel L76K
//! (Codeberg #69). Everything below the module-init step is identical
//! for both, because nothing in it is module-specific: a baud sweep, a
//! presence tri-state and an NMEA fold work on sentences, and sentences
//! are sentences. The two differences are named in [`GnssWiring`] — which
//! pins, and which [`ModuleKind`] the one-shot init speaks.
//!
//! The `gnss` cargo feature only says the board routes this UART; whether a
//! receiver is attached and delivering is a runtime question with three
//! answers (Codeberg #240), owned by the pure
//! [`leviculum_gnss_presence::PresenceMachine`]:
//!
//! - the machine sweeps 9600 → 38400 → 115200 until a checksum-clean
//!   sentence locks a baud (so a non-default module still works),
//! - publishes `no-hardware` / `no-fix` / `fix` through
//!   [`GNSS_PRESENCE`] with Fix→NoFix hysteresis,
//! - and forwards parsed RMC/GGA content, which this task folds into
//!   the [`GNSS_FIX`] snapshot exactly as before.
//!
//! The machine also aggregates GSV groups into satellites-in-view and
//! best C/N0 (#324); this task only prints them in the heartbeat as
//! `sv=` / `cno=`. They are the instrument for config-versus-antenna:
//! `sat=` is GGA satellites-in-USE and is 0 until a fix exists, so an
//! open-sky session that stays at `valid=false sv=0 cno=0` indicts the
//! RF path, while many SVs at 35-45 dBHz without a fix indicts
//! configuration instead.
//!
//! This task is deliberately a thin driver: reads via
//! `read_until_idle` (TIMER1 + PPI ch0/ch1 detect the line going quiet
//! between 1 Hz NMEA bursts, so chunks align with sentence boundaries
//! and nothing is lost between reads), a 1 s read timeout to carry
//! time into the machine when the line is silent, and UART
//! reconfiguration when the machine asks for a new baud. All policy —
//! window lengths, sweep order, hysteresis hold — lives host-tested in
//! the pure crate.
//!
//! One-shot module init (#324 on the V2, #69 on the T114): after the
//! first baud lock the task walks the sequence its board's
//! [`leviculum_gnss_init::ModuleInit`] hands out — UBX factory clear /
//! full power / antenna supply on the ZOE-M8Q, the `$PCAS` probe /
//! constellations / sentence selection / navigation mode on the L76K —
//! so a persisted Meshtastic-era module configuration cannot survive
//! into our runtime, and acquisition never depends on a field module's
//! factory defaults. Both sequences are derived from the Meshtastic
//! reference (`meshtastic/src/gps/ubx.h`, `GPS.cpp`) and live
//! host-tested in the pure crate with the citations; this task only
//! writes the frames and logs `[GNSS_INIT]` lines. Neither sequence
//! contains a reset or a baud command: a forced cold start would wipe
//! the module's assistance data on every boot, and a baud command would
//! desynchronise the line the driver is talking on. The UBX clear does
//! reset the module's I/O system, after which it may fall back to its
//! default baud — the presence machine's sentence-starvation re-sweep
//! recovers the line, no special-casing here.
//!
//! Two things that are not sentences and therefore live here rather
//! than in the pure crate: the module's **standby pin**, held awake for
//! the life of the task on a board that has one (the L76K parks in
//! low power otherwise — the same #324 lesson, one layer down), and the
//! **shared supply rail**, which the task waits out rather than
//! switching, because on the T114 the display sits on it too
//! ([`crate::vext`]).
//!
//! The PPS pin (P0.17 on the V2, P1.04 on the T114) is configured as a
//! pull-down input but not used — reserved for a future
//! timestamp-capture iteration.

use embassy_executor::Spawner;
use embassy_nrf::gpio::{AnyPin, Input, Level, OutputDrive, Pull};
use embassy_nrf::peripherals;
use embassy_nrf::uarte::{self, Uarte};
use embassy_nrf::{bind_interrupts, Peri};
use embassy_time::{with_timeout, Duration, Instant, Timer};

use leviculum_gnss_init::{L76kInit, ModuleInit, UbxInit};
use leviculum_gnss_presence::{Output, PresenceMachine};

use crate::baseboard::{GnssFix, GnssPresenceState, GNSS_FIX, GNSS_PRESENCE};

/// Which receiver is on the other end of the UART, and therefore which
/// command language the one-shot boot init speaks. The board knows; the
/// rest of the driver does not care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleKind {
    /// u-blox ZOE-M8Q (WisMesh Pocket V2), UBX binary protocol.
    UbloxM8,
    /// Quectel L76K (Heltec Mesh Node T114), CASIC `$PCAS` sentences.
    QuectelL76k,
}

/// The board-specific half of the driver: which peripherals and pins
/// carry the receiver, and which module language it speaks.
pub struct GnssWiring {
    pub uarte: Peri<'static, peripherals::UARTE0>,
    /// Idle-line detection for `read_until_idle`.
    pub timer: Peri<'static, peripherals::TIMER1>,
    /// RXDRDY → timer clear/start.
    pub ppi_a: Peri<'static, peripherals::PPI_CH0>,
    /// Timer compare → RX stop.
    pub ppi_b: Peri<'static, peripherals::PPI_CH1>,
    /// RX at the MCU: the module's TX line.
    pub rx: Peri<'static, AnyPin>,
    /// TX at the MCU: the module's RX line.
    pub tx: Peri<'static, AnyPin>,
    /// PPS, configured and held but not used.
    pub pps: Peri<'static, AnyPin>,
    /// Standby control, driven HIGH for the life of the task to keep the
    /// module awake (L76K: LOW would allow sleep). `None` on a board
    /// whose receiver has no such pin.
    pub standby: Option<Peri<'static, AnyPin>>,
    pub module: ModuleKind,
}

/// The board's boot init sequencer. An enum rather than a `dyn` object
/// so the state lives in the task's own frame; the delegation is what
/// makes [`ModuleInit`] the single surface the loop below drives.
enum Init {
    Ubx(UbxInit),
    L76k(L76kInit),
}

impl Init {
    fn new(module: ModuleKind) -> Self {
        match module {
            ModuleKind::UbloxM8 => Init::Ubx(UbxInit::new()),
            ModuleKind::QuectelL76k => Init::L76k(L76kInit::new()),
        }
    }

    fn as_mut(&mut self) -> &mut dyn ModuleInit {
        match self {
            Init::Ubx(i) => i,
            Init::L76k(i) => i,
        }
    }
}

/// Act on the init sequencer's outputs: stage the frame into RAM (UBX
/// frames are flash consts, EasyDMA reads RAM only) and write it, then
/// log the banner-class `[GNSS_INIT]` line the board smoke check greps
/// for. TX errors are logged, never retried — the sequencer's ACK
/// handling makes a lost frame visible as `ack=timeout`.
async fn apply_init_output(output: leviculum_gnss_init::Output, uart_tx: &mut uarte::UarteTx<'_>) {
    match output {
        leviculum_gnss_init::Output::Send { step, frame } => {
            let mut staged = [0u8; leviculum_gnss_init::MAX_FRAME];
            staged[..frame.len()].copy_from_slice(frame);
            let verb = if uart_tx.write(&staged[..frame.len()]).await.is_ok() {
                "sent"
            } else {
                "tx-error"
            };
            crate::log::log_fmt_critical(
                "[INFO!] ",
                format_args!("[GNSS_INIT] step={} {}", step, verb),
            );
        }
        leviculum_gnss_init::Output::AckResult { step, outcome } => {
            crate::log::log_fmt_critical(
                "[INFO!] ",
                format_args!("[GNSS_INIT] step={} ack={}", step, outcome.as_str()),
            );
        }
    }
}

/// Convert nmea0183's positive-magnitude `Latitude` to signed decimal
/// degrees (negative south).
fn lat_to_f64(lat: &nmea0183::coords::Latitude) -> f64 {
    let mag = lat.as_f64();
    match lat.hemisphere {
        nmea0183::coords::Hemisphere::South => -mag,
        _ => mag,
    }
}

/// Convert nmea0183's positive-magnitude `Longitude` to signed decimal
/// degrees (negative west).
fn lon_to_f64(lon: &nmea0183::coords::Longitude) -> f64 {
    let mag = lon.as_f64();
    match lon.hemisphere {
        nmea0183::coords::Hemisphere::West => -mag,
        _ => mag,
    }
}

bind_interrupts!(pub struct GnssIrqs {
    UARTE0 => uarte::InterruptHandler<peripherals::UARTE0>;
});

/// Map a sweep baud to the UARTE register value. The machine only emits
/// values from `BAUD_SWEEP`; the catch-all keeps this total without an
/// `unwrap`.
fn baudrate_of(baud: u32) -> uarte::Baudrate {
    match baud {
        38_400 => uarte::Baudrate::BAUD38400,
        115_200 => uarte::Baudrate::BAUD115200,
        _ => uarte::Baudrate::BAUD9600,
    }
}

/// Act on one machine output: publish a transition (watch + debug
/// event), record a requested baud change, or fold RMC/GGA content into
/// the `GnssFix` snapshot (the fold is byte-for-byte the pre-#240
/// behaviour — `unix_secs` exists only while a valid RMC does, so the
/// #166 seed gate stays keyed to valid RMC only).
fn apply_output(output: Output, latest: &mut GnssFix, pending_baud: &mut Option<u32>) {
    let sender = GNSS_FIX.sender();
    match output {
        Output::Transition { state, baud } => {
            GNSS_PRESENCE
                .sender()
                .send(GnssPresenceState { state, baud });
            // Banner-class state event, same replay semantics as
            // `[TIME_SOURCE]`: bypasses the runtime-drain gate, last
            // line wins. Rate is bounded by the hysteresis hold and
            // duplicate-suppression in the machine.
            crate::log::log_fmt_critical(
                "[INFO!] ",
                format_args!("[GNSS_PRESENCE] state={} baud={}", state.as_str(), baud),
            );
        }
        Output::SetBaud(baud) => *pending_baud = Some(baud),
        Output::Rmc(rmc) => {
            latest.valid = rmc.mode.is_valid();
            if latest.valid {
                latest.latitude = Some(lat_to_f64(&rmc.latitude));
                latest.longitude = Some(lon_to_f64(&rmc.longitude));
                // RMC UTC is already leap-second-corrected by
                // the receiver — converted as-is, never via
                // raw GPS time (#166, time-and-clocks.md).
                latest.unix_secs = leviculum_gnss_time::unix_secs_from_rmc_utc(&rmc.datetime);
                // Movement, for the telemetry location sensor (#236).
                // Course is absent while stationary on most receivers, so
                // it is carried as absent rather than as zero.
                latest.speed_mps = Some(rmc.speed.as_mps());
                latest.bearing_deg = rmc.course.map(|c| c.degrees);
            } else {
                // A stale time claim must not outlive the fix
                // that made it: position keeps last-good for
                // the display, time does not.
                latest.unix_secs = None;
            }
            sender.send(*latest);
        }
        Output::Gga(gga) => {
            latest.sat_in_use = gga.sat_in_use;
            let fix = !matches!(gga.gps_quality, nmea0183::GPSQuality::NoFix);
            latest.valid = fix;
            if fix {
                latest.latitude = Some(lat_to_f64(&gga.latitude));
                latest.longitude = Some(lon_to_f64(&gga.longitude));
                latest.altitude_m = gga.altitude.map(|a| a.meters);
                // The accuracy gate's input (#236). Kept with position
                // rather than cleared with time: a stale HDOP mis-scales a
                // position that is itself stale, which is the same class
                // of error, not a new one.
                latest.hdop = Some(gga.hdop);
            }
            sender.send(*latest);
        }
    }
}

/// Pump UART bytes through the presence machine; publish presence
/// transitions via `GNSS_PRESENCE` and fix snapshots via `GNSS_FIX`.
#[embassy_executor::task]
pub async fn gnss_task(wiring: GnssWiring) {
    let GnssWiring {
        mut uarte,
        mut timer,
        mut ppi_a,
        mut ppi_b,
        mut rx,
        mut tx,
        pps,
        standby,
        module,
    } = wiring;

    // Hold the PPS pin low-impedance enough that no spurious capture fires
    // before we wire it up. Drop returns it to its reset state on task exit
    // (which never happens for this task, but the convention is clear).
    let _pps = Input::new(pps, Pull::Down);

    // Force the module awake and keep it that way. A receiver parked in
    // standby by whatever firmware ran before ours streams nothing, and
    // the presence machine would report `no-hardware` on perfectly good
    // wiring — the #324 failure mode with a different cause. Held for
    // the life of the task: dropping it returns the pin to its reset
    // state, which on the T114 is "sleep allowed".
    let _standby = standby.map(|pin| {
        crate::log::log_fmt("[GNSS] ", format_args!("standby pin held high (wake)"));
        embassy_nrf::gpio::Output::new(pin, Level::High, OutputDrive::Standard)
    });

    // The receiver's supply may be a rail the binary raised for several
    // peripherals at once (T114: VEXT, shared with the display). Waiting
    // for its warmup is free on a board without one.
    crate::vext::wait_ready().await;

    let mut machine = PresenceMachine::new(Instant::now().as_millis());

    // One-shot module init (#324, #69): constructed once per boot,
    // silent until the machine locks a baud, silent again forever once
    // done.
    let mut init = Init::new(module);

    // Rolling GnssFix snapshot across sentences. RMC owns "is the
    // receiver happy" (mode is_valid()); GGA owns "how many sats".
    let mut latest = GnssFix::empty();
    let mut pending_baud: Option<u32> = None;

    let mut bytes_total: u32 = 0;
    let mut uart_errors: u32 = 0;
    let mut last_health_log = Instant::now();

    let mut configured_baud = machine.current_baud();
    crate::log::log_fmt(
        "[GNSS] ",
        format_args!("UARTE0 up, sweep start @ {} 8N1", configured_baud),
    );

    // Outer loop: one iteration per UART configuration. The machine
    // requests baud changes through `Output::SetBaud`; recreating the
    // Uarte from reborrowed peripherals is the supported embassy way to
    // change the baud rate (and re-derives the idle timeout from it).
    loop {
        let mut config = uarte::Config::default();
        config.baudrate = baudrate_of(configured_baud);
        let uart = Uarte::new(
            uarte.reborrow(),
            rx.reborrow(),
            tx.reborrow(),
            GnssIrqs,
            config,
        );
        let (mut uart_tx, mut uart_rx) =
            uart.split_with_idle(timer.reborrow(), ppi_a.reborrow(), ppi_b.reborrow());

        // 256-byte chunk: a full 1 Hz NMEA burst (RMC+GGA+GSA — the
        // GSV tail may split) fits, and the idle detector ends each
        // read at the burst boundary anyway. The 1 s timeout exists to
        // carry time into the machine on a silent line (detection
        // windows, fix hold); when data flows, idle completes the read
        // long before it fires, so the cancel-loses-bytes race is
        // confined to near-silent lines where there is nothing to lose.
        let mut buf = [0u8; 256];
        let new_baud = loop {
            let now_ms = Instant::now().as_millis();
            let mut chunk_len = 0usize;
            match with_timeout(Duration::from_secs(1), uart_rx.read_until_idle(&mut buf)).await {
                Ok(Ok(n)) => {
                    bytes_total = bytes_total.saturating_add(n as u32);
                    chunk_len = n;
                    machine.on_bytes(&buf[..n], now_ms, &mut |o| {
                        apply_output(o, &mut latest, &mut pending_baud)
                    });
                }
                Ok(Err(_e)) => {
                    // Framing/overrun. At a wrong sweep baud this can be
                    // every chunk — count it (the heartbeat reports it)
                    // instead of logging per error, and give the EasyDMA
                    // a moment instead of hot-looping.
                    uart_errors = uart_errors.saturating_add(1);
                    machine.on_uart_error(now_ms, &mut |o| {
                        apply_output(o, &mut latest, &mut pending_baud)
                    });
                    Timer::after(Duration::from_millis(50)).await;
                }
                Err(_timeout) => {
                    machine.poll(now_ms, &mut |o| {
                        apply_output(o, &mut latest, &mut pending_baud)
                    });
                }
            }

            // Heartbeat log every 5 s with cumulative counters. Keeps
            // the debug log readable but proves the GNSS pipe is alive.
            // `sat=` is GGA satellites-in-USE and stays 0 without a fix;
            // `sv=`/`cno=` are the GSV in-VIEW count and best C/N0 in
            // dBHz (#324) and move as soon as the antenna hears anything
            // — that is what separates a config problem from a weak RF
            // path while `valid=false`. `cno=0` means "no C/N0 reported
            // yet", which a tracked satellite never is.
            if last_health_log.elapsed().as_secs() >= 5 {
                crate::log::log_fmt(
                    "[GNSS] ",
                    format_args!(
                        "bytes={} sentences={} errs={} valid={} sat={} sv={} cno={} baud={}",
                        bytes_total,
                        machine.sentences_seen(),
                        uart_errors,
                        latest.valid,
                        latest.sat_in_use,
                        machine.sv_in_view(),
                        machine.cno_best().unwrap_or(0),
                        configured_baud,
                    ),
                );
                last_health_log = Instant::now();
            }

            if let Some(b) = pending_baud.take() {
                if b != configured_baud {
                    break b;
                }
            }

            // Init runs strictly after the baud decision above, so a
            // frame is only ever written at the rate the machine locked
            // (a UBX frame at the wrong baud is garbage into the
            // module). The emit callback cannot await, so outputs are
            // staged and applied afterwards; the sequencer emits at
            // most one Send and one AckResult per iteration.
            let mut staged: [Option<leviculum_gnss_init::Output>; 4] = [None; 4];
            let mut staged_n = 0usize;
            {
                let mut emit = |o: leviculum_gnss_init::Output| {
                    if staged_n < staged.len() {
                        staged[staged_n] = Some(o);
                        staged_n += 1;
                    }
                };
                let init_now = Instant::now().as_millis();
                let init = init.as_mut();
                init.on_bytes(&buf[..chunk_len], init_now, &mut emit);
                init.poll(
                    machine.locked(),
                    machine.sentences_seen(),
                    init_now,
                    &mut emit,
                );
            }
            for slot in staged.iter_mut().take(staged_n) {
                if let Some(o) = slot.take() {
                    apply_init_output(o, &mut uart_tx).await;
                }
            }
        };
        configured_baud = new_baud;
    }
}

/// Convenience wrapper invoked from the bin file.
pub fn init(spawner: &Spawner, wiring: GnssWiring) {
    spawner.must_spawn(gnss_task(wiring));
}
