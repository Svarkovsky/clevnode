//! Inline images: which links are pictures, how big they may be drawn, which
//! backend the terminal can take, and what to show when it can take none.
//!
//! Micron has no image construct. NomadNet's parser recognises formatting,
//! colour, sections, links, fields and tables, and that is the whole list, so
//! a page can only ever *link* to a picture in the node's file area. `lnomad`
//! therefore recognises image links by their target — a `/file/` path whose
//! name ends in a format we can decode — and draws them where the link sits.
//! There is no other signal available, and the rule is documented as the
//! heuristic it is.
//!
//! Everything here is pure. The fetching, the decoding of an actual response
//! and the drawing live in [`crate::tui`]; what this module owns is the
//! arithmetic and the decisions, so they can be tested without a terminal.

use ratatui_image::picker::ProtocolType;
use ratatui_image::FontSize;

use crate::color::ColorDepth;

/// The file extensions treated as inline images, lowercase.
///
/// Deliberately the same list the `image` dependency is compiled with: an
/// extension we would recognise but could not decode buys the reader a
/// transfer and then an error box, which is worse than a plain download link.
pub const IMAGE_EXTENSIONS: [&str; 4] = ["png", "jpg", "jpeg", "gif"];

/// How many images a single page may pull in on its own.
///
/// A page is written by somebody else. Without a ceiling, one that lists fifty
/// pictures would put fifty transfers on a link the reader may be paying for
/// in minutes of airtime. Past the ceiling the links stay links, and following
/// one still fetches it.
pub const MAX_INLINE_IMAGES: usize = 8;

/// The tallest an inline image may be drawn, in rows, before the viewport
/// bound applies — on a terminal with a graphics protocol. Keeps a portrait
/// photograph from turning a page into a scroll through one picture.
///
/// Deliberately not applied to half-blocks: there a row IS two pixels of
/// resolution, so the same cap costs detail rather than screen estate, and a
/// picture drawn small enough to fit the cap is a picture nobody can make out
/// (see the half-block sizing note on [`Backend::cell_pixels`]).
pub const MAX_IMAGE_ROWS: u16 = 24;

/// How many viewport heights a half-block picture may occupy. See
/// [`Backend::max_rows`] for why this is not 1: with half-blocks, height is
/// resolution, and a picture nobody can make out is worth less than one that
/// costs a scroll.
pub const HALFBLOCK_VIEWPORT_MULTIPLE: usize = 2;

/// How the terminal will be asked to draw pictures.
///
/// Not `Eq`: `ProtocolType` is only `PartialEq` upstream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Backend {
    /// A real graphics protocol: Kitty, iTerm2 or Sixel.
    Graphics(ProtocolType),
    /// Unicode half-blocks with foreground and background colour. Works on any
    /// colour terminal, at two pixels per cell.
    Halfblocks,
    /// No picture at all: a text box naming the file. What a monochrome
    /// terminal, `--no-color`, or an undecodable payload gets.
    Text,
}

impl Backend {
    /// How many image pixels one character cell carries, which is what
    /// "natural size" has to be measured in.
    ///
    /// A graphics protocol paints the cell's full pixel box, so the terminal's
    /// font size is the answer. Half-blocks carry exactly one pixel across and
    /// two down — the upper and lower half of the cell — whatever the font
    /// size is. Measuring half-blocks against the font size was the bug this
    /// distinction exists for: a 300x300 portrait came out as 38x38 pixels on
    /// an 8x16 font, when the same 78-column page had room for 78x44.
    pub fn cell_pixels(self, font: FontSize) -> (u16, u16) {
        match self {
            Backend::Graphics(_) => (font.width.max(1), font.height.max(1)),
            Backend::Halfblocks => (1, 2),
            // Nothing is drawn; the value is never used.
            Backend::Text => (font.width.max(1), font.height.max(1)),
        }
    }

