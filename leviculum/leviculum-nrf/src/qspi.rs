//! The QSPI NOR flash both boards carry and neither has ever driven.
//!
//! 2 MB of MX25R1635F on the T114, 1 MB of IS25LP080D on the WisMesh
//! Pocket V2. Codeberg #384 and
//! `docs/src/concepts/propagation-node-on-a-board.md` ask what to put in
//! it; this module is only the part that gets there — the peripheral, the
//! part's identity, and the `embedded_storage` NorFlash surface the record
//! log ([`leviculum_record_log`]) wants underneath it.
//!
//! # The storage trait shape
//!
//! There is nothing to wrap. `embassy_nrf::qspi::Qspi` already implements
//! `ReadNorFlash`/`NorFlash` (and their async twins) with `ERASE_SIZE`
//! 4096, `WRITE_SIZE` 4 and `READ_SIZE` 4, which is exactly the shape
//! `flash.rs` already uses for the internal NVMC and the SoftDevice flash.
//! So this module builds a `Qspi`, proves it is talking to the part we
//! think it is, and hands it back. Anything that adds a layer here adds
//! flash and a place for a bug.
//!
//! What it does *not* do is give the caller a device before it knows what
//! is on the other end. [`identify_at_boot`] returns `None` on a JEDEC id
//! that is not the board's part, and dropping the `Qspi` deactivates the
//! peripheral: **a board with an unexpected part says so and writes
//! nothing.** That is deliberate — the store's whole safety argument is
//! about 4 KB sectors, a 0xFF erased state and program-once bits, and none
//! of those are true of a part we have not identified.
//!
//! # Why the two frequencies differ
//!
//! `Speed::M8` on the T114 and `Speed::M32` on the Pocket, and the
//! asymmetry is the parts', not ours. The Macronix part is the low-power
//! one: in its default ultra-low-power mode its quad read tops out at
//! 8 MHz. The ISSI part allows 133 MHz, so the nRF52840's own 32 MHz
//! ceiling is what binds there. That is 4 MB/s against 16 MB/s, which the
//! concept paper turns into a 0.5 s full-store scan on the T114 and 0.07 s
//! on the Pocket — the number that makes an on-flash directory affordable
//! and a RAM index unnecessary.
//!
//! # Quad enable
//!
//! Both parts power up in single-line SPI mode with the QE bit of their
//! status register clear, and both put QE in bit 6. The `READ4IO`/`PP4IO`
//! opcodes this driver configures do not work until it is set, so
//! [`identify_at_boot`] sets it if it is not already. The write-enable the
//! part needs first is supplied by the QSPI peripheral itself: every
//! custom instruction it issues has `CINSTRCONF.WREN` set
//! (`custom_instruction_start`, `embassy-nrf-0.9.0/src/qspi.rs`), and
//! `WIPWAIT` makes it wait for the part to finish. One byte of data, so
//! the Macronix configuration registers — which hold the ultra-low-power
//! bit — keep their values.
//!
//! # Deep power down
//!
//! Both parts have a Deep Power Down state in which they answer nothing,
//! drive nothing, and — this is the part that bites — survive a warm
//! reset. Firmware that put the part to sleep once leaves it asleep for
//! every boot after, and the symptom is a JEDEC read that succeeds and
//! returns `00:00:00`: the peripheral clocked the opcode out, and nothing
//! on the bus ever pulled MISO high. A wrong pin map looks the same from
//! the log line, which is why the pins are gated separately
//! (`scripts/check-nrf-board-pins.sh`) and why this boots with a release.
//!
//! So [`identify_at_boot`] sends Release from Deep Power Down (0xAB)
//! before it asks anything, waits out the parts' recovery time, and reads
//! JEDEC. A release sent to an awake part is a no-op, so it is
//! unconditional rather than a flag. A first read that still comes back
//! silent earns exactly one retry, because on the Macronix part it is the
//! CS# pulse and not the opcode that does the releasing (rev. 1.6,
//! §10-24) — so the first read may be the thing that woke it. Both
//! answers go on the log line, `id=` and `id2=`, and one boot then says
//! whether the part is asleep, awake, or not there at all.
//!
//! # The second opinion on `no-answer`
//!
//! `id=00:00:00 id2=00:00:00 state=no-answer` is where that sentence runs
//! out. It says nobody drove MISO — it does not say why, and the two
//! reasons lead to completely different work:
//!
//! | The hand-clocked read says | conclusion |
//! |---|---|
//! | the expected id | the part is there and the QSPI setup does not reach it. That is our bug, and it would be on the other board too. |
//! | all zeros | nothing drives MISO under either driver. The part is absent or unpowered on this unit, and no code fixes that. |
//! | something else | a third thing, and the bytes are the evidence for whatever it is. |
//!
//! So before it gives up, that path drops the peripheral, takes the same
//! pins back as ordinary GPIOs, and asks 0x9F again by hand at 250 kHz
//! (`bitbang_second_opinion` below, shifter in
//! [`leviculum_qspi_bitbang`]).
//! One extra `[QSPI] BITBANG` line, then `None` exactly as before. It
//! reads an identifier and writes nothing — the only opcode the shifter
//! knows is 0x9F, and there is no path from here to one that erases.
//!
//! **Only on that path.** A board whose part answers reaches `state=ok`
//! without a single GPIO write from any of this, and its boot is not a
//! cycle slower.

