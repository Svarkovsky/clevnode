//! Blind-driven ST7789 status display on the Heltec Mesh Node T114.
//!
//! The optional T114 TFT (ST7789, 240×135, SPI) is WRITE-ONLY — MISO is
//! not connected on the board. Presence detection is therefore physically
//! impossible, and BOTH references drive the panel blind, unconditionally:
//! Meshtastic (`variants/nrf52840/heltec_mesh_node_t114/variant.h`,
//! `ST7789_MISO -1`) and the RNode firmware (`Boards.h` T114 block,
//! `PIN_T114_TFT_MISO 11 // not connected`). We do the same: one t114 UF2
//! serves panel-equipped and panel-less boards alike. On a board without
//! the panel every SPI write goes nowhere, harmlessly — the task keeps
//! painting at 1 Hz either way.
//!
//! Pin double-reference (nRF port.pin — Meshtastic variant.h uses Arduino
//! numbering 32+n, RNode Boards.h the bare variant index; same physical
//! pins):
//!
//! | signal            | nRF   | Meshtastic         | RNode                 |
//! |-------------------|-------|--------------------|-----------------------|
//! | CS                | P0.11 | ST7789_NSS 11      | PIN_T114_TFT_SS 11    |
//! | DC                | P0.12 | ST7789_RS 12       | PIN_T114_TFT_DC 12    |
//! | MOSI              | P1.09 | ST7789_SDA 41      | PIN_T114_TFT_MOSI 9   |
//! | SCK               | P1.08 | ST7789_SCK 40      | PIN_T114_TFT_SCK 8    |
//! | RST               | P0.02 | ST7789_RESET 2     | PIN_T114_TFT_RST 2    |
//! | VTFT_CTRL (rail)  | P0.03 | VTFT_CTRL 3, LOW=on| PIN_T114_TFT_EN 3, LOW|
//! | VTFT_LEDA (backl.)| P0.15 | VTFT_LEDA 15, LOW=on| PIN_T114_TFT_BLGT 15 |
//! | VEXT (supply)     | P0.21 | VEXT_ENABLE 21, HIGH| PIN_VEXT_EN 21, HIGH |
//!
//! Both references raise VEXT at boot ("turn on the display power",
//! Meshtastic `main.cpp:420-422`) and the TFT chain hangs off that rail
//! — but so does the L76K GNSS receiver (#69), so the rail is not this
//! task's to own. The binary raises it before spawning anything that
//! sits on it and this task waits out the warmup with
//! [`crate::vext::wait_ready`]; the VTFT rail and the backlight, which
//! are the panel's alone, stay on for as long as the task runs.
//!
//! Rendering: the shared painter (`leviculum-screen`) draws the same
//! status frame the Pocket V2 shows into a 120×67 mono framebuffer; this
//! backend diffs it against the last-pushed frame (double buffer) and
//! sends only the changed region over SPI, scaled 2× with white mapped to
//! muted yellow (Meshtastic's T114 text colour, COLOR565(255,255,128)) —
//! full-frame pushes are RNode's `display()` flicker-and-airtime lesson.
//!
//! Init sequence double-referenced from `reference/RNode_Firmware/
//! ST7789.h` (reset pulse 1 ms high / 10 ms low / high; SWRESET, SLPOUT,
//! COLMOD 16-bit, INVON, NORON, DISPON; landscape MADCTL = RGB|MV|MX at
//! `:245`) and the `mipidsi` crate's ST7789 model (same command set).
//! SPI bus: SPI3 — unclaimed on T114 (the radio owns SPI2), outside the
//! SoftDevice's reserved set, and its known T114 MISO-read bug is
//! irrelevant on a TX-only bus.
//!
//! Hardware-blind residual (Lew's boards have no panel; only wiring and
//! init are unverifiable here, both double-referenced): the exact row
//! offset of the 135-line window inside the controller's 240-line axis is
//! 52 or 53 depending on the panel's mounting mirror; we use 53 (the
//! widely-validated Adafruit rotation-1 value). A wrong guess shifts the
//! image by one row into an area we clear to black at init — cosmetic at
//! worst.

