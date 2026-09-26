//! Terminal colour-depth ladder: 24-bit true colour versus the xterm-256 palette.
//!
//! lnomad authors every accent as an absolute 24-bit RGB triple. On a terminal
//! that speaks true colour those emit directly; on one that does not, the same
//! `38;2;r;g;b` sequences render as garbage or the nearest 16-colour guess. The
//! [`ColorDepth`] ladder resolves, once at startup, whether the terminal takes
//! true colour, and [`rgb_to_ansi256`] downgrades an RGB triple to the nearest
//! xterm-256 index when it does not.
//!
//! Resolution is split so the decision is testable without an environment: the
//! IO shell reads `COLORTERM` and the `--color` flag, and the pure
//! [`resolve_depth`] maps them onto a [`ColorDepth`]. This composes with, and is
//! subordinate to, the `no_color` suppression: `NO_COLOR` still wins and drops
//! the UI to monochrome/reverse regardless of the resolved depth.

/// The terminal's colour depth, resolved once at startup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit true colour: RGB triples emit as `38;2;r;g;b` unchanged.
    #[default]
    Truecolor,
    /// The xterm-256 palette: RGB triples are downgraded to the nearest indexed
    /// colour (`38;5;idx`) via [`rgb_to_ansi256`].
    Ansi256,
}

/// The `--color` command-line choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorFlag {
    /// Detect the depth from `COLORTERM` (the default).
    #[default]
    Auto,
    /// Force 24-bit true colour.
    Truecolor,
    /// Force the xterm-256 palette.
    Ansi256,
}

/// Resolve the colour depth from the `--color` flag and the `COLORTERM`
/// environment value. An explicit flag always wins; `auto` treats a `COLORTERM`
/// of `truecolor` or `24bit` as [`ColorDepth::Truecolor`] and everything else
/// (including an absent value) as [`ColorDepth::Ansi256`].
pub fn resolve_depth(flag: ColorFlag, colorterm: Option<&str>) -> ColorDepth {
    match flag {
        ColorFlag::Truecolor => ColorDepth::Truecolor,
        ColorFlag::Ansi256 => ColorDepth::Ansi256,
        ColorFlag::Auto => match colorterm {
            Some("truecolor") | Some("24bit") => ColorDepth::Truecolor,
            _ => ColorDepth::Ansi256,
        },
    }
}

/// The six per-channel levels of the xterm 6x6x6 colour cube.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Map [`ColorDepth::Truecolor`] RGB to the nearest xterm-256 palette index.
///
/// Considers both the 6x6x6 colour cube (`16 + 36*r6 + 6*g6 + b6`, each channel
/// snapped to its nearest cube level) and the 24-step grayscale ramp
/// (`232..=255`), and returns whichever candidate is nearer to the input by
/// squared Euclidean distance. A near-grey input therefore lands on the ramp,
/// which is finer than the cube's grey diagonal, while a saturated colour lands
/// in the cube. Pure and total.
pub fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let (cube_idx, cube_rgb) = nearest_cube(r, g, b);
    let (gray_idx, gray_rgb) = nearest_gray(r, g, b);
    if dist2((r, g, b), gray_rgb) < dist2((r, g, b), cube_rgb) {
        gray_idx
    } else {
        cube_idx
    }
}

/// The nearest colour-cube index for an RGB triple, with the cube colour it
/// resolves to (for the cube-vs-gray distance comparison).
fn nearest_cube(r: u8, g: u8, b: u8) -> (u8, (u8, u8, u8)) {
    let r6 = nearest_cube_level(r);
    let g6 = nearest_cube_level(g);
    let b6 = nearest_cube_level(b);
    let idx = 16 + 36 * r6 + 6 * g6 + b6;
    (
        idx as u8,
        (CUBE_LEVELS[r6], CUBE_LEVELS[g6], CUBE_LEVELS[b6]),
    )
}

/// The index (`0..=5`) of the cube level nearest to a channel value.
fn nearest_cube_level(c: u8) -> usize {
    let mut best = 0;
    let mut best_d = i32::MAX;
    for (i, &level) in CUBE_LEVELS.iter().enumerate() {
        let d = (c as i32 - level as i32).abs();
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// The nearest grayscale-ramp index (`232..=255`) for an RGB triple, using its
/// mean as the grey level, with the grey colour it resolves to. The ramp holds
/// 24 steps at `8 + 10*i`.
fn nearest_gray(r: u8, g: u8, b: u8) -> (u8, (u8, u8, u8)) {
    let mean = (r as i32 + g as i32 + b as i32) / 3;
    let step = (((mean - 8).max(0) + 5) / 10).min(23);
    let value = (8 + 10 * step) as u8;
    (232 + step as u8, (value, value, value))
}

/// Squared Euclidean distance between two RGB triples.
fn dist2(a: (u8, u8, u8), b: (u8, u8, u8)) -> i32 {
    let dr = a.0 as i32 - b.0 as i32;
    let dg = a.1 as i32 - b.1 as i32;
    let db = a.2 as i32 - b.2 as i32;
    dr * dr + dg * dg + db * db
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_colours_map_to_the_cube() {
        assert_eq!(rgb_to_ansi256(255, 0, 0), 196);
        assert_eq!(rgb_to_ansi256(0, 255, 0), 46);
        assert_eq!(rgb_to_ansi256(0, 0, 255), 21);
    }

    #[test]
    fn black_and_white_map_to_the_cube_corners() {
        assert_eq!(rgb_to_ansi256(0, 0, 0), 16);
        assert_eq!(rgb_to_ansi256(255, 255, 255), 231);
    }

    #[test]
    fn mid_grey_maps_into_the_grayscale_ramp() {
        let idx = rgb_to_ansi256(128, 128, 128);
        assert!(
            (232..=255).contains(&idx),
            "mid-grey should land on the 232..=255 ramp, got {idx}"
        );
    }

    #[test]
    fn depth_resolves_from_colorterm_when_auto() {
        assert_eq!(
            resolve_depth(ColorFlag::Auto, Some("truecolor")),
            ColorDepth::Truecolor
        );
        assert_eq!(
            resolve_depth(ColorFlag::Auto, Some("24bit")),
            ColorDepth::Truecolor
        );
        assert_eq!(
            resolve_depth(ColorFlag::Auto, Some("xterm-256color")),
            ColorDepth::Ansi256
        );
        assert_eq!(resolve_depth(ColorFlag::Auto, None), ColorDepth::Ansi256);
    }

    #[test]
    fn explicit_flag_overrides_colorterm() {
        assert_eq!(
            resolve_depth(ColorFlag::Ansi256, Some("truecolor")),
            ColorDepth::Ansi256
        );
        assert_eq!(
            resolve_depth(ColorFlag::Truecolor, None),
            ColorDepth::Truecolor
        );
    }
}
