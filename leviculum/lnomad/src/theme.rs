//! The light/dark theme model and the pure detection decision.
//!
//! A [`Theme`] carries the accent colours lnomad chooses for a page: the section
//! heading bands, the link colour, the hint-badge colours, and the fixed chrome
//! bars. It does NOT carry the page's own micron foreground/background colours,
//! which are absolute RGB authored into the document and stay as written; the
//! content default fg/bg is left unset (the terminal's own default) in both
//! themes. Only the accents and chrome move between light and dark.
//!
//! [`Dark`](Theme::Dark) reproduces today's look. [`Light`](Theme::Light)
//! mirrors NomadNet's `STYLES_LIGHT` heading bands (`MicronParser.py`) and pairs
//! them with a deep-blue link colour and a light chrome bar.
//!
//! Detection is split so the decision is testable without a terminal: the IO
//! shell reads the terminal background (via the `termbg` crate's OSC 11 query,
//! with a `COLORFGBG` fallback) into a [`Bg`], and the pure
//! [`resolve_theme`] maps that plus the `--theme` flag onto a [`Theme`]. On any
//! detection failure or timeout the result is [`Theme::Dark`].

/// The set of accent colours to render a page with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    /// Today's look: light accents on the terminal's (assumed dark) background.
    #[default]
    Dark,
    /// A light-terminal palette: darker accents and a light chrome bar.
    Light,
}

/// The `--theme` command-line choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeFlag {
    /// Detect the terminal background and pick the matching theme.
    #[default]
    Auto,
    /// Force the light theme.
    Light,
    /// Force the dark theme.
    Dark,
}

/// A detected terminal background, and its foreground when one is known.
///
/// Colours are 8-bit-per-channel RGB. `fg` is `Some` only when the terminal (or
/// `COLORFGBG`) reported a foreground; it is used solely as a tiebreaker for a
/// mid-tone background whose luminance alone does not settle the choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bg {
    /// The terminal background colour.
    pub bg: (u8, u8, u8),
    /// The terminal foreground colour, if known.
    pub fg: Option<(u8, u8, u8)>,
}

/// Backgrounds at or above this WCAG luminance are unambiguously light.
const LIGHT_CUTOFF: f32 = 0.6;
/// Backgrounds at or below this WCAG luminance are unambiguously dark.
const DARK_CUTOFF: f32 = 0.4;

/// WCAG relative luminance of an RGB colour, in `[0, 1]`
/// (`0.2126 R + 0.7152 G + 0.0722 B`, on the 0..255 channels).
fn luminance((r, g, b): (u8, u8, u8)) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// Resolve the theme from the (optional) detected background and the `--theme`
/// flag. An explicit `light`/`dark` flag always wins; `auto` uses the detection,
/// defaulting to [`Theme::Dark`] when nothing was detected.
pub fn resolve_theme(detected: Option<Bg>, flag: ThemeFlag) -> Theme {
    match flag {
        ThemeFlag::Light => Theme::Light,
        ThemeFlag::Dark => Theme::Dark,
        ThemeFlag::Auto => match detected {
            Some(bg) => decide(bg),
            None => Theme::Dark,
        },
    }
}

/// The `auto` decision: primary signal is background luminance; a mid-tone
/// background falls back to the foreground-vs-background contrast (light text on
/// a darker background reads as a dark terminal, and vice versa).
fn decide(bg: Bg) -> Theme {
    let lum = luminance(bg.bg);
    if lum >= LIGHT_CUTOFF {
        return Theme::Light;
    }
    if lum <= DARK_CUTOFF {
        return Theme::Dark;
    }
    // Mid-tone: use the foreground as a tiebreaker when we have one.
    match bg.fg {
        Some(fg) if luminance(fg) > lum => Theme::Dark,
        Some(_) => Theme::Light,
        None if lum >= 0.5 => Theme::Light,
        None => Theme::Dark,
    }
}

impl Theme {
    /// The other theme (for the runtime toggle key).
    pub fn toggle(self) -> Theme {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }

    /// The `(fg, bg)` heading-band colours for a section depth. Depths beyond 3
    /// reuse the depth-3 band. Dark mirrors `STYLES_DARK`, light `STYLES_LIGHT`
    /// (`MicronParser.py`), each micron nibble doubled into an 8-bit channel.
    pub fn heading_band(self, depth: u8) -> ((u8, u8, u8), (u8, u8, u8)) {
        match self {
            Theme::Dark => match depth {
                1 => ((0x22, 0x22, 0x22), (0xbb, 0xbb, 0xbb)),
                2 => ((0x11, 0x11, 0x11), (0x99, 0x99, 0x99)),
                _ => ((0x00, 0x00, 0x00), (0x77, 0x77, 0x77)),
            },
            Theme::Light => match depth {
                1 => ((0x00, 0x00, 0x00), (0x77, 0x77, 0x77)),
                2 => ((0x11, 0x11, 0x11), (0xaa, 0xaa, 0xaa)),
                _ => ((0x22, 0x22, 0x22), (0xcc, 0xcc, 0xcc)),
            },
        }
    }

    /// The link-label foreground: bright cyan on dark, deep blue on light.
    pub fn link_fg(self) -> (u8, u8, u8) {
        match self {
            Theme::Dark => (0, 175, 255),
            Theme::Light => (0, 90, 170),
        }
    }