    /// The row ceiling for an inline image on this backend, given the page's
    /// viewport height.
    ///
    /// Under a graphics protocol the cap keeps one photograph from taking over
    /// the page, and costs no detail: the pixels are there either way.
    ///
    /// Under half-blocks a row IS two pixels, so the same cap is the thing
    /// standing between the reader and a recognisable picture. A square
    /// portrait on a 24-row terminal fits 22 rows, i.e. 44 pixels tall, and
    /// the aspect ratio then holds it to 44 wide however many columns the page
    /// has. Allowing [`HALFBLOCK_VIEWPORT_MULTIPLE`] screens of height lets the
    /// WIDTH become the binding constraint instead, which is what a reader
    /// actually has to spare: the same portrait then fills 78 columns at 78x78
    /// pixels — twice the linear resolution — at the price of scrolling once.
    pub fn max_rows(self, viewport: usize) -> u16 {
        let room = viewport.saturating_sub(2).max(1);
        let bounded = match self {
            Backend::Halfblocks => room.saturating_mul(HALFBLOCK_VIEWPORT_MULTIPLE),
            _ => return MAX_IMAGE_ROWS.min(room.min(u16::MAX as usize) as u16),
        };
        bounded.min(u16::MAX as usize) as u16
    }
}

/// Choose how to draw, from what the terminal answered and how much colour it
/// has.
///
/// The ladder is: a graphics protocol if the terminal claimed one; else
/// half-blocks if there is colour to draw them with; else text. `no_color`
/// overrides everything — a reader who asked for no colour did not ask for a
/// picture painted out of coloured blocks either, and the graphics protocols
/// are excluded with it so `--no-color` output stays plain.
pub fn choose_backend(detected: ProtocolType, depth: ColorDepth, no_color: bool) -> Backend {
    if no_color {
        return Backend::Text;
    }
    match detected {
        ProtocolType::Halfblocks => match depth {
            ColorDepth::Truecolor | ColorDepth::Ansi256 => Backend::Halfblocks,
        },
        graphics => Backend::Graphics(graphics),
    }
}

/// The name a `/file/` request path points at, or `None` when the path is not
/// a file path or names nothing.
pub fn file_name(path: &str) -> Option<&str> {
    let name = path.strip_prefix("/file/")?;
    match name.is_empty() || name.contains('/') {
        true => None,
        false => Some(name),
    }
}

/// Whether a request path names a picture this browser can draw.
///
/// The extension is the only signal a micron page carries. It is compared
/// case-insensitively, so `ANTENNE.PNG` counts.
pub fn is_image_path(path: &str) -> bool {
    let Some(name) = file_name(path) else {
        return false;
    };
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&ext.as_str())
}

/// The request path part of a micron link target, ignoring the destination.
///
/// `:/file/a.png` and `<hash>:/file/a.png` both yield `/file/a.png`; a target
/// with no `:` is returned unchanged. Enough to decide whether a link is worth
/// treating as a picture, which is all this is used for — the real target is
/// built by [`crate::url::parse_url`] when the fetch is issued.
pub fn link_path(target: &str) -> &str {
    match target.rfind(':') {
        Some(i) => &target[i + 1..],
        None => target,
    }
}

/// One inline image on the current page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageSlot {
    /// The 1-based index of the link this picture belongs to.
    pub link: usize,
    /// The file name, for the status line and for saving.
    pub name: String,
    /// Where it has got to.
    pub state: SlotState,
}

/// What a decoded image turned out to be, for the caption and the text box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The detected format's short name (`PNG`, `JPEG`, `GIF`).
    pub format: &'static str,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Payload size in bytes.
    pub len: usize,
}

/// Read the format and pixel size out of an encoded image without decoding it
/// fully.
///
/// Cheap enough to run on arrival, and it is what lets a failure still say
/// something useful about what the file was.
pub fn probe(bytes: &[u8]) -> Option<Probe> {
    let reader = ::image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = match reader.format()? {
        ::image::ImageFormat::Png => "PNG",
        ::image::ImageFormat::Jpeg => "JPEG",
        ::image::ImageFormat::Gif => "GIF",
        _ => return None,
    };
    let (width, height) = reader.into_dimensions().ok()?;
    Some(Probe {
        format,
        width,
        height,
        len: bytes.len(),
    })
}