use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::peripherals;
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::{bind_interrupts, Peri};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use leviculum_screen::{
    ident_short, BatteryStatus, DirtyRect, FrameKey, GnssStatus, StatusModel, T114Fb,
};

bind_interrupts!(pub struct TftIrqs {
    SPIM3 => spim::InterruptHandler<peripherals::SPI3>;
});

// ST77xx command opcodes (reference/RNode_Firmware/ST7789.h:40-63).
const CMD_SWRESET: u8 = 0x01;
const CMD_SLPOUT: u8 = 0x11;
const CMD_NORON: u8 = 0x13;
const CMD_INVON: u8 = 0x21;
const CMD_DISPON: u8 = 0x29;
const CMD_CASET: u8 = 0x2A;
const CMD_RASET: u8 = 0x2B;
const CMD_RAMWR: u8 = 0x2C;
const CMD_MADCTL: u8 = 0x36;
const CMD_COLMOD: u8 = 0x3A;

/// Landscape: RGB|MV|MX (RNode ST7789.h:245 `resetOrientation`).
const MADCTL_LANDSCAPE: u8 = 0x60;

/// Mono framebuffer geometry: 240×135 panel at scale 2 (the 135th panel
/// row stays black).
const FB_W: usize = 120;
const FB_H: usize = 67;
const _: () = assert!(FB_W * 2 <= 240 && FB_H * 2 <= 135);

/// Controller RAM is 320×240 in landscape; the visible 240×135 window is
/// centered: X offset (320-240)/2 = 40 exactly (mirror-symmetric), Y
/// offset 52.5 → 53 per Adafruit's rotation-1 convention (see module
/// header for the ±1 row caveat).
const X_OFF: u16 = 40;
const Y_OFF: u16 = 53;
/// Full controller RAM extent in landscape addressing (for the init
/// clear, which erases garbage everywhere including the offset margins).
const RAM_W: u16 = 320;
const RAM_H: u16 = 240;

/// "On" pixels map to muted yellow — Meshtastic's T114 text colour,
/// COLOR565(255,255,128) = 0xFFF0 (formula: meshtastic
/// src/configuration.h:335). "Off" is black.
const COLOR_ON: u16 = 0xFFF0;
const COLOR_OFF: u16 = 0x0000;

/// One scaled panel row: max 240 px × 2 bytes. Also serves as the zero
/// source for the init clear.
const ROWBUF_LEN: usize = 240 * 2;

struct St7789 {
    spim: Spim<'static>,
    cs: Output<'static>,
    dc: Output<'static>,
}