use embassy_nrf::qspi::{self, Config, Frequency, Qspi};
use embassy_nrf::{bind_interrupts, peripherals, Peri};

use embassy_nrf::gpio::{AnyPin, Flex, OutputDrive, Pull};

bind_interrupts!(pub struct QspiIrqs {
    QSPI => qspi::InterruptHandler<peripherals::QSPI>;
});

/// Read JEDEC ID (opcode 0x9F): manufacturer, memory type, capacity.
const CMD_READ_JEDEC_ID: u8 = 0x9F;
/// Read Status Register (opcode 0x05).
const CMD_READ_STATUS: u8 = 0x05;
/// Write Status Register (opcode 0x01).
const CMD_WRITE_STATUS: u8 = 0x01;
/// Quad Enable, bit 6 of the status register on both parts.
const STATUS_QE: u8 = 0x40;
/// Release from Deep Power Down (opcode 0xAB). `RES` in the Macronix
/// datasheet, `RDPD` in the ISSI one; same opcode, same effect on a
/// sleeping part.
const CMD_RELEASE_DEEP_POWER_DOWN: u8 = 0xAB;

/// Core cycles to wait for a part to come out of Deep Power Down.
///
/// 1 ms at the nRF52840's 64 MHz core, against datasheet recovery times of
/// 35 us and 3 us:
///
/// - MX25R1635F: `tRDP`, "Recovery Time for Release from deep power down
///   mode", 35 us max — Macronix datasheet rev. 1.6 (2018-12-12), Table 17
///   "AC Characteristics".
/// - IS25LP080D: `tRES1`, "Release deep power down", 3 us max. Taken from
///   the IS25LP128 datasheet (ISSI rev. A, Table 9.5 AC characteristics),
///   which is the same IS25LP family and command set; the 080D sheet
///   itself was not reachable from these machines.
///
/// 28x the larger of the two is deliberate. This runs once per boot, so
/// the margin costs nothing measurable, and a value trimmed to the
/// datasheet would buy nothing.
const RELEASE_WAIT_CYCLES: u32 = 64_000;

/// The two bus speeds we use, in a form a `const` board table can hold.
///
/// `embassy_nrf::qspi::Frequency` is neither `Copy` nor constructible out
/// of a `&'static` struct, so the board table carries this and converts on
/// the way in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speed {
    /// 8 MHz — the MX25R1635F's quad-read ceiling in ultra-low-power mode.
    M8,
    /// 32 MHz — the nRF52840 QSPI's own ceiling.
    M32,
}