/// Decode an image payload, or say why it could not be decoded.
///
/// The bytes come from somebody else's node, so this must never panic and
/// never trust a declared size: `image` is asked for a plain decode and any
/// failure becomes a message the reader sees in the text box.
pub fn decode(bytes: &[u8]) -> Result<::image::DynamicImage, String> {
    ::image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())
}

/// The cell size an image is drawn at: its natural size in cells, shrunk to
/// fit the available width and height, never enlarged.
///
/// `cell_px` is how many image pixels one cell carries — the font size under a
/// graphics protocol, `(1, 2)` under half-blocks (see
/// [`Backend::cell_pixels`]). Everything else follows from that: a 300x300
/// picture is 38x19 cells on an 8x16 font (drawn 1:1) but 300x150 cells in
/// half-block geometry, which the shrink below then fits to the page — filling
/// the width, because on that backend every extra cell is extra resolution.
///
/// Never enlarged because upscaling a 32-pixel icon to half the terminal
/// serves nobody, and because the graphics protocols are perfectly happy to
/// draw something small. The result is at least one cell in each direction, so
/// a rounding-down can never produce an empty rectangle.
pub fn fit_cells(px: (u32, u32), cell_px: (u16, u16), max_cols: u16, max_rows: u16) -> (u16, u16) {
    let max_cols = max_cols.max(1);
    let max_rows = max_rows.max(1);
    let font_w = cell_px.0.max(1) as u32;
    let font_h = cell_px.1.max(1) as u32;

    let natural_cols = px.0.div_ceil(font_w).max(1);
    let natural_rows = px.1.div_ceil(font_h).max(1);

    // Scale down by whichever axis is tighter, so the aspect ratio survives.
    let cols_limit = natural_cols.min(max_cols as u32);
    let rows_limit = natural_rows.min(max_rows as u32);
    let by_width = (cols_limit as f64) / (natural_cols as f64);
    let by_height = (rows_limit as f64) / (natural_rows as f64);
    let scale = by_width.min(by_height).min(1.0);

    let cols = ((natural_cols as f64) * scale).floor().max(1.0) as u16;
    let rows = ((natural_rows as f64) * scale).floor().max(1.0) as u16;
    (cols.min(max_cols), rows.min(max_rows))
}

/// What is known about one inline image, for the caption line and the text
/// fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// Waiting its turn in the page's fetch queue.
    Queued,
    /// Being transferred right now.
    Loading,
    /// Transferred and decoded; drawn as a picture over `rows` rows.
    Ready {
        /// The cell size the picture is drawn at.
        cells: (u16, u16),
        /// What the payload turned out to be.
        probe: Probe,
    },
    /// Transferred but not drawable: the terminal cannot show pictures, or the
    /// payload did not decode. Either way the reader gets a text box.
    Text {
        /// What the payload turned out to be, when it could be read at all.
        probe: Option<Probe>,
        /// Why there is no picture, in one short phrase.
        reason: String,
    },
    /// Not fetched: the page asked for more pictures than [`MAX_INLINE_IMAGES`].
    TooMany,
    /// The transfer failed.
    Failed(String),
}

impl SlotState {
    /// How many rows this slot occupies in the laid-out page, over and above
    /// the line its link sits on.
    pub fn rows(&self) -> u16 {
        match self {
            SlotState::Ready { cells, .. } => cells.1,
            // Every non-picture state is one line of status under the link.
            SlotState::Queued
            | SlotState::Loading
            | SlotState::Text { .. }
            | SlotState::Failed(_) => 1,
            // Nothing is drawn and nothing is reserved: the link stands alone,
            // exactly as it would in NomadNet.
            SlotState::TooMany => 0,
        }
    }
}

/// Human-readable byte size: `842 B`, `12.4 kB`, `3.1 MB`.
pub fn human_bytes(len: usize) -> String {
    const KB: f64 = 1000.0;
    let len = len as f64;
    if len < KB {
        return format!("{len:.0} B");
    }
    if len < KB * KB {
        return format!("{:.1} kB", len / KB);
    }
    format!("{:.1} MB", len / (KB * KB))
}

