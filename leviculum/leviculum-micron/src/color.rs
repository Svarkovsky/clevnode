//! Micron colour values.
//!
//! Micron encodes a colour after an `F` (foreground) or `B` (background) inline
//! command in one of two forms: three characters (`` `Fxxx ``) for the 12-bit
//! form, or a `T` prefix plus six hex characters (`` `FT<rrggbb> ``) for the
//! 24-bit true-colour form. The raw form is kept verbatim, plus a best-effort
//! resolved 24-bit RGB triple. Resolution mirrors NomadNet `MicronParser.py`
//! `high_color` (canonical lines 518-567): a leading `g` selects a two-digit
//! grayscale level, three hex nibbles are each doubled to form `#rrggbb`, and
//! six hex characters are taken as a full `#rrggbb`.

/// A parsed micron colour.
///
/// `raw` is the exact micron colour form, either three characters (e.g.
/// `"f00"`, `"g50"`) or six hex characters for true colour (e.g. `"00ff80"`).
/// `rgb` is the resolved 24-bit colour, or `None` when the raw form is not a
/// valid micron colour. `None` mirrors the reference behaviour of falling back
/// to the terminal default; resolution stays render-agnostic (no theme here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Color {
    /// The three-character micron colour form, kept verbatim.
    pub raw: String,
    /// Resolved 24-bit RGB, or `None` for the terminal default fallback.
    pub rgb: Option<(u8, u8, u8)>,
}

impl Color {
    /// Parse a three-character micron colour form into a [`Color`].
    ///
    /// Never fails: an unparseable form yields `rgb = None` (default fallback),
    /// matching the reference `high_color` `try/except` behaviour.
    pub fn parse(raw: &str) -> Color {
        Color {
            raw: raw.to_string(),
            rgb: resolve_rgb(raw),
        }
    }
}

/// Resolve a micron colour to 24-bit RGB.
///
/// Mirrors `high_color`: six hex characters form a full `#rrggbb` true colour;
/// a three-character grayscale `gNN` maps the two decimal digits to a 0-99
/// level scaled onto 0-255; otherwise the three hex nibbles are each doubled
/// (`f` -> `0xff`).
fn resolve_rgb(raw: &str) -> Option<(u8, u8, u8)> {
    let cs: Vec<char> = raw.chars().collect();

    if cs.len() == 6 {
        // True colour: three hex byte pairs (`FT`/`BT` prefix already stripped).
        let r: String = cs[0..2].iter().collect();
        let g: String = cs[2..4].iter().collect();
        let b: String = cs[4..6].iter().collect();
        return Some((
            u8::from_str_radix(&r, 16).ok()?,
            u8::from_str_radix(&g, 16).ok()?,
            u8::from_str_radix(&b, 16).ok()?,
        ));
    }

    if cs.len() != 3 {
        return None;
    }

    if cs[0] == 'g' {
        // Grayscale: two decimal digits form a 0-99 level (reference builds
        // the urwid `gNN` spec from two `parseval_dec` digits).
        let d1 = cs[1].to_digit(10)?;
        let d2 = cs[2].to_digit(10)?;
        let level = d1 * 10 + d2; // 0..=99
        let v = (level * 255 / 100) as u8;
        Some((v, v, v))
    } else {
        // Three hex nibbles, each doubled into a byte.
        let r = cs[0].to_digit(16)?;
        let g = cs[1].to_digit(16)?;
        let b = cs[2].to_digit(16)?;
        Some(((r * 17) as u8, (g * 17) as u8, (b * 17) as u8))
    }
}