impl Speed {
    fn frequency(self) -> Frequency {
        match self {
            Speed::M8 => Frequency::M8,
            Speed::M32 => Frequency::M32,
        }
    }

    /// The bus clock in MHz, for the boot log line.
    pub fn mhz(self) -> u32 {
        match self {
            Speed::M8 => 8,
            Speed::M32 => 32,
        }
    }
}

/// The part a board is expected to carry.
pub struct FlashPart {
    /// Datasheet name, for the boot line.
    pub name: &'static str,
    /// The three bytes opcode 0x9F answers with.
    pub jedec: [u8; 3],
    /// Density in bytes. Also the bound `Qspi` enforces on every access.
    pub capacity: u32,
    /// Bus clock this part is driven at.
    pub speed: Speed,
}

/// Macronix MX25R1635F, 16 Mbit, on the Heltec Mesh Node T114.
pub const MX25R1635F: FlashPart = FlashPart {
    name: "MX25R1635F",
    jedec: [0xC2, 0x28, 0x15],
    capacity: 2 * 1024 * 1024,
    speed: Speed::M8,
};

/// ISSI IS25LP080D, 8 Mbit, on the RAK4631 module (WisMesh Pocket V2).
pub const IS25LP080D: FlashPart = FlashPart {
    name: "IS25LP080D",
    jedec: [0x9D, 0x60, 0x14],
    capacity: 1024 * 1024,
    speed: Speed::M32,
};

