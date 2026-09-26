//! Board-independent status-screen painter.
//!
//! One drawing codebase for every leviculum-nrf display backend: the
//! WisMesh Pocket V2 paints straight into the SSD1306's buffered
//! `DrawTarget`, the Heltec T114 paints into a [`MonoFb`] that the ST7789
//! backend scales onto the panel. Everything in this crate is pure
//! (no peripherals, no async, no allocation), so the exact frames the
//! firmware shows are locked by golden-frame tests running under
//! `cargo test` on the host.
//!
//! The content model mirrors the historical Pocket V2 screen exactly:
//! six FONT_6X10 text lines at 10 px pitch plus a 2×2 heartbeat dot in
//! the top-right corner. Feature-dependent lines ("Bat: -- (no feature)")
//! are data-driven here — the firmware maps its `cfg`-gated sources into
//! [`BatteryStatus`]/[`GnssStatus`] variants instead of formatting its own
//! strings.

#![no_std]

use core::fmt::Write as _;

use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};

/// Battery line source. `FeatureOff` renders the historical
/// "Bat: -- (no feature)", `NoData` the runtime "Bat: --".
#[derive(Clone, Copy, Debug)]
pub enum BatteryStatus {
    FeatureOff,
    NoData,
    Data { percent: u8, voltage_mv: u16 },
}

/// GNSS line source. `NoData` is the "receiver not heard from yet"
/// state ("GPS: 0 sat init"); `NoHardware` is the settled runtime
/// answer that no receiver is wired to the UART (Codeberg #240) — the
/// operator action differs (check wiring versus wait), so the two must
/// render distinguishably. An invalid fix still carries whatever
/// coordinates the receiver reported (matching the historical V2 loop,
/// which zipped latitude/longitude independently of fix validity).
#[derive(Clone, Copy, Debug)]
pub enum GnssStatus {
    FeatureOff,
    NoData,
    NoHardware,
    Data {
        sats: u8,
        valid: bool,
        coords: Option<(f64, f64)>,
    },
}

/// Everything a status frame is derived from.
#[derive(Clone, Copy)]
pub struct StatusModel<'a> {
    /// First line, e.g. "leviculum RAK4631".
    pub title: &'a str,
    /// 10-hex-char identity prefix (see [`ident_short`]).
    pub id_short: &'a str,
    pub rx: u32,
    pub tx: u32,
    pub battery: BatteryStatus,
    pub gnss: GnssStatus,
    /// Live BLE peer count for line 3's `B:` field, or `None` on a
    /// board without BLE at all — then line 3 keeps its historical
    /// `RX:`/`TX:` shape and spends no character on a medium the board
    /// does not have. The firmware feeds this from the same claimed
    /// drain slots the `BLE_COUNTERS` log line prints as `links=`, so
    /// screen and log cannot disagree.
    pub ble_peers: Option<u8>,
    pub heartbeat: bool,
}

/// Frame key — when nothing relevant has changed since last render, the
/// backend can skip painting entirely. lat/lon are quantized to 5 decimal
/// places (≈1 m); receiver-jitter below that level no longer flips the
/// frame. The heartbeat phase bumps the key every 5 s so the screen
/// refreshes itself slowly (lifetime indicator).
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct FrameKey {
    rx: u32,
    tx: u32,
    sat: u8,
    valid: bool,
    // Latitude/longitude quantized to 5dp via int(round(value * 1e5)).
    // Option<i64> survives the no-fix transition.
    lat_e5: Option<i64>,
    lon_e5: Option<i64>,
    // Battery percent — None when feature off or no reading; quantized
    // to 1 % so ADC-noise doesn't trigger every-render flushes.
    bat_pct: Option<u8>,
    // Pack voltage rounded to 100 mV so noise on the LSDigit doesn't
    // wake the renderer.
    bat_dv: Option<u16>,
    // GnssStatus discriminant — NoData and NoHardware render different
    // text with otherwise identical field values, so the variant itself
    // must key the frame.
    gnss_kind: u8,
    // BLE peer count, verbatim — a peer coming or going must repaint on
    // the spot rather than waiting for the 5 s heartbeat to bump the key.
    ble_peers: Option<u8>,
    heartbeat: bool,
}

/// Number of text lines on the status screen.
pub const LINES: usize = 6;
/// Vertical pitch of the FONT_6X10 lines in pixels.
pub const LINE_PITCH: i32 = 10;