/// The one-line status shown under an image link that is not (yet) a picture.
///
/// Deliberately one line, and deliberately never empty: a reader who cannot
/// see the picture still learns what it is, how big it is and that the link
/// under it will save or open it.
pub fn status_line(name: &str, state: &SlotState) -> String {
    match state {
        SlotState::Queued => format!("[{name}: queued]"),
        SlotState::Loading => format!("[{name}: loading...]"),
        SlotState::Ready { probe, .. } => describe(name, Some(probe)),
        SlotState::Text { probe, reason } => {
            format!("[{} - {reason}]", describe_inner(name, probe.as_ref()))
        }
        SlotState::TooMany => format!("[{name}: not fetched]"),
        SlotState::Failed(reason) => format!("[{name}: {reason}]"),
    }
}

/// `[name, PNG 1024x768, 84.2 kB]`, with whatever parts are known.
fn describe(name: &str, probe: Option<&Probe>) -> String {
    format!("[{}]", describe_inner(name, probe))
}

fn describe_inner(name: &str, probe: Option<&Probe>) -> String {
    match probe {
        Some(p) => format!(
            "{name}, {} {}x{}, {}",
            p.format,
            p.width,
            p.height,
            human_bytes(p.len)
        ),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> FontSize {
        FontSize {
            width: 8,
            height: 16,
        }
    }

    /// Cell geometry under a graphics protocol: the terminal's font box.
    fn graphics_cell() -> (u16, u16) {
        Backend::Graphics(ProtocolType::Kitty).cell_pixels(font())
    }

    /// Cell geometry under half-blocks: one pixel across, two down.
    fn halfblock_cell() -> (u16, u16) {
        Backend::Halfblocks.cell_pixels(font())
    }

    #[test]
    fn only_file_paths_with_a_drawable_extension_are_images() {
        assert!(is_image_path("/file/antenne.png"));
        assert!(is_image_path("/file/antenne.PNG"));
        assert!(is_image_path("/file/a.jpg"));
        assert!(is_image_path("/file/a.jpeg"));
        assert!(is_image_path("/file/a.gif"));

        // A format we cannot decode stays an ordinary download link: better a
        // link than a transfer that ends in an error box.
        assert!(!is_image_path("/file/a.webp"));
        assert!(!is_image_path("/file/a.svg"));
        assert!(!is_image_path("/file/notes.txt"));
        assert!(!is_image_path("/file/noextension"));
        // A page is a page even if somebody names one `.png`.
        assert!(!is_image_path("/page/a.png"));
        assert!(!is_image_path("/file/"));
        assert!(!is_image_path("/file/sub/a.png"));
        assert!(!is_image_path(""));
    }

    #[test]
    fn the_file_name_is_the_last_segment() {
        assert_eq!(file_name("/file/antenne.png"), Some("antenne.png"));
        assert_eq!(file_name("/page/index.mu"), None);
        assert_eq!(file_name("/file/"), None);
    }

    #[test]
    fn the_link_path_survives_both_url_spellings() {
        assert_eq!(link_path(":/file/a.png"), "/file/a.png");
        assert_eq!(
            link_path("a1b2c3d4e5f60718293a4b5c6d7e8f90:/file/a.png"),
            "/file/a.png"
        );
        assert_eq!(link_path("/file/a.png"), "/file/a.png");
    }

    #[test]
    fn the_backend_ladder_ends_in_text() {
        // A terminal that answered for a graphics protocol gets it.
        assert_eq!(
            choose_backend(ProtocolType::Kitty, ColorDepth::Truecolor, false),
            Backend::Graphics(ProtocolType::Kitty)
        );
        assert_eq!(
            choose_backend(ProtocolType::Sixel, ColorDepth::Ansi256, false),
            Backend::Graphics(ProtocolType::Sixel)
        );
        // One that did not falls back to coloured blocks.
        assert_eq!(
            choose_backend(ProtocolType::Halfblocks, ColorDepth::Truecolor, false),
            Backend::Halfblocks
        );
        assert_eq!(
            choose_backend(ProtocolType::Halfblocks, ColorDepth::Ansi256, false),
            Backend::Halfblocks
        );
        // --no-color means no picture, not a picture in fewer colours. It
        // overrides even a working graphics protocol.
        assert_eq!(
            choose_backend(ProtocolType::Halfblocks, ColorDepth::Truecolor, true),
            Backend::Text
        );
        assert_eq!(
            choose_backend(ProtocolType::Kitty, ColorDepth::Truecolor, true),
            Backend::Text
        );
    }

    #[test]
    fn a_small_image_is_drawn_at_its_natural_size() {
        // 80x32 pixels at an 8x16 font is 10 by 2 cells, well inside the page.
        assert_eq!(fit_cells((80, 32), graphics_cell(), 60, 24), (10, 2));
    }

    #[test]
    fn a_wide_image_shrinks_to_the_page_width_keeping_its_shape() {
        // 1600x800 is 200x50 cells naturally; at 40 columns the height has to
        // come down by the same factor, to 10 rows (not stay at 50).
        let (cols, rows) = fit_cells((1600, 800), graphics_cell(), 40, 100);
        assert_eq!(cols, 40);
        assert_eq!(rows, 10);
    }

    #[test]
    fn a_tall_image_shrinks_to_the_row_ceiling_keeping_its_shape() {
        // 400x1600 is 50x100 cells; capped at 10 rows it must lose width too.
        let (cols, rows) = fit_cells((400, 1600), graphics_cell(), 200, 10);
        assert_eq!(rows, 10);
        assert_eq!(cols, 5);
    }

    #[test]
    fn half_blocks_fill_the_page_width_instead_of_the_font_box() {
        // The bug this distinction exists for. A 300x300 portrait on an 8x16
        // font is 38x19 cells "naturally" — right for a graphics protocol,
        // which then paints 304x304 pixels, but on half-blocks those same
        // cells carry only 38x38 pixels. Measured in half-block geometry the
        // picture is 300x150 cells, so the fit uses the whole page width and
        // the reader gets twice the linear resolution.
        assert_eq!(fit_cells((300, 300), graphics_cell(), 78, 22), (38, 19));

        // 22 visible rows, so the half-block ceiling is 44 (two screens).
        let (cols, rows) = fit_cells((300, 300), halfblock_cell(), 78, 44);
        assert_eq!(cols, 78, "half-blocks must use the width available");
        assert_eq!(rows, 39, "height follows the aspect ratio, not the width");
        assert!(
            cols > 38,
            "the half-block picture must beat the font-box fit of 38 columns"
        );
    }

    #[test]
    fn half_blocks_still_never_enlarge_a_small_picture() {
        // Filling the width is only right while the picture has pixels to
        // spare. A 20x20 icon is 20x10 cells in half-block geometry and stays
        // there rather than being blown up to the full page.
        assert_eq!(fit_cells((20, 20), halfblock_cell(), 78, 22), (20, 10));
    }

    #[test]
    fn the_row_ceiling_follows_the_backend() {
        // A graphics protocol keeps the fixed cap so one photograph cannot
        // take over the page; half-blocks are bounded only by the viewport,
        // because there height IS resolution.
        let tall = 60;
        assert_eq!(
            Backend::Graphics(ProtocolType::Kitty).max_rows(tall),
            MAX_IMAGE_ROWS
        );
        assert_eq!(
            Backend::Halfblocks.max_rows(tall),
            ((tall - 2) * HALFBLOCK_VIEWPORT_MULTIPLE) as u16
        );
        // A short viewport still bounds both, and never yields zero rows.
        assert_eq!(
            Backend::Halfblocks.max_rows(3),
            HALFBLOCK_VIEWPORT_MULTIPLE as u16
        );
        assert_eq!(
            Backend::Halfblocks.max_rows(0),
            HALFBLOCK_VIEWPORT_MULTIPLE as u16
        );
    }

    #[test]
    fn a_tiny_image_never_collapses_to_nothing() {
        // Smaller than one cell in both directions, and a zero-sized area.
        assert_eq!(fit_cells((1, 1), graphics_cell(), 80, 24), (1, 1));
        assert_eq!(fit_cells((1600, 800), graphics_cell(), 0, 0), (1, 1));
    }

    #[test]
    fn reserved_rows_follow_the_state() {
        let probe = Probe {
            format: "PNG",
            width: 100,
            height: 50,
            len: 1234,
        };
        assert_eq!(
            SlotState::Ready {
                cells: (20, 7),
                probe
            }
            .rows(),
            7
        );
        assert_eq!(SlotState::Queued.rows(), 1);
        assert_eq!(SlotState::Loading.rows(), 1);
        assert_eq!(SlotState::Failed("timed out".into()).rows(), 1);
        assert_eq!(
            SlotState::Text {
                probe: Some(probe),
                reason: "terminal cannot show images".into()
            }
            .rows(),
            1
        );
        // Past the ceiling nothing is drawn and nothing is reserved.
        assert_eq!(SlotState::TooMany.rows(), 0);
    }

    #[test]
    fn the_status_line_says_what_the_picture_is_and_why_it_is_not_shown() {
        let probe = Probe {
            format: "PNG",
            width: 1024,
            height: 768,
            len: 84_200,
        };
        assert_eq!(
            status_line("antenne.png", &SlotState::Queued),
            "[antenne.png: queued]"
        );
        assert_eq!(
            status_line("antenne.png", &SlotState::Loading),
            "[antenne.png: loading...]"
        );
        assert_eq!(
            status_line(
                "antenne.png",
                &SlotState::Text {
                    probe: Some(probe),
                    reason: "terminal cannot show images".into(),
                }
            ),
            "[antenne.png, PNG 1024x768, 84.2 kB - terminal cannot show images]"
        );
        assert_eq!(
            status_line(
                "antenne.png",
                &SlotState::Text {
                    probe: None,
                    reason: "not a decodable image".into(),
                }
            ),
            "[antenne.png - not a decodable image]"
        );
        assert_eq!(
            status_line("antenne.png", &SlotState::Failed("timed out".into())),
            "[antenne.png: timed out]"
        );
        assert_eq!(
            status_line("antenne.png", &SlotState::TooMany),
            "[antenne.png: not fetched]"
        );
    }

    #[test]
    fn byte_sizes_read_like_sizes() {
        assert_eq!(human_bytes(842), "842 B");
        assert_eq!(human_bytes(12_400), "12.4 kB");
        assert_eq!(human_bytes(3_100_000), "3.1 MB");
    }

    #[test]
    fn probing_reads_the_format_and_size_and_refuses_rubbish() {
        let png = tiny_png();
        let probe = probe(&png).expect("a valid PNG must probe");
        assert_eq!(probe.format, "PNG");
        assert_eq!((probe.width, probe.height), (4, 4));
        assert_eq!(probe.len, png.len());

        assert!(probe_is_none(b"not an image at all"));
        // A truncated PNG keeps its header, so probing still works while
        // decoding must fail: that is the case the text box exists for.
        let truncated = &png[..png.len() / 2];
        assert!(decode(truncated).is_err());
    }

    #[test]
    fn decoding_a_valid_image_yields_its_pixels() {
        let img = decode(&tiny_png()).expect("decode");
        assert_eq!((img.width(), img.height()), (4, 4));
    }

    fn probe_is_none(bytes: &[u8]) -> bool {
        probe(bytes).is_none()
    }

    /// A 4x4 PNG, encoded here rather than committed as a fixture so the test
    /// data cannot drift from what the decoder is asked to read.
    fn tiny_png() -> Vec<u8> {
        let mut img = ::image::RgbaImage::new(4, 4);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = ::image::Rgba([(x * 60) as u8, (y * 60) as u8, 128, 255]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        ::image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ::image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }
}