/// Bring the QSPI up, wake the part, identify it, and hand back a NorFlash
/// device.
///
/// `None` means the part did not answer with the JEDEC id this board
/// expects — `state=no-answer` if it said nothing at all even after the
/// deep-power-down release, `state=unexpected-part` if it named itself and
/// named something else. The `Qspi` is dropped on either path, which
/// deactivates the peripheral and leaves the pins deconfigured, so nothing
/// can write to a part whose geometry we do not know.
///
/// Emits one `[QSPI]` line either way, ungated like `SD_RAM_FLOOR`: a
/// board that comes up with an unexpected part has to say so in a boot
/// capture, before any host has attached to the debug port.
#[allow(clippy::too_many_arguments)]
pub fn identify_at_boot(
    qspi: Peri<'static, peripherals::QSPI>,
    sck: Peri<'static, AnyPin>,
    csn: Peri<'static, AnyPin>,
    io0: Peri<'static, AnyPin>,
    io1: Peri<'static, AnyPin>,
    io2: Peri<'static, AnyPin>,
    io3: Peri<'static, AnyPin>,
    part: &'static FlashPart,
) -> Option<Qspi<'static>> {
    // Second handles on the six pins, for the `no-answer` path alone.
    //
    // SAFETY: the only use is in `bitbang_second_opinion`, and the only
    // caller of that is the branch below, which runs strictly AFTER the
    // `Qspi` built from the originals has been dropped. The two drivers
    // therefore never hold the bus at the same time. `AnyPin` is a byte,
    // so the copies cost nothing on the path that never uses them —
    // taking them here rather than inside the branch is only because
    // `Qspi::new` consumes the originals.
    let spare = unsafe {
        [
            sck.clone_unchecked(),
            csn.clone_unchecked(),
            io0.clone_unchecked(),
            io1.clone_unchecked(),
            io2.clone_unchecked(),
            io3.clone_unchecked(),
        ]
    };

    let mut config = Config::default();
    config.frequency = part.speed.frequency();
    config.capacity = part.capacity;

    // `Qspi::new` drives IO3 (the part's HOLD#/RESET#) high before it
    // activates the interface, which is what the T114 board comment asks
    // for; every pin goes out high first (`config_pin!`,
    // `embassy-nrf-0.9.0/src/qspi.rs`).
    let mut flash = Qspi::new(qspi, QspiIrqs, sck, csn, io0, io1, io2, io3, config);

    // Wake it before asking it anything. A part sleeping in Deep Power
    // Down ignores every other command and rides through a warm reset, so
    // without this a board that was ever put to sleep answers 00:00:00 for
    // the rest of its life. Harmless on a part that is already awake,
    // which is why it is in the boot path and not behind a flag.
    if flash
        .blocking_custom_instruction(CMD_RELEASE_DEEP_POWER_DOWN, &[], &mut [])
        .is_err()
    {
        log_part(part, None, None, false, "release-failed");
        return None;
    }
    cortex_m::asm::delay(RELEASE_WAIT_CYCLES);

    // Custom instructions run on the single-line SPI path regardless of
    // the quad read/write opcodes configured above, so this works before
    // QE is set.
    let first = match read_jedec(&mut flash) {
        Some(id) => id,
        None => {
            log_part(part, None, None, false, "read-failed");
            return None;
        }
    };

    // A silent first answer earns exactly one more read, because on the
    // Macronix part the release is the CS# pulse rather than the opcode
    // ("returns to Stand-by mode if CS# pulses low for tCRDP", rev. 1.6
    // §10-24) and the recovery time runs from that pulse. So the read
    // above may be the thing that woke the part, and the read below is the
    // first one it could have answered. Both go on the log line.
    let second = if first == [0u8; 3] {
        cortex_m::asm::delay(RELEASE_WAIT_CYCLES);
        match read_jedec(&mut flash) {
            Some(id) => Some(id),
            None => {
                log_part(part, Some(first), None, false, "read-failed");
                return None;
            }
        }
    } else {
        None
    };

    let jedec = second.unwrap_or(first);
    if jedec != part.jedec {
        // Two silent reads after a release is not "some other part is
        // fitted" — it is nothing on the bus driving MISO at all, which is
        // a statement about the board rather than about the part number.
        let silent = jedec == [0u8; 3];
        let state = if silent {
            "no-answer"
        } else {
            "unexpected-part"
        };
        log_part(part, Some(first), second, false, state);
        if silent {
            // The peripheral is out of answers; the pins are not. Drop it
            // first — the bit-bang needs the QSPI off the pins, and
            // `Drop` is what deactivates it and deconfigures them.
            drop(flash);
            bitbang_second_opinion(spare);
        }
        return None;
    }

    let mut status = [0u8; 1];
    if flash
        .blocking_custom_instruction(CMD_READ_STATUS, &[], &mut status)
        .is_err()
    {
        log_part(part, Some(first), second, false, "status-read-failed");
        return None;
    }
    if status[0] & STATUS_QE == 0 {
        let want = status[0] | STATUS_QE;
        if flash
            .blocking_custom_instruction(CMD_WRITE_STATUS, &[want], &mut [])
            .is_err()
        {
            log_part(part, Some(first), second, false, "quad-enable-failed");
            return None;
        }
        // Read it back rather than assume: the quad opcodes this driver is
        // configured with are silently wrong if QE did not take, and the
        // symptom would be garbage data rather than an error.
        let mut check = [0u8; 1];
        if flash
            .blocking_custom_instruction(CMD_READ_STATUS, &[], &mut check)
            .is_err()
            || check[0] & STATUS_QE == 0
        {
            log_part(part, Some(first), second, false, "quad-enable-refused");
            return None;
        }
    }

    log_part(part, Some(first), second, true, "ok");
    Some(flash)
}

/// Half a bit-bang clock period, in core cycles at the nRF52840's 64 MHz.
///
/// 128 cycles is 2 us, so a nominal 250 kHz — 32x under the slower
/// candidate part's own single-line ceiling (the MX25R1635F's 8 MHz in
/// ultra-low-power mode) and far under anything about the wiring that
/// could plausibly be marginal. That is the point: this runs once, on a
/// board that has already failed to answer, and a diagnostic that is
/// itself near a timing limit proves nothing. `asm::delay` plus the GPIO
/// writes make the real clock somewhat slower than the nominal figure,
/// which only ever helps here.
const BITBANG_HALF_PERIOD_CYCLES: u32 = 128;