impl St7789 {
    /// Send one command byte, then `data` with DC high. SPI write errors
    /// are ignored: on a TX-only bus to a write-only (possibly absent)
    /// panel there is nothing to observe and nothing to recover.
    async fn cmd(&mut self, op: u8, data: &[u8]) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spim.write(&[op]).await;
        self.dc.set_high();
        if !data.is_empty() {
            let _ = self.spim.write(data).await;
        }
        self.cs.set_high();
    }

    /// CASET/RASET in controller coordinates (offsets already applied by
    /// the caller), inclusive bounds.
    async fn set_window(&mut self, x0: u16, x1: u16, y0: u16, y1: u16) {
        self.cmd(
            CMD_CASET,
            &[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8],
        )
        .await;
        self.cmd(
            CMD_RASET,
            &[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8],
        )
        .await;
    }

    /// Issue RAMWR and leave CS low / DC high for pixel-data streaming;
    /// pair with `end_stream`.
    async fn begin_stream(&mut self) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spim.write(&[CMD_RAMWR]).await;
        self.dc.set_high();
    }

    fn end_stream(&mut self) {
        self.cs.set_high();
    }

    /// Init sequence VERBATIM from RNode `sendInitCommands()`
    /// (ST7789.h:295-340) — including the redundant CASET/RASET pre-set
    /// and the duplicated SLPOUT the reference carries ("hack" in the
    /// original). The mipidsi ST7789 model corroborates the command set
    /// (SLPOUT, COLMOD, MADCTL, INVON, DISPON); the exact order and
    /// delays are the T114-hardware-proven ones, kept untouched because
    /// this driver is blind — we cannot re-verify a "cleaned-up"
    /// sequence on a panel we don't have.
    async fn init(&mut self) {
        self.cmd(CMD_SWRESET, &[]).await;
        Timer::after(Duration::from_millis(150)).await;
        self.cmd(CMD_SLPOUT, &[]).await;
        Timer::after(Duration::from_millis(10)).await;
        // 16-bit RGB565.
        self.cmd(CMD_COLMOD, &[0x55]).await;
        Timer::after(Duration::from_millis(10)).await;
        self.cmd(CMD_MADCTL, &[0x08]).await;
        self.cmd(CMD_CASET, &[0x00, 0x00, 0x00, 240]).await;
        self.cmd(
            CMD_RASET,
            &[0x00, 0x00, (320 >> 8) as u8, (320 & 0xFF) as u8],
        )
        .await;
        self.cmd(CMD_SLPOUT, &[]).await;
        Timer::after(Duration::from_millis(10)).await;
        self.cmd(CMD_NORON, &[]).await;
        Timer::after(Duration::from_millis(10)).await;
        self.cmd(CMD_DISPON, &[]).await;
        Timer::after(Duration::from_millis(10)).await;
        // This panel wants inversion on for nominal colours (both
        // references send INVON).
        self.cmd(CMD_INVON, &[]).await;
        Timer::after(Duration::from_millis(10)).await;
        self.cmd(CMD_MADCTL, &[MADCTL_LANDSCAPE]).await;
        Timer::after(Duration::from_millis(10)).await;
    }

    /// Blacken the entire controller RAM (not just the visible window),
    /// so power-up garbage never shows regardless of offsets.
    async fn clear_ram(&mut self, zeros: &[u8; ROWBUF_LEN]) {
        self.set_window(0, RAM_W - 1, 0, RAM_H - 1).await;
        self.begin_stream().await;
        // 320×240 px × 2 B = 153 600 B = 320 chunks of one 480 B row.
        for _ in 0..RAM_W {
            let _ = self.spim.write(zeros).await;
        }
        self.end_stream();
    }

    /// Push one dirty region of the mono framebuffer, scaled 2×.
    async fn push_rect(&mut self, fb: &T114Fb, r: DirtyRect, rowbuf: &mut [u8; ROWBUF_LEN]) {
        let px0 = r.x0 * 2;
        let px1 = r.x1 * 2 + 1;
        let py0 = r.y0 * 2;
        let py1 = r.y1 * 2 + 1;
        self.set_window(px0 + X_OFF, px1 + X_OFF, py0 + Y_OFF, py1 + Y_OFF)
            .await;
        self.begin_stream().await;
        let row_bytes = (px1 - px0 + 1) as usize * 2;
        for y in r.y0..=r.y1 {
            let mut i = 0;
            for x in r.x0..=r.x1 {
                let c = if fb.pixel(x as usize, y as usize) {
                    COLOR_ON
                } else {
                    COLOR_OFF
                };
                // Pixel doubling: two big-endian RGB565 pixels per mono
                // pixel.
                rowbuf[i] = (c >> 8) as u8;
                rowbuf[i + 1] = c as u8;
                rowbuf[i + 2] = (c >> 8) as u8;
                rowbuf[i + 3] = c as u8;
                i += 4;
            }
            // Row doubling: each mono row covers two panel rows.
            let _ = self.spim.write(&rowbuf[..row_bytes]).await;
            let _ = self.spim.write(&rowbuf[..row_bytes]).await;
        }
        self.end_stream();
    }
}