impl StatusModel<'_> {
    /// Change-detection key for this frame.
    pub fn key(&self) -> FrameKey {
        let (bat_pct, bat_dv) = match self.battery {
            BatteryStatus::Data {
                percent,
                voltage_mv,
            } => (Some(percent), Some(voltage_mv / 100)),
            _ => (None, None),
        };
        let (sat, valid, coords) = match self.gnss {
            GnssStatus::Data {
                sats,
                valid,
                coords,
            } => (sats, valid, coords),
            _ => (0, false, None),
        };
        let gnss_kind = match self.gnss {
            GnssStatus::FeatureOff => 0,
            GnssStatus::NoData => 1,
            GnssStatus::NoHardware => 2,
            GnssStatus::Data { .. } => 3,
        };
        FrameKey {
            rx: self.rx,
            tx: self.tx,
            sat,
            valid,
            // `as i64` truncates toward zero; that's fine for change
            // detection — a single LSB flip from rounding semantics
            // would just trigger one extra render, and at 5dp each
            // unit equals ~1.1 m so coordinate jitter rarely flips
            // the truncated key at all.
            lat_e5: coords.map(|(lat, _)| (lat * 1e5) as i64),
            lon_e5: coords.map(|(_, lon)| (lon * 1e5) as i64),
            bat_pct,
            bat_dv,
            gnss_kind,
            ble_peers: self.ble_peers,
            heartbeat: self.heartbeat,
        }
    }

    /// The six text lines, formatted exactly as the historical V2 loop
    /// did. Overlong content truncates at the 24-char capacity (partial
    /// write, same `let _ = write!` semantics as before) and clips at
    /// the right framebuffer edge when drawn.
    pub fn lines(&self) -> [heapless::String<24>; LINES] {
        let mut line1 = heapless::String::<24>::new();
        for c in self.title.chars() {
            if line1.push(c).is_err() {
                break;
            }
        }

        let mut line2 = heapless::String::<24>::new();
        let _ = write!(line2, "ID: {}", self.id_short);

        // Line 3 carries the traffic counters and, on a BLE board, the
        // peer count. The narrower of the two backends (the T114's
        // 120 px framebuffer, 20 FONT_6X10 characters) sets the budget;
        // the short labels keep the three-field form at 19 characters
        // with five-digit counts, both columns still fixed-width. A
        // board without BLE renders the historical two-field line
        // unchanged, trailing space included in neither form.
        let mut line3 = heapless::String::<24>::new();
        match self.ble_peers {
            Some(peers) => {
                let _ = write!(line3, "R:{:<5} T:{:<5} B:{}", self.rx, self.tx, peers);
            }
            None => {
                let _ = write!(line3, "RX: {:<5} TX: {:<5}", self.rx, self.tx);
            }
        }

        let mut line4 = heapless::String::<24>::new();
        match self.battery {
            BatteryStatus::FeatureOff => {
                let _ = write!(line4, "Bat: -- (no feature)");
            }
            BatteryStatus::NoData => {
                let _ = write!(line4, "Bat: --");
            }
            BatteryStatus::Data {
                percent,
                voltage_mv,
            } => {
                let v_int = voltage_mv / 1000;
                let v_frac = (voltage_mv % 1000) / 10;
                let _ = write!(line4, "Bat: {:>3}% {}.{:02}V", percent, v_int, v_frac);
            }
        }

        let mut line5 = heapless::String::<24>::new();
        let mut line6 = heapless::String::<24>::new();
        match self.gnss {
            GnssStatus::FeatureOff => {
                let _ = write!(line5, "GPS: -- (no feature)");
            }
            GnssStatus::NoData => {
                let _ = write!(line5, "GPS: 0 sat init");
                let _ = write!(line6, "(no fix)");
            }
            GnssStatus::NoHardware => {
                let _ = write!(line5, "GPS: no receiver");
                let _ = write!(line6, "(check wiring)");
            }
            GnssStatus::Data {
                sats,
                valid,
                coords,
            } => {
                let label = if valid { "fix" } else { "search" };
                let _ = write!(line5, "GPS: {} sat {}", sats, label);
                match coords {
                    Some((lat, lon)) => {
                        let _ = write!(line6, "{:.5},{:.5}", lat, lon);
                    }
                    None => {
                        let _ = write!(line6, "(no fix)");
                    }
                }
            }
        }

        [line1, line2, line3, line4, line5, line6]
    }

    /// Paint the full frame: clear, six text lines, heartbeat dot.
    ///
    /// `width` is the target's drawable width in pixels; the heartbeat
    /// dot sits at `(width - 3, 0)` — 125 on the V2's 128-wide OLED,
    /// exactly where it has always been.
    pub fn paint<D: DrawTarget<Color = BinaryColor>>(
        &self,
        target: &mut D,
        width: u32,
    ) -> Result<(), D::Error> {
        target.clear(BinaryColor::Off)?;

        let style = MonoTextStyleBuilder::new()
            .font(&FONT_6X10)
            .text_color(BinaryColor::On)
            .build();

        for (i, line) in self.lines().iter().enumerate() {
            let y = (i as i32) * LINE_PITCH;
            Text::with_baseline(line, Point::new(0, y), style, Baseline::Top).draw(target)?;
        }

        // Heartbeat marker — a 2×2 dot in the top-right corner, drawn only
        // during the "on" phase. Tells you "the firmware loop is alive"
        // without redrawing the rest of the screen at any visible rate.
        if self.heartbeat {
            Rectangle::new(Point::new(width as i32 - 3, 0), Size::new(2, 2))
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(target)?;
        }
        Ok(())
    }
}