/// The nRF52840 core clock in kHz, for the nominal bit-bang frequency.
const CORE_CLOCK_KHZ: u32 = 64_000;

/// What goes on the `clk_khz=` field: nominal, from the half period above.
const BITBANG_CLK_KHZ: u32 = CORE_CLOCK_KHZ / (2 * BITBANG_HALF_PERIOD_CYCLES);

/// The four pins the hand-clocked read drives, as
/// [`leviculum_qspi_bitbang::Bus`] wants them.
///
/// IO2 and IO3 are not here: they carry no edges, they are held high for
/// the whole transfer by the caller, and giving the shifter the ability to
/// move them would only be a way to get them wrong.
struct GpioBus {
    sck: Flex<'static>,
    csn: Flex<'static>,
    io0: Flex<'static>,
    io1: Flex<'static>,
}

impl leviculum_qspi_bitbang::Bus for GpioBus {
    fn set_sck(&mut self, high: bool) {
        self.sck.set_level(high.into());
    }

    fn set_cs(&mut self, high: bool) {
        self.csn.set_level(high.into());
    }

    fn set_io0(&mut self, high: bool) {
        self.io0.set_level(high.into());
    }

    fn read_io1(&mut self) -> bool {
        self.io1.is_high()
    }

    fn settle(&mut self) {
        cortex_m::asm::delay(BITBANG_HALF_PERIOD_CYCLES);
    }
}