    /// The chrome-bar background (top-bar and status bar fill).
    pub fn chrome_bg(self) -> (u8, u8, u8) {
        match self {
            Theme::Dark => (38, 40, 49),
            Theme::Light => (225, 228, 235),
        }
    }

    /// The chrome-bar foreground drawn over [`chrome_bg`](Theme::chrome_bg).
    pub fn chrome_fg(self) -> (u8, u8, u8) {
        match self {
            Theme::Dark => (205, 210, 220),
            Theme::Light => (40, 44, 52),
        }
    }

    /// A muted-but-readable chrome foreground for the UNAVAILABLE top-bar
    /// controls (a disabled back/forward arrow). It sits between
    /// [`chrome_fg`](Theme::chrome_fg) and the chrome background: distinct enough
    /// to read as "disabled", but with roughly 3:1 contrast on
    /// [`chrome_bg`](Theme::chrome_bg) so it stays legible. Not `Modifier::DIM`,
    /// which halves the foreground and drops below readable on the chrome bar.
    pub fn chrome_muted_fg(self) -> (u8, u8, u8) {
        match self {
            Theme::Dark => (140, 150, 165),
            Theme::Light => (95, 100, 112),
        }
    }

    /// The hint-badge foreground (the typed label glyphs).
    pub fn hint_badge_fg(self) -> (u8, u8, u8) {
        match self {
            Theme::Dark => (0, 0, 0),
            Theme::Light => (255, 255, 255),
        }
    }

    /// The hint-badge background: gold on dark, deep blue on light.
    pub fn hint_badge_bg(self) -> (u8, u8, u8) {
        match self {
            Theme::Dark => (255, 215, 0),
            Theme::Light => (0, 90, 170),
        }
    }

    /// The background tint painted over every in-page search match (the current
    /// match gets a stronger, reversed highlight on top of this). A muted amber
    /// on dark, a pale amber on light, so a match reads as marked without the
    /// full contrast reserved for the current one.
    pub fn search_match_bg(self) -> (u8, u8, u8) {
        match self {
            Theme::Dark => (120, 100, 0),
            Theme::Light => (255, 235, 130),
        }
    }

    /// The foreground drawn over [`search_match_bg`](Theme::search_match_bg): a
    /// readable pairing for the non-current search-match tint.
    pub fn search_match_fg(self) -> (u8, u8, u8) {
        match self {
            Theme::Dark => (255, 255, 255),
            Theme::Light => (0, 0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bg(rgb: (u8, u8, u8)) -> Option<Bg> {
        Some(Bg { bg: rgb, fg: None })
    }

    #[test]
    fn bright_background_auto_resolves_light() {
        assert_eq!(
            resolve_theme(bg((255, 255, 255)), ThemeFlag::Auto),
            Theme::Light
        );
        assert_eq!(
            resolve_theme(bg((240, 240, 235)), ThemeFlag::Auto),
            Theme::Light
        );
    }

    #[test]
    fn dark_background_auto_resolves_dark() {
        assert_eq!(resolve_theme(bg((0, 0, 0)), ThemeFlag::Auto), Theme::Dark);
        assert_eq!(
            resolve_theme(bg((16, 18, 24)), ThemeFlag::Auto),
            Theme::Dark
        );
    }

    #[test]
    fn explicit_flag_overrides_detection() {
        // A bright background but an explicit dark flag => dark.
        assert_eq!(
            resolve_theme(bg((255, 255, 255)), ThemeFlag::Dark),
            Theme::Dark
        );
        // A dark background but an explicit light flag => light.
        assert_eq!(resolve_theme(bg((0, 0, 0)), ThemeFlag::Light), Theme::Light);
    }

    #[test]
    fn auto_without_detection_defaults_dark() {
        assert_eq!(resolve_theme(None, ThemeFlag::Auto), Theme::Dark);
    }

    #[test]
    fn midtone_background_uses_foreground_tiebreaker() {
        // A mid-grey background: a darker foreground (dark text on grey) reads as
        // a light terminal; a brighter foreground (light text on grey) as dark.
        let mid = (128, 128, 128);
        let darker_fg = Some(Bg {
            bg: mid,
            fg: Some((20, 20, 20)),
        });
        let brighter_fg = Some(Bg {
            bg: mid,
            fg: Some((240, 240, 240)),
        });
        assert_eq!(resolve_theme(darker_fg, ThemeFlag::Auto), Theme::Light);
        assert_eq!(resolve_theme(brighter_fg, ThemeFlag::Auto), Theme::Dark);
    }

    #[test]
    fn toggle_flips_the_theme() {
        assert_eq!(Theme::Dark.toggle(), Theme::Light);
        assert_eq!(Theme::Light.toggle(), Theme::Dark);
    }

    #[test]
    fn dark_values_match_todays_look() {
        assert_eq!(Theme::Dark.link_fg(), (0, 175, 255));
        assert_eq!(Theme::Dark.chrome_bg(), (38, 40, 49));
        assert_eq!(
            Theme::Dark.heading_band(2),
            ((0x11, 0x11, 0x11), (0x99, 0x99, 0x99))
        );
    }

    #[test]
    fn light_values_are_the_light_palette() {
        assert_eq!(Theme::Light.link_fg(), (0, 90, 170));
        assert_eq!(Theme::Light.chrome_bg(), (225, 228, 235));
        assert_eq!(
            Theme::Light.heading_band(1),
            ((0x00, 0x00, 0x00), (0x77, 0x77, 0x77))
        );
    }
}