/// Full TFT wiring, bundled so the task signature stays within reason.
/// Field types are the board aliases — the bin file can only hand over
/// the correct physical pins.
pub struct TftWiring {
    pub spi: Peri<'static, peripherals::SPI3>,
    pub sck: Peri<'static, crate::boards::t114::TftSck>,
    pub mosi: Peri<'static, crate::boards::t114::TftMosi>,
    pub cs: Peri<'static, crate::boards::t114::TftCs>,
    pub dc: Peri<'static, crate::boards::t114::TftDc>,
    pub rst: Peri<'static, crate::boards::t114::TftReset>,
    pub vtft: Peri<'static, crate::boards::t114::TftPowerEn>,
    pub leda: Peri<'static, crate::boards::t114::TftBacklight>,
}

#[embassy_executor::task]
pub async fn display_task(wiring: TftWiring, identity_hash: [u8; 16]) {
    let TftWiring {
        spi,
        sck,
        mosi,
        cs,
        dc,
        rst,
        vtft,
        leda,
    } = wiring;
    // Supply chain first: the shared VEXT rail is already high (the
    // binary raised it; see module header) — wait out what is left of
    // its warmup, then the TFT rail on (active low) and the backlight
    // OFF until the panel is initialised and cleared, so no garbage
    // flashes up.
    crate::vext::wait_ready().await;
    let _vtft = Output::new(vtft, Level::Low, OutputDrive::Standard);
    let mut leda = Output::new(leda, Level::High, OutputDrive::Standard);
    let mut rst = Output::new(rst, Level::High, OutputDrive::Standard);

    let mut config = spim::Config::default();
    // MODE_0 (embassy default) per RNode's SPISettings. 16 MHz: half of
    // SPIM3's maximum, 2.5× RNode's nRF rate; Meshtastic runs this very
    // wiring at 32+ MHz, so plenty of margin.
    config.frequency = spim::Frequency::M16;
    let spim = Spim::new_txonly(spi, TftIrqs, sck, mosi, config);

    let cs = Output::new(cs, Level::High, OutputDrive::Standard);
    let dc = Output::new(dc, Level::High, OutputDrive::Standard);
    let mut panel = St7789 { spim, cs, dc };

    // Rail settle, then the RNode reset pulse (ST7789.h `connect()`:
    // 1 ms high, 10 ms low, high).
    Timer::after(Duration::from_millis(100)).await;
    rst.set_high();
    Timer::after(Duration::from_millis(1)).await;
    rst.set_low();
    Timer::after(Duration::from_millis(10)).await;
    rst.set_high();

    crate::log::log_fmt(
        "[DISP] ",
        format_args!("ST7789 blind init start (write-only panel, no probe possible)"),
    );

    static ROWBUF: StaticCell<[u8; ROWBUF_LEN]> = StaticCell::new();
    let rowbuf: &'static mut [u8; ROWBUF_LEN] = ROWBUF.init([0u8; ROWBUF_LEN]);

    panel.init().await;
    panel.clear_ram(rowbuf).await;
    // Panel is black now — light it up.
    leda.set_low();

    crate::log::log_fmt(
        "[DISP] ",
        format_args!("ST7789 init complete, status frame at 1 Hz"),
    );

    // Double buffer: WORK is painted each frame, SHOWN mirrors what the
    // panel currently displays (all-black after clear_ram, matching the
    // zeroed buffer). 2 × 1005 B statics, no heap.
    static WORK: StaticCell<T114Fb> = StaticCell::new();
    static SHOWN: StaticCell<T114Fb> = StaticCell::new();
    let work = WORK.init(T114Fb::new());
    let shown = SHOWN.init(T114Fb::new());

    let id_short = ident_short(&identity_hash);
    let mut last_key: Option<FrameKey> = None;
    let mut first_frame = true;
    let mut tick: u32 = 0;

    loop {
        tick = tick.wrapping_add(1);
        let heartbeat = (tick / 5) & 1 != 0; // toggles every 5 seconds

        // The presence tri-state (#240) drives the GPS line exactly as it
        // does on the V2 (`display.rs`): NoHardware renders "check
        // wiring", and the fix/search label follows the hysteresis-held
        // policy state rather than the raw per-sentence receiver flag, so
        // the panel agrees with what every other consumer of
        // GNSS_PRESENCE acts on. Read straight off the watches rather
        // than through receivers — this task only ever wants the latest
        // value and must not compete for a receiver slot. `bsp-t114`
        // implies the `gnss` feature (Cargo.toml), so there is no
        // feature-off arm here.
        let gnss = match crate::baseboard::GNSS_PRESENCE.try_get() {
            Some(p) if p.state == crate::baseboard::GnssPresence::NoHardware => {
                GnssStatus::NoHardware
            }
            Some(p) => {
                let f = crate::baseboard::GNSS_FIX
                    .try_get()
                    .unwrap_or(crate::baseboard::GnssFix::empty());
                GnssStatus::Data {
                    sats: f.sat_in_use,
                    valid: p.state == crate::baseboard::GnssPresence::Fix,
                    coords: f.latitude.zip(f.longitude),
                }
            }
            None => GnssStatus::NoData,
        };

        let model = StatusModel {
            title: "leviculum T114",
            id_short: id_short.as_str(),
            rx: crate::lora::LORA_RX_COUNT.load(core::sync::atomic::Ordering::Relaxed),
            tx: crate::lora::LORA_TX_COUNT.load(core::sync::atomic::Ordering::Relaxed),
            // No battery gauge task on this board yet: the ADC divider is
            // wired but nothing reads it, so the shared painter renders
            // the "(no feature)" line the V2 shows in that configuration.
            battery: BatteryStatus::FeatureOff,
            gnss,
            // Same source as the `links=` field of the `BLE_COUNTERS`
            // log line, so the panel and the capture can never
            // disagree. Not behind a cfg, for the reason spelled out at
            // the V2's model (`display.rs`): `bsp-t114` implies
            // `softdevice`, so this build always has BLE. `MAX_LINKS`
            // is 4, so the saturation never bites.
            ble_peers: Some(crate::ble::HVN_DRAIN.claimed().min(u8::MAX as usize) as u8),
            heartbeat,
        };

        let key = model.key();
        if last_key.as_ref() != Some(&key) {
            // MonoFb painting is infallible.
            let _ = model.paint(work, FB_W as u32);
            if let Some(dirty) = work.diff(shown) {
                if first_frame {
                    // The FONT_6X10 render into `work` above is
                    // the deepest frame on this task's stack, and the
                    // whole task runs on the single thread-mode stack.
                    // Bracketing the first push isolates what the
                    // display path costs from what the rest of the
                    // firmware costs.
                    crate::log_stack("disp-pre-first-frame");
                }
                panel.push_rect(work, dirty, rowbuf).await;
                shown.copy_from(work);
                if first_frame {
                    crate::log_stack("disp-post-first-frame");
                    crate::log::log_fmt(
                        "[DISP] ",
                        format_args!(
                            "first frame pushed (rect {},{}..{},{})",
                            dirty.x0, dirty.y0, dirty.x1, dirty.y1
                        ),
                    );
                    first_frame = false;
                }
            }
            last_key = Some(key);
        }

        Timer::after(Duration::from_secs(1)).await;
    }
}

/// Convenience wrapper invoked from the bin file.
pub fn init(spawner: &embassy_executor::Spawner, wiring: TftWiring, identity_hash: [u8; 16]) {
    spawner.must_spawn(display_task(wiring, identity_hash));
}