/// Ask the pins directly, once, and say what they answered.
///
/// Only reached from the `state=no-answer` branch of [`identify_at_boot`],
/// and only after the `Qspi` is dropped. One read of opcode 0x9F at
/// [`BITBANG_CLK_KHZ`], one `[QSPI] BITBANG` line, no retry and no second
/// opcode: the caller returns `None` either way, so a ladder of attempts
/// would produce more lines and no more information than the first.
///
/// # Reading the answer
///
/// See the table in the module docs. Two details of the wiring below feed
/// into it and are worth having in front of you:
///
/// - IO1 is read with a **pull-down**. Without one, an undriven wire is a
///   floating input and the bytes would be noise rather than evidence, so
///   `00:00:00` has to be made to mean something: it means the wire never
///   left the level the nRF's own internal pull put it at. A part that is
///   present drives IO1 push-pull and wins against a pull of that order
///   easily, so it cannot suppress a real answer — which also
///   makes `ff:ff:ff` a genuine "something else" here and not the
///   idle reading it would be under a pull-up.
/// - IO2 and IO3 are driven high for the whole transfer. They are the
///   part's WP# and HOLD#, and a part with HOLD# low suspends the transfer
///   and answers nothing. The QSPI peripheral was doing this implicitly
///   through `CINSTRCONF.LIO2`/`LIO3`
///   (`embassy-nrf-0.9.0/src/qspi.rs:290`); by hand it is explicit.
///
/// One ambiguity this does not resolve, and should not be read past: a
/// part still in Deep Power Down answers nothing here either, because the
/// only opcode it would honour is 0xAB and this sends 0x9F. That is not
/// worth a second opcode. `identify_at_boot` already sent 0xAB and pulsed
/// CS# low three times before reaching this branch, so on any board where
/// the QSPI reaches the part the part is awake — and on a board where it
/// does not, the interesting row of the table is the one where the
/// hand-clocked read *succeeds*, which no amount of sleep can fake.
///
/// # What it leaves behind
///
/// Exactly the pin states `Qspi::drop` left: SCK, IO0..IO3 disconnected
/// (`Flex::drop` writes the same `PIN_CNF` that `gpio::deconfigure_pin`
/// does), and CS# still an output driven high. That last one is not an
/// oversight in either place — embassy leaves CSN driven on purpose, so a
/// part in Deep Power Down does not read a floating CS# as a select and
/// wake up on its own — so this path restores it rather than "cleaning it
/// up" into a state its caller never had. A later `Qspi::new` on these
/// pins therefore starts from the same place it would have without this
/// function.
fn bitbang_second_opinion(pins: [Peri<'static, AnyPin>; 6]) {
    let [sck, csn, io0, io1, io2, io3] = pins;

    // WP# and HOLD# first, before anything can be selected.
    let mut wp = Flex::new(io2);
    wp.set_high();
    wp.set_as_output(OutputDrive::HighDrive);
    let mut hold = Flex::new(io3);
    hold.set_high();
    hold.set_as_output(OutputDrive::HighDrive);

    // CS# is already an output driven high, so setting the level before
    // the direction keeps it that way through the handover: no glitch a
    // sleeping part could read as a select.
    let mut csn = Flex::new(csn);
    csn.set_high();
    csn.set_as_output(OutputDrive::HighDrive);

    let mut sck = Flex::new(sck);
    sck.set_low();
    sck.set_as_output(OutputDrive::HighDrive);

    let mut io0 = Flex::new(io0);
    io0.set_low();
    io0.set_as_output(OutputDrive::HighDrive);

    let mut io1 = Flex::new(io1);
    io1.set_as_input(Pull::Down);

    let mut bus = GpioBus { sck, csn, io0, io1 };
    let id = leviculum_qspi_bitbang::read_jedec_id(&mut bus);

    let GpioBus { sck, csn, io0, io1 } = bus;
    // Give the pins back. Dropping a `Flex` disconnects it, which is what
    // `Qspi::drop` did to these five; CS# is the exception it deliberately
    // left driven, so it is the one that persists.
    drop(sck);
    drop(io0);
    drop(io1);
    drop(wp);
    drop(hold);
    csn.persist();

    crate::log::log_fmt_critical(
        "[QSPI] ",
        format_args!(
            "BITBANG id={} clk_khz={}",
            JedecId(Some(id)),
            BITBANG_CLK_KHZ
        ),
    );
}

/// One JEDEC id read (opcode 0x9F). `None` is a transaction the peripheral
/// refused; three zero bytes are a transaction that worked and found
/// nobody driving the bus.
fn read_jedec(flash: &mut Qspi<'static>) -> Option<[u8; 3]> {
    let mut jedec = [0u8; 3];
    flash
        .blocking_custom_instruction(CMD_READ_JEDEC_ID, &[], &mut jedec)
        .ok()?;
    Some(jedec)
}

/// A JEDEC id on the boot line: `c2:28:15`, or `none` for a read that was
/// never made — the second read only happens when the first was silent,
/// and a release that fails means neither did.
struct JedecId(Option<[u8; 3]>);

impl core::fmt::Display for JedecId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(id) => write!(f, "{:02x}:{:02x}:{:02x}", id[0], id[1], id[2]),
            None => f.write_str("none"),
        }
    }
}

/// The one boot line. Same shape as `SD_RAM_FLOOR`: every value that went
/// into the verdict is on it, so the verdict is checkable from a capture.
///
/// `id` is the read after the deep-power-down release, `id2` the retry it
/// earns by being silent. `id2=none` therefore means the first read was
/// answered — the part was awake, or the release woke it — and anything
/// else means the first read was not, so a capture says which of the two
/// spoke without anyone having to know the sequence.
fn log_part(
    part: &FlashPart,
    first: Option<[u8; 3]>,
    second: Option<[u8; 3]>,
    matched: bool,
    state: &str,
) {
    crate::log::log_fmt_critical(
        "[QSPI] ",
        format_args!(
            "JEDEC id={} id2={} expect={} part={} bytes={} clk={}MHz match={} state={}",
            JedecId(first),
            JedecId(second),
            JedecId(Some(part.jedec)),
            part.name,
            part.capacity,
            part.speed.mhz(),
            u8::from(matched),
            state,
        ),
    );
}