/// Format the identity hash as 10 hex chars (5 bytes).
pub fn ident_short(hash: &[u8; 16]) -> heapless::String<10> {
    let mut s: heapless::String<10> = heapless::String::new();
    let hex = b"0123456789abcdef";
    for b in &hash[..5] {
        let _ = s.push(hex[(*b >> 4) as usize] as char);
        let _ = s.push(hex[(*b & 0x0F) as usize] as char);
    }
    s
}

/// Inclusive dirty rectangle in framebuffer pixel coordinates.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct DirtyRect {
    pub x0: u16,
    pub y0: u16,
    pub x1: u16,
    pub y1: u16,
}

/// 1-bit row-major framebuffer, MSB-first within each byte.
///
/// `STRIDE` must be `W.div_ceil(8)` and `N == STRIDE * H`; the
/// constructor asserts this (const generics can't express the relation
/// on stable). Implements `DrawTarget<BinaryColor>` with out-of-bounds
/// pixels silently clipped — that clipping IS the long-line truncation
/// behaviour the golden tests pin down.
pub struct MonoFb<const W: usize, const H: usize, const N: usize> {
    bits: [u8; N],
}

impl<const W: usize, const H: usize, const N: usize> MonoFb<W, H, N> {
    const STRIDE: usize = W.div_ceil(8);

    pub const fn new() -> Self {
        assert!(N == Self::STRIDE * H, "N must equal W.div_ceil(8) * H");
        Self { bits: [0u8; N] }
    }

    pub const fn width(&self) -> usize {
        W
    }

    pub const fn height(&self) -> usize {
        H
    }

    /// Read one pixel. Out-of-bounds reads return `false`.
    pub fn pixel(&self, x: usize, y: usize) -> bool {
        if x >= W || y >= H {
            return false;
        }
        self.bits[y * Self::STRIDE + x / 8] & (0x80 >> (x % 8)) != 0
    }

    /// Bounding box of every pixel that differs from `other`, or `None`
    /// when the buffers are identical. Byte-wise scan, bit-exact bounds.
    pub fn diff(&self, other: &Self) -> Option<DirtyRect> {
        let mut r: Option<DirtyRect> = None;
        for y in 0..H {
            let row = y * Self::STRIDE;
            for bx in 0..Self::STRIDE {
                let delta = self.bits[row + bx] ^ other.bits[row + bx];
                if delta == 0 {
                    continue;
                }
                let x_lo = (bx * 8 + delta.leading_zeros() as usize) as u16;
                let x_hi = (bx * 8 + 7 - delta.trailing_zeros() as usize) as u16;
                let y16 = y as u16;
                r = Some(match r {
                    None => DirtyRect {
                        x0: x_lo,
                        y0: y16,
                        x1: x_hi,
                        y1: y16,
                    },
                    Some(c) => DirtyRect {
                        x0: c.x0.min(x_lo),
                        y0: c.y0,
                        x1: c.x1.max(x_hi),
                        y1: y16,
                    },
                });
            }
        }
        r
    }

    /// Copy the full contents of `other` into `self`.
    pub fn copy_from(&mut self, other: &Self) {
        self.bits.copy_from_slice(&other.bits);
    }
}

impl<const W: usize, const H: usize, const N: usize> Default for MonoFb<W, H, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const W: usize, const H: usize, const N: usize> OriginDimensions for MonoFb<W, H, N> {
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

impl<const W: usize, const H: usize, const N: usize> DrawTarget for MonoFb<W, H, N> {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, color) in pixels {
            if p.x < 0 || p.y < 0 || p.x as usize >= W || p.y as usize >= H {
                continue;
            }
            let (x, y) = (p.x as usize, p.y as usize);
            let byte = &mut self.bits[y * Self::STRIDE + x / 8];
            let mask = 0x80 >> (x % 8);
            match color {
                BinaryColor::On => *byte |= mask,
                BinaryColor::Off => *byte &= !mask,
            }
        }
        Ok(())
    }
}

/// The T114 ST7789 paints through a 120×67 mono framebuffer, scaled 2×
/// onto the 240×135 panel (the 135th panel row stays black).
pub type T114Fb = MonoFb<120, 67, { 15 * 67 }>;
/// Pocket-V2-geometry framebuffer (128×64) — used by the golden tests to
/// lock the V2 frame; the real V2 path draws into the SSD1306 buffer.
pub type V2Fb = MonoFb<128, 64, { 16 * 64 }>;