/// Bytes of the part read back and summarised by [`log_head`].
const HEAD_PROBE_LEN: usize = 256;

/// A 4-byte-aligned read buffer. The QSPI peripheral DMAs into it and
/// asserts `ptr % 4 == 0` (`start_read`, `embassy-nrf-0.9.0/src/qspi.rs`).
#[repr(align(4))]
struct Aligned<const N: usize>([u8; N]);

/// Read the first 256 bytes over the quad path and say what came back.
///
/// This is the only thing in part 1 that exercises `READ4IO` at the
/// configured clock, so it is what a boot capture has to show before the
/// bus is believed. It is also the honest answer to "is there already
/// something on these boards' flash?", which matters before anything
/// formats them: `nonff=0/256` is a blank part (or a bus that answers with
/// pull-ups — the two look alike, which is why the JEDEC line above is the
/// separate proof that a part is there at all), and anything else is data
/// somebody wrote.
pub fn log_head(flash: &mut Qspi<'static>) {
    let mut buf = Aligned([0u8; HEAD_PROBE_LEN]);
    if flash.blocking_read(0, &mut buf.0).is_err() {
        crate::log::log_fmt_critical("[QSPI] ", format_args!("HEAD state=read-failed"));
        return;
    }
    // FNV-1a over the window: a number a capture can be diffed on, not a
    // cryptographic claim.
    let mut digest: u32 = 0x811C_9DC5;
    let mut nonff = 0u32;
    for byte in buf.0.iter() {
        digest = (digest ^ u32::from(*byte)).wrapping_mul(0x0100_0193);
        if *byte != 0xFF {
            nonff += 1;
        }
    }
    crate::log::log_fmt_critical(
        "[QSPI] ",
        format_args!(
            "HEAD fnv1a={digest:08x} nonff={nonff}/{HEAD_PROBE_LEN} b0={:02x}{:02x}{:02x}{:02x}",
            buf.0[0], buf.0[1], buf.0[2], buf.0[3]
        ),
    );
}

/// Mount the record log over the whole part, read-only, and say what is
/// there.
///
/// **Read-only on purpose.** These boards have carried other people's
/// firmware, and part 1 of #384 has no business erasing whatever that left
/// behind; `RecordLog::mount` returns `None` on a region it did not write
/// rather than formatting it. Formatting is a decision for the batch that
/// actually stores something.
///
/// Consumes the device: nothing in part 1 keeps the QSPI, and dropping it
/// deactivates the peripheral (and, on the way out, works around
/// nRF52840 anomaly 122) instead of holding the part out of its standby
/// current for a store no one has mounted yet.
pub fn log_store(flash: Qspi<'static>, part: &FlashPart) {
    use leviculum_record_log::{RecordLog, SECTOR_SIZE};

    let sectors = part.capacity / SECTOR_SIZE;
    match RecordLog::mount(flash, 0, part.capacity) {
        Ok(Some(mut log)) => {
            let active = log.active_sector();
            let seq = log.sequence();
            match log.count() {
                Ok((live, purged)) => crate::log::log_fmt_critical(
                    "[QSPI] ",
                    format_args!(
                        "STORE state=mounted sectors={sectors} active={active} seq={seq} \
                         live={live} purged={purged}"
                    ),
                ),
                Err(_) => crate::log::log_fmt_critical(
                    "[QSPI] ",
                    format_args!("STORE state=scan-failed sectors={sectors} active={active}"),
                ),
            }
        }
        Ok(None) => crate::log::log_fmt_critical(
            "[QSPI] ",
            format_args!("STORE state=unformatted sectors={sectors} note=this-batch-never-formats"),
        ),
        Err(_) => crate::log::log_fmt_critical(
            "[QSPI] ",
            format_args!("STORE state=mount-failed sectors={sectors}"),
        ),
    }
}
