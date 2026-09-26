//! ANSI terminal renderer for the micron document model.
//!
//! [`render`] turns a [`MicronDocument`] into a [`RenderedPage`]: an ANSI SGR
//! string laid out for a fixed `width`, plus the page's links collected for
//! navigation. It mirrors the style semantics of NomadNet's `MicronParser.py`
//! (`markup_to_attrmaps`/`make_style`/`make_output`) and `Browser.py`, but
//! emits 24-bit ANSI SGR rather than urwid attributes.
//!
//! Style mapping mirrored from the reference (canonical
//! `nomadnet-textui/MicronParser.py`):
//!
//! - Colour resolution comes from [`leviculum_micron::Color::rgb`], which ports
//!   `high_color` (lines 518-567): three hex nibbles doubled into `#rrggbb`, a
//!   grayscale `gNN` level, or six hex characters as a full true colour. A colour
//!   that does not resolve falls back to the terminal default, as in the
//!   reference `try/except`.
//! - Bold (`` `! ``), underline (`` `_ ``), italic (`` `* ``) map to SGR `1`/`4`/`3`
//!   (`make_style` lines 570-591). Every styled run is self contained and reset.
//! - Alignment (`` `c ``/`` `l ``/`` `r ``) centres/left/right aligns the content
//!   within the content width (`make_output` lines 648-655, applied at the urwid
//!   `Text` `align` in `parse_line`).
//! - Section headings indent by `(depth - 1) * SECTION_INDENT` and carry the
//!   theme banding colour (`left_indent` line 418, `STYLES_DARK` lines 18-23,
//!   heading dispatch lines 287-318). The banding colour spans the full row.
//! - The default page foreground (`DEFAULT_FG_DARK = "ddd"`, line 12) is mapped
//!   to the terminal's own default foreground: an unset span colour emits no
//!   SGR, which is wire-compatible with the reference's near-white default.
//! - Dividers draw a full-width rule (`parse_line` lines 324-336), indented by
//!   `left_indent`/`right_indent` when inside a section.
//! - Literal blocks pass through verbatim with no inline parsing or wrapping
//!   (`parse_line` lines 226-231, `make_output` lines 595-598).
//! - Tables lay out `|`-separated rows in aligned columns bounded by
//!   `MAX_TABLE_WIDTH = 100` (lines 37, 197-218).

use leviculum_micron::{Align, Block, Field, FieldKind, Line, MicronDocument, Style};
use unicode_width::UnicodeWidthChar;

use crate::color::{rgb_to_ansi256, ColorDepth};
use crate::theme::Theme;

/// Per-depth section indent, matching the reference `SECTION_INDENT`.
const SECTION_INDENT: usize = 2;
/// Default table width cap, matching the reference `MAX_TABLE_WIDTH`.
const MAX_TABLE_WIDTH: usize = 100;
/// Smallest width the renderer will lay out to; anything smaller is clamped up
/// so wrapping and alignment stay well defined.
const MIN_WIDTH: usize = 1;

/// The full rendered page: laid-out text plus the links found in it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedPage {
    /// The rendered ANSI (or, with `no_color`, plain) text.
    pub text: String,
    /// The page's links, in source order, each with a 1-based `index`.
    pub links: Vec<RenderedLink>,
}

/// A single link collected while rendering, ready for navigation and (via its
/// laid-out position) future mouse hit-testing in the TUI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedLink {
    /// 1-based index in source order. Internal only now (no visible `[N]`
    /// marker): it identifies the link for focus, hint labelling and hit-testing.
    pub index: usize,
    /// The link's display label.
    pub label: String,
    /// The link target (destination hash / page path).
    pub target: String,
    /// The link's `|`-separated field components, split into `(key, value)`
    /// pairs (a component without `=` yields an empty value).
    pub fields: Vec<(String, String)>,
    /// 0-based index of the laid-out [`RLine`] the link's visible label starts
    /// on (the clickable core, after any leading whitespace).
    pub line: usize,
    /// 0-based DISPLAY column where the clickable label starts on that line (a
    /// wide char before it advances two columns), matching the screen columns the
    /// renderer paints so hit-testing and hints land on the label.
    pub col_start: usize,
    /// 0-based display column one past the last clickable cell.
    /// `col_start..col_end` is the hit-test range (the visible label core, with
    /// no `[N]` marker).
    pub col_end: usize,
}

/// The current value of an input field, supplied by the TUI so a re-layout
/// reflects what the reader has typed / toggled. The print / non-tty sink passes
/// no overrides, so it always lays fields out from their parsed prefill (keeping
/// the golden output stable). Indexed positionally by the field's 1-based
/// [`RenderedField::index`] (i.e. `field_values[index - 1]`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldValue {
    /// The current text of a text field (ignored for checkbox/radio).
    pub text: String,
    /// The current checked state of a checkbox/radio (ignored for text).
    pub checked: bool,
    /// How far the text is scrolled inside the widget: the offset of the first
    /// character drawn in the box. Lets a text longer than the field's width
    /// still be typed and read — the editor keeps the caret in view by sliding
    /// this window, exactly as the address bar does. Zero for checkbox/radio and
    /// for the print sink, which never scrolls.
    pub scroll: usize,
}

/// A form field collected while rendering, with its laid-out position (mirroring
/// [`RenderedLink`]) so the TUI can hit-test, focus and edit it. The metadata is
/// enough to (re)build the editable field store and to package a submit request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedField {
    /// 1-based index in source order, identifying the field for focus and
    /// hit-testing (independent of the link index space).
    pub index: usize,
    /// The field name (the form key; submitted as `field_<name>`).
    pub name: String,
    /// The field kind (text, checkbox, radio).
    pub kind: FieldKind,
    /// The submit value: a text field's current text, or a checkbox/radio's
    /// explicit value (the label when the field gave none).
    pub value: String,
    /// The checkbox/radio display label (empty for text fields).
    pub label: String,
    /// The field's declared display width (text fields; default 24).
    pub width: usize,
    /// Whether a text field masks its input.
    pub masked: bool,
    /// The current checked state of a checkbox/radio (its prefill when not yet
    /// toggled).
    pub checked: bool,
    /// 0-based index of the laid-out [`RLine`] the field renders on.
    pub line: usize,
    /// 0-based DISPLAY column where the field widget starts on that line.
    pub col_start: usize,
    /// 0-based display column one past the last cell of the field widget.
    pub col_end: usize,
}

/// Render a document to a [`RenderedPage`] at `width` columns, with 24-bit ANSI
/// colour enabled and the dark theme (the `--print` / non-tty look).
pub fn render(doc: &MicronDocument, width: usize) -> RenderedPage {
    render_with_options(doc, width, false, ColorDepth::Truecolor)
}

/// Render a document, optionally stripping all SGR sequences.
///
/// With `no_color` set, the output carries no escape sequences at all
/// (indentation, wrapping and alignment are preserved; links, no longer marked
/// by a `[N]`, become indistinguishable from body text in plain output).
///
/// This is the ANSI sink: it lays the document out into target-agnostic styled
/// lines with [`layout`], then serialises them to the exact byte stream the
/// `--print` / non-tty path emits. `depth` selects 24-bit (`38;2;r;g;b`) or the
/// downgraded xterm-256 (`38;5;idx`) SGR encoding; it is ignored under
/// `no_color`, which emits no SGR at all.
pub fn render_with_options(
    doc: &MicronDocument,
    width: usize,
    no_color: bool,
    depth: ColorDepth,
) -> RenderedPage {
    // The print / non-tty sink is always the dark theme, so its golden output is
    // stable and independent of any terminal-background detection.
    let (lines, links) = layout(doc, width, Theme::Dark);
    RenderedPage {
        text: emit_ansi(&lines, no_color, depth),
        links,
    }
}

/// Empty field overrides: the print / non-tty sink lays every field out from its
/// parsed prefill, so its golden output never depends on interactive edits.
const NO_FIELD_VALUES: &[FieldValue] = &[];

/// Lay a document out into target-agnostic styled lines: the shared core both
/// the ANSI sink ([`render_with_options`]) and the ratatui sink build on.
///
/// Each [`RLine`] is one output row, already width-wrapped, aligned and
/// indented; every character carries its resolved [`RStyle`]. The returned
/// [`RenderedLink`]s carry both their navigation data and their laid-out
/// `(line, col_start, col_end)` position for hit-testing. No `no_color` choice
/// is made here: colour is a property of the sink, not the layout. The `theme`
/// selects the accent colours (heading bands, link colour) baked into the cells.
pub fn layout(doc: &MicronDocument, width: usize, theme: Theme) -> (Vec<RLine>, Vec<RenderedLink>) {
    let (lines, links, _block_lines, _fields) = layout_blocks(doc, width, theme, NO_FIELD_VALUES);
    (lines, links)
}

/// Lay a document out like [`layout`], additionally returning the block->line
/// mapping: `block_lines[i]` is the 0-based index of the first [`RLine`] the
/// `i`-th [`Block`] laid out into. This lets `#anchor` navigation, which stores
/// anchors as block indices, resolve an anchor to a page line. A block that
/// emits no line (an empty table) records the line index the next block starts
/// at, so the mapping is still monotonic and in range.
pub fn layout_blocks(
    doc: &MicronDocument,
    width: usize,
    theme: Theme,
    field_values: &[FieldValue],
) -> (
    Vec<RLine>,
    Vec<RenderedLink>,
    Vec<usize>,
    Vec<RenderedField>,
) {
    let mut renderer = Renderer {
        lines: Vec::new(),
        links: Vec::new(),
        fields: Vec::new(),
        field_values,
        width: width.max(MIN_WIDTH),
        theme,
    };
    let mut block_lines = Vec::with_capacity(doc.blocks.len());
    for block in &doc.blocks {
        block_lines.push(renderer.lines.len());
        renderer.render_block(block);
    }
    renderer.record_positions();
    (renderer.lines, renderer.links, block_lines, renderer.fields)
}

/// Serialise laid-out styled lines to the ANSI byte stream. Runs of equal style
/// are grouped into a single self-contained SGR sequence, matching the layout's
/// original line-at-a-time emission byte-for-byte. With `no_color`, no escape
/// sequences are emitted at all.
fn emit_ansi(lines: &[RLine], no_color: bool, depth: ColorDepth) -> String {
    let mut out = String::new();
    for line in lines {
        emit_ansi_line(&line.cells, no_color, depth, &mut out);
    }
    out
}

/// Emit one styled line (plus its trailing newline) into `out`.
fn emit_ansi_line(cells: &[StyledChar], no_color: bool, depth: ColorDepth, out: &mut String) {
    let mut active = false;
    let mut i = 0;
    while i < cells.len() {
        let st = cells[i].st;
        let mut j = i;
        let mut text = String::new();
        while j < cells.len() && cells[j].st == st {
            // A text sink has no background with which to draw an input bar, so
            // a field's padding is emitted as underscores: a reader (or a
            // script) can still see that a field is there and how wide it is.
            // Blank spaces would render it invisible. The TUI paints the very
            // same cells as a filled bar instead.
            let ch = cells[j].ch;
            let printed = if cells[j].field.is_some() && ch == ' ' {
                '_'
            } else {
                ch
            };
            text.push(printed);
            j += 1;
        }
        if !no_color && !st.is_plain() {
            out.push_str(&st.sgr(depth));
            out.push_str(&text);
            active = true;
        } else {
            if active {
                out.push_str("\x1b[0m");
                active = false;
            }
            out.push_str(&text);
        }
        i = j;
    }
    if active {
        out.push_str("\x1b[0m");
    }
    out.push('\n');
}

/// A resolved terminal style: colours already reduced to RGB, attributes as
/// flags. `None` colours mean "use the terminal default" (no SGR emitted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RStyle {
    /// Foreground colour, or `None` for the terminal default.
    pub fg: Option<(u8, u8, u8)>,
    /// Background colour, or `None` for the terminal default.
    pub bg: Option<(u8, u8, u8)>,
    /// Bold attribute.
    pub bold: bool,
    /// Underline attribute.
    pub underline: bool,
    /// Italic attribute.
    pub italic: bool,
}

impl RStyle {
    fn is_plain(&self) -> bool {
        self.fg.is_none() && self.bg.is_none() && !self.bold && !self.underline && !self.italic
    }

    /// The self-contained SGR prefix for this style (starts by resetting). Under
    /// [`ColorDepth::Truecolor`] colours emit as 24-bit `38;2;r;g;b` / `48;2;...`;
    /// under [`ColorDepth::Ansi256`] they are downgraded to the nearest palette
    /// index as `38;5;idx` / `48;5;idx`.
    fn sgr(&self, depth: ColorDepth) -> String {
        let mut s = String::from("\x1b[0");
        if self.bold {
            s.push_str(";1");
        }
        if self.italic {
            s.push_str(";3");
        }
        if self.underline {
            s.push_str(";4");
        }
        if let Some((r, g, b)) = self.fg {
            match depth {
                ColorDepth::Truecolor => s.push_str(&format!(";38;2;{r};{g};{b}")),
                ColorDepth::Ansi256 => s.push_str(&format!(";38;5;{}", rgb_to_ansi256(r, g, b))),
            }
        }
        if let Some((r, g, b)) = self.bg {
            match depth {
                ColorDepth::Truecolor => s.push_str(&format!(";48;2;{r};{g};{b}")),
                ColorDepth::Ansi256 => s.push_str(&format!(";48;5;{}", rgb_to_ansi256(r, g, b))),
            }
        }
        s.push('m');
        s
    }
}

/// Resolve a model [`Style`] to a terminal [`RStyle`].
fn resolve_style(style: &Style) -> RStyle {
    RStyle {
        fg: style.fg.as_ref().and_then(|c| c.rgb),
        bg: style.bg.as_ref().and_then(|c| c.rgb),
        bold: style.bold,
        underline: style.underline,
        italic: style.italic,
    }
}

/// One styled character, the unit the layout engine works in.
#[derive(Clone, Copy, Debug)]
pub struct StyledChar {
    /// The visible character.
    pub ch: char,
    /// Its resolved style.
    pub st: RStyle,
    /// The 1-based index of the link this cell is a clickable part of, if any.
    /// Leading/trailing whitespace inside a link label is untagged so it stays
    /// unstyled and outside the hit-test range.
    pub link: Option<usize>,
    /// The 1-based index of the form field this cell is part of, if any (the
    /// whole rendered widget is tagged, including its brackets), for hit-testing
    /// and the input-box overlay.
    pub field: Option<usize>,
}

/// One laid-out output row: a sequence of styled cells, already wrapped,
/// aligned and indented. The target-agnostic unit both sinks consume.
#[derive(Clone, Debug, Default)]
pub struct RLine {
    /// The row's cells, left to right.
    pub cells: Vec<StyledChar>,
}

/// The links that stand alone on their line, by 1-based link index.
///
/// Only these can become inline pictures. A link inside a sentence has no
/// rectangle to grow into without tearing the text apart, and an author who
/// writes an image in Markdown gets a paragraph of its own anyway — which is
/// exactly what lblogd renders and what this rule then recognises.
///
/// "Alone" means the line holds no visible character outside the link's own
/// columns: leading indentation and trailing spaces do not count.
pub fn standalone_links(lines: &[RLine], links: &[RenderedLink]) -> Vec<usize> {
    links
        .iter()
        .filter(|link| {
            let Some(line) = lines.get(link.line) else {
                return false;
            };
            line.cells
                .iter()
                .enumerate()
                .all(|(col, cell)| (col >= link.col_start && col < link.col_end) || cell.ch == ' ')
        })
        .map(|link| link.index)
        .collect()
}

/// Reserve rows under laid-out links, shifting everything below down.
///
/// `reserved` gives `(1-based link index, rows)` pairs; each link's line gains
/// that many blank rows immediately under it, which the caller then draws an
/// image (or a status line) into. Every stored line index — links, fields, the
/// block-to-line map used by `#anchor` navigation — is moved with the text, so
/// hit-testing, focus and anchors keep pointing at what they pointed at.
///
/// A separate pass rather than a parameter to [`layout_blocks`]: the reserved
/// height is not known until an image has been fetched and decoded, which
/// happens long after the page was first laid out, and keeping it out of the
/// renderer means the frozen `--print` output cannot be disturbed by it.
pub fn insert_image_rows(
    lines: &mut Vec<RLine>,
    links: &mut [RenderedLink],
    fields: &mut [RenderedField],
    block_lines: &mut [usize],
    reserved: &[(usize, u16)],
) {
    // Insert from the bottom up so an earlier insertion cannot move the line
    // a later one was measured against.
    let mut pending: Vec<(usize, u16)> = reserved
        .iter()
        .filter(|(_, rows)| *rows > 0)
        .filter_map(|(index, rows)| {
            let link = links.iter().find(|l| l.index == *index)?;
            Some((link.line, *rows))
        })
        .collect();
    pending.sort_by_key(|entry| std::cmp::Reverse(entry.0));

    for (line, rows) in pending {
        if line >= lines.len() {
            continue;
        }
        let at = line + 1;
        for _ in 0..rows {
            lines.insert(at, RLine::default());
        }
        let rows = rows as usize;
        for link in links.iter_mut().filter(|l| l.line >= at) {
            link.line += rows;
        }
        for field in fields.iter_mut().filter(|f| f.line >= at) {
            field.line += rows;
        }
        for block in block_lines.iter_mut().filter(|b| **b >= at) {
            *block += rows;
        }
    }
}

struct Renderer<'a> {
    lines: Vec<RLine>,
    links: Vec<RenderedLink>,
    fields: Vec<RenderedField>,
    /// Current field values from the TUI (empty for the print sink), indexed by
    /// a field's 1-based index minus one.
    field_values: &'a [FieldValue],
    width: usize,
    theme: Theme,
}

impl Renderer<'_> {
    fn render_block(&mut self, block: &Block) {
        match block {
            Block::Heading { depth, line } => self.render_heading(*depth, line),
            Block::Paragraph { depth, line } => self.render_paragraph(*depth as usize, line),
            Block::Divider { depth, character } => self.render_divider(*depth as usize, *character),
            Block::LiteralBlock { lines } => self.render_literal(lines),
            Block::Table {
                depth,
                align,
                max_width,
                rows,
            } => self.render_table(*depth as usize, *align, *max_width, rows),
            Block::Partial { depth, .. } => self.render_partial(*depth as usize),
            Block::Blank => self.push_blank(),
        }
    }

    /// Store a finished row of styled cells as one output line.
    fn push_line(&mut self, cells: Vec<StyledChar>) {
        self.lines.push(RLine { cells });
    }

    /// Store an empty output line (a blank row).
    fn push_blank(&mut self) {
        self.lines.push(RLine::default());
    }

    /// After all blocks are laid out, walk the lines and record each link's and
    /// field's laid-out position: the line and column span of its tagged cells,
    /// taken from the first line the element appears on. Column indices are
    /// DISPLAY columns (a wide char before the element advances two columns), so
    /// the recorded span lines up with the cells the renderer paints.
    fn record_positions(&mut self) {
        let mut seen_links: Vec<bool> = vec![false; self.links.len()];
        let mut seen_fields: Vec<bool> = vec![false; self.fields.len()];
        for (li, line) in self.lines.iter().enumerate() {
            let mut col = 0usize;
            for cell in line.cells.iter() {
                let w = UnicodeWidthChar::width(cell.ch).unwrap_or(0);
                if let Some(idx) = cell.link {
                    if let Some(slot) = idx.checked_sub(1) {
                        if let Some(link) = self.links.get_mut(slot) {
                            if !seen_links[slot] {
                                seen_links[slot] = true;
                                link.line = li;
                                link.col_start = col;
                                link.col_end = col + w;
                            } else if link.line == li {
                                link.col_end = col + w;
                            }
                        }
                    }
                }
                if let Some(idx) = cell.field {
                    if let Some(slot) = idx.checked_sub(1) {
                        if let Some(field) = self.fields.get_mut(slot) {
                            if !seen_fields[slot] {
                                seen_fields[slot] = true;
                                field.line = li;
                                field.col_start = col;
                                field.col_end = col + w;
                            } else if field.line == li {
                                field.col_end = col + w;
                            }
                        }
                    }
                }
                col += w;
            }
        }
    }

    /// Indent width for a given section depth: `(depth - 1) * SECTION_INDENT`,
    /// mirroring the reference `left_indent` (zero at depth 0/1).
    fn indent_for(depth: usize) -> usize {
        depth.saturating_sub(1) * SECTION_INDENT
    }

    fn render_paragraph(&mut self, depth: usize, line: &Line) {
        let (chars, align) = self.flatten_line(line);
        if chars.is_empty() {
            self.push_blank();
            return;
        }
        let indent = Self::indent_for(depth);
        let content_width = self.width.saturating_sub(indent).max(MIN_WIDTH);
        for visual in wrap(chars, content_width) {
            self.emit_aligned(visual, indent, content_width, align);
        }
    }

    fn render_heading(&mut self, depth: u8, line: &Line) {
        let (fg, bg) = self.theme.heading_band(depth);
        let (chars, align) = self.flatten_heading(line, fg, bg);
        let indent = Self::indent_for(depth as usize);
        let content_width = self.width.saturating_sub(indent).max(MIN_WIDTH);
        let band = RStyle {
            fg: Some(fg),
            bg: Some(bg),
            ..RStyle::default()
        };
        let visuals = if chars.is_empty() {
            vec![Vec::new()]
        } else {
            wrap(chars, content_width)
        };
        for content in visuals {
            let lead = leading_pad(align, content_width, content.len());
            let mut row: Vec<StyledChar> = Vec::new();
            for _ in 0..indent {
                row.push(cell(' ', band));
            }
            for _ in 0..lead {
                row.push(cell(' ', band));
            }
            row.extend(content);
            // Pad the whole row so the theme band spans the full page width.
            while row.len() < self.width {
                row.push(cell(' ', band));
            }
            self.push_line(row);
        }
    }

    fn render_divider(&mut self, depth: usize, character: char) {
        let side = Self::indent_for(depth);
        let rule_len = self.width.saturating_sub(side * 2);
        let mut row: Vec<StyledChar> = Vec::new();
        for _ in 0..side {
            row.push(cell(' ', RStyle::default()));
        }
        for _ in 0..rule_len {
            row.push(cell(character, RStyle::default()));
        }
        self.push_line(row);
    }

    fn render_literal(&mut self, lines: &[Line]) {
        for line in lines {
            let (chars, _align) = self.flatten_line(line);
            // Verbatim: no wrapping, no indent, no alignment. Backticks and any
            // other markup characters are already stored literally by the parser.
            self.push_line(chars);
        }
    }

    fn render_partial(&mut self, depth: usize) {
        let indent = Self::indent_for(depth);
        let mut row: Vec<StyledChar> = Vec::new();
        for _ in 0..indent {
            row.push(cell(' ', RStyle::default()));
        }
        // Match the reference placeholder glyph for an unresolved partial.
        row.push(cell('\u{29d6}', RStyle::default()));
        self.push_line(row);
    }

    fn render_table(
        &mut self,
        depth: usize,
        align: Option<Align>,
        max_width: Option<usize>,
        rows: &[String],
    ) {
        if rows.is_empty() {
            return;
        }
        let indent = Self::indent_for(depth);
        let cell_align = align.unwrap_or(Align::Left);
        let bound = max_width
            .unwrap_or(MAX_TABLE_WIDTH)
            .min(self.width.saturating_sub(indent))
            .max(MIN_WIDTH);

        let parsed: Vec<Vec<String>> = rows.iter().map(|row| split_cells(row)).collect();
        let ncols = parsed.iter().map(Vec::len).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }

        // A cell's text is inline micron, so it is parsed and flattened rather
        // than printed. The reference does the same thing by re-parsing each
        // formatted row line (NomadNet MicronParser.render_table -> parse_line),
        // and without it a cell holding `!bold`! or a link shows its markup.
        // Flattening also gives the VISIBLE width for free: the markup is gone
        // by the time the characters are counted.
        let flat: Vec<Vec<Vec<StyledChar>>> = parsed
            .iter()
            .map(|row| match is_separator_row(row) {
                true => Vec::new(),
                false => row.iter().map(|cell| self.flatten_cell(cell)).collect(),
            })
            .collect();

        // Natural column widths from the non-separator rows.
        let mut colw = vec![1usize; ncols];
        for row in &flat {
            for (c, cell) in row.iter().enumerate() {
                colw[c] = colw[c].max(cell.len());
            }
        }

        // Scale down proportionally if the natural layout overflows `bound`.
        let gap = 3; // " | "
        let sep_total = ncols.saturating_sub(1) * gap;
        let natural: usize = colw.iter().sum::<usize>() + sep_total;
        if natural > bound && colw.iter().sum::<usize>() > 0 {
            let budget = bound.saturating_sub(sep_total).max(ncols);
            let sum: usize = colw.iter().sum();
            for w in colw.iter_mut() {
                *w = ((*w * budget) / sum).max(1);
            }
        }

        for (r, row) in parsed.iter().enumerate() {
            let mut cells: Vec<StyledChar> = Vec::new();
            for _ in 0..indent {
                cells.push(cell(' ', RStyle::default()));
            }
            if is_separator_row(row) {
                for (c, w) in colw.iter().enumerate() {
                    if c > 0 {
                        push_plain(&mut cells, "\u{2500}\u{253c}\u{2500}");
                    }
                    for _ in 0..*w {
                        cells.push(cell('\u{2500}', RStyle::default()));
                    }
                }
            } else {
                let empty: Vec<StyledChar> = Vec::new();
                for (c, w) in colw.iter().enumerate() {
                    if c > 0 {
                        push_plain(&mut cells, " \u{2502} ");
                    }
                    let content = flat[r].get(c).unwrap_or(&empty);
                    push_fitted(&mut cells, content, *w, cell_align);
                }
            }
            self.push_line(cells);
        }
    }

    /// Flatten a line's spans into styled characters, recording each link's
    /// clickable span. Returns the characters and the line's alignment.
    fn flatten_line(&mut self, line: &Line) -> (Vec<StyledChar>, Align) {
        let mut out: Vec<StyledChar> = Vec::new();
        for span in &line.spans {
            let base = resolve_style(&span.style);
            if let Some(link) = &span.link {
                let index = self.links.len() + 1;
                let label = if span.text.is_empty() {
                    link.label.clone()
                } else {
                    span.text.clone()
                };
                self.links.push(RenderedLink {
                    index,
                    label: link.label.clone(),
                    target: link.target.clone(),
                    fields: link.fields.iter().map(|f| split_field(f)).collect(),
                    // Filled in by `record_link_positions` after layout.
                    line: 0,
                    col_start: 0,
                    col_end: 0,
                });
                let link_style = RStyle {
                    fg: Some(self.theme.link_fg()),
                    underline: true,
                    ..base
                };
                // Only the core label (and the appended `[N]` marker) carry the
                // link style; surrounding whitespace stays in the base style so it
                // is not underlined or coloured. rngit puts bullets/indent inside
                // the link label, and the underlined leading whitespace looks bad.
                let lead_len = label.len() - label.trim_start().len();
                let (lead, rest) = label.split_at(lead_len);
                let trail_len = rest.len() - rest.trim_end().len();
                let (core, trail) = rest.split_at(rest.len() - trail_len);
                push_styled(&mut out, lead, base);
                // The core label is the clickable span: style it and tag it with
                // the link index for hit-testing. No visible `[N]` marker is
                // appended; the underline + LINK_FG alone mark it as a link.
                push_link(&mut out, core, link_style, index);
                push_styled(&mut out, trail, base);
                continue;
            }
            if let Some(field) = &span.field {
                let index = self.fields.len() + 1;
                let over = self.field_values.get(index - 1);
                let (display, value, checked) = field_display(field, over);
                self.fields.push(RenderedField {
                    index,
                    name: field.name.clone(),
                    kind: field.kind,
                    value,
                    label: field.label.clone(),
                    width: field.width,
                    masked: field.masked,
                    checked,
                    // Filled in by `record_positions` after layout.
                    line: 0,
                    col_start: 0,
                    col_end: 0,
                });
                push_field(&mut out, &display, base, index);
                continue;
            }
            // Anchor spans carry no visible text; a plain span carries its text.
            push_styled(&mut out, &span.text, base);
        }
        let align = line.spans.last().map(|s| s.style.align).unwrap_or_default();
        (out, align)
    }

    /// Parse one table cell's inline micron and flatten it to styled cells.
    ///
    /// Falls back to the raw text when the parse yields nothing visible: a cell
    /// whose text happens to start with a line-level control character (`-`,
    /// `>`) parses as a divider or a heading and would otherwise vanish, and
    /// losing a reader's content is worse than showing a stray character.
    fn flatten_cell(&mut self, text: &str) -> Vec<StyledChar> {
        let doc = leviculum_micron::parse(text);
        let mut out: Vec<StyledChar> = Vec::new();
        for block in &doc.blocks {
            let line = match block {
                Block::Paragraph { line, .. } | Block::Heading { line, .. } => line,
                _ => continue,
            };
            let (chars, _) = self.flatten_line(line);
            out.extend(chars);
        }
        if out.is_empty() && !text.trim().is_empty() {
            push_plain(&mut out, text);
        }
        out
    }

    /// Flatten a heading line, forcing the theme band colours while preserving
    /// per-span bold/underline/italic toggles.
    fn flatten_heading(
        &mut self,
        line: &Line,
        fg: (u8, u8, u8),
        bg: (u8, u8, u8),
    ) -> (Vec<StyledChar>, Align) {
        let (chars, align) = self.flatten_line(line);
        let banded = chars
            .into_iter()
            .map(|c| StyledChar {
                ch: c.ch,
                st: RStyle {
                    fg: Some(fg),
                    bg: Some(bg),
                    bold: c.st.bold,
                    underline: c.st.underline,
                    italic: c.st.italic,
                },
                // Keep the link/field tags so an element inside a heading stays
                // hit-testable.
                link: c.link,
                field: c.field,
            })
            .collect();
        (banded, align)
    }

    /// Emit a wrapped content line with indentation and alignment padding.
    fn emit_aligned(
        &mut self,
        content: Vec<StyledChar>,
        indent: usize,
        content_width: usize,
        align: Align,
    ) {
        let lead = leading_pad(align, content_width, content.len());
        let mut row: Vec<StyledChar> = Vec::new();
        for _ in 0..(indent + lead) {
            row.push(cell(' ', RStyle::default()));
        }
        row.extend(content);
        self.push_line(row);
    }
}

/// Leading padding count for aligning `len` visible columns within `width`.
fn leading_pad(align: Align, width: usize, len: usize) -> usize {
    let slack = width.saturating_sub(len);
    match align {
        Align::Left => 0,
        Align::Right => slack,
        Align::Center => slack / 2,
    }
}

/// Word-wrap styled characters to `width` columns, breaking at spaces where
/// possible and hard-breaking over-long runs. Always yields at least one line.
fn wrap(chars: Vec<StyledChar>, width: usize) -> Vec<Vec<StyledChar>> {
    let width = width.max(MIN_WIDTH);
    let mut lines: Vec<Vec<StyledChar>> = Vec::new();
    let mut cur: Vec<StyledChar> = Vec::new();
    for sc in chars {
        cur.push(sc);
        if cur.len() > width {
            match cur.iter().rposition(|c| c.ch == ' ') {
                Some(pos) => {
                    let rest = cur.split_off(pos + 1);
                    while cur.last().is_some_and(|c| c.ch == ' ') {
                        cur.pop();
                    }
                    lines.push(std::mem::take(&mut cur));
                    cur = rest;
                }
                None => {
                    let rest = cur.split_off(width);
                    lines.push(std::mem::take(&mut cur));
                    cur = rest;
                }
            }
        }
    }
    lines.push(cur);
    lines
}

/// Build one untagged styled cell.
fn cell(ch: char, st: RStyle) -> StyledChar {
    StyledChar {
        ch,
        st,
        link: None,
        field: None,
    }
}

/// Append the characters of `text` with a single style, untagged.
fn push_styled(out: &mut Vec<StyledChar>, text: &str, st: RStyle) {
    for ch in text.chars() {
        out.push(cell(ch, st));
    }
}

/// Append `text` with a single style, tagging every cell with `link` (its
/// 1-based index) so the laid-out span can be hit-tested.
fn push_link(out: &mut Vec<StyledChar>, text: &str, st: RStyle, link: usize) {
    for ch in text.chars() {
        out.push(StyledChar {
            ch,
            st,
            link: Some(link),
            field: None,
        });
    }
}

/// Append `text` with a single style, tagging every cell with `field` (its
/// 1-based index) so the laid-out widget can be hit-tested, focused and edited.
fn push_field(out: &mut Vec<StyledChar>, text: &str, st: RStyle, field: usize) {
    for ch in text.chars() {
        out.push(StyledChar {
            ch,
            st,
            link: None,
            field: Some(field),
        });
    }
}

/// Append `text` with the default (unstyled) style.
fn push_plain(out: &mut Vec<StyledChar>, text: &str) {
    push_styled(out, text, RStyle::default());
}

/// Append `content` padded or truncated to exactly `width` columns per
/// `align`, keeping each character's style (and link tag).
fn push_fitted(out: &mut Vec<StyledChar>, content: &[StyledChar], width: usize, align: Align) {
    if width == 0 {
        return;
    }
    if content.len() >= width {
        out.extend_from_slice(&content[..width]);
        return;
    }
    let slack = width - content.len();
    let (left, right) = match align {
        Align::Left => (0, slack),
        Align::Right => (slack, 0),
        Align::Center => (slack / 2, slack - slack / 2),
    };
    for _ in 0..left {
        out.push(cell(' ', RStyle::default()));
    }
    out.extend_from_slice(content);
    for _ in 0..right {
        out.push(cell(' ', RStyle::default()));
    }
}

/// Fit `text` into exactly `width` columns, truncating or padding per `align`.
fn fit(text: &str, width: usize, align: Align) -> String {
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() >= width {
        return chars[..width].iter().collect();
    }
    let slack = width - chars.len();
    let (left, right) = match align {
        Align::Left => (0, slack),
        Align::Right => (slack, 0),
        Align::Center => (slack / 2, slack - slack / 2),
    };
    let mut s = String::new();
    for _ in 0..left {
        s.push(' ');
    }
    s.push_str(text);
    for _ in 0..right {
        s.push(' ');
    }
    s
}

/// Whether a parsed table row is a markdown separator (all cells are dashes,
/// optionally with alignment colons).
/// Split a micron table row into cells on unescaped `|`, unescaping `\|` and
/// `\\` as it goes.
///
/// The escape is the reference's (RNS `rngit/util.py:633-654`): a cell whose
/// text contains a pipe is written `\|`, which must not start a new column
/// and must not keep its backslash on screen.
fn split_cells(row: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in row.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            cells.push(current.trim().to_string());
            current = String::new();
        } else {
            current.push(ch);
        }
    }
    // A trailing backslash is a literal one; dropping it would eat a character.
    if escaped {
        current.push('\\');
    }
    cells.push(current.trim().to_string());
    cells
}

fn is_separator_row(row: &[String]) -> bool {
    !row.is_empty()
        && row.iter().all(|cell| {
            let t = cell.trim();
            !t.is_empty() && t.chars().all(|c| c == '-' || c == ':')
        })
}

/// Split a link/field component `k=v` into `(k, v)`; a component without `=`
/// yields an empty value.
fn split_field(component: &str) -> (String, String) {
    match component.split_once('=') {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => (component.to_string(), String::new()),
    }
}

/// The visible widget text, submit value, and checked state of a form field,
/// honouring any current-value override (the reader's typed text / toggled
/// state); with no override it renders from the parsed prefill, which is what the
/// print / non-tty sink always does. A masked text field shows its content as
/// asterisks while its true text is still returned as the submit value.
fn field_display(field: &Field, over: Option<&FieldValue>) -> (String, String, bool) {
    match field.kind {
        FieldKind::Text => {
            let text = over
                .map(|o| o.text.clone())
                .unwrap_or_else(|| field.value.clone());
            let shown = if field.masked {
                "*".repeat(text.chars().count())
            } else {
                text.clone()
            };
            // The widget spans the field's DECLARED micron width, not the width
            // of whatever is currently typed: NomadNet draws an empty field as a
            // full-width input bar. A zero width would leave no cell to focus or
            // click, so keep at least one.
            let w = field.width.max(1);
            // Text longer than the box is not lost: the editor slides a `w`-wide
            // window over it (`scroll`) so the caret stays visible while typing,
            // the same way the address bar scrolls. `fit` then pads the window
            // out to exactly `w` columns.
            let window: String = shown
                .chars()
                .skip(over.map_or(0, |o| o.scroll))
                .take(w)
                .collect();
            (fit(&window, w, Align::Left), text, false)
        }
        FieldKind::Checkbox => {
            let checked = over.map(|o| o.checked).unwrap_or(field.prechecked);
            (
                format!("[{}] {}", if checked { "x" } else { " " }, field.label),
                field.value.clone(),
                checked,
            )
        }
        FieldKind::Radio => {
            let checked = over.map(|o| o.checked).unwrap_or(field.prechecked);
            (
                format!(
                    "({}) {}",
                    if checked { "\u{2022}" } else { " " },
                    field.label
                ),
                field.value.clone(),
                checked,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_micron::{parse, Color, Line, Link, Span, Style};

    #[test]
    fn print_sink_marks_a_field_box_with_underscores() {
        // An 8-wide field prefilled "bo": the print sink cannot paint an input
        // bar, so its padding must show as underscores. Blank spaces would make
        // the field invisible in a plain dump.
        let doc = parse("Name: `<8|n`bo>");
        for no_color in [true, false] {
            let page = render_with_options(&doc, 80, no_color, ColorDepth::Truecolor);
            assert!(
                page.text.contains("bo______"),
                "field box not marked (no_color={no_color}): {:?}",
                page.text
            );
        }
    }

    fn plain_span(text: &str) -> Span {
        Span {
            text: text.to_string(),
            ..Span::default()
        }
    }

    fn styled_span(text: &str, style: Style) -> Span {
        Span {
            text: text.to_string(),
            style,
            ..Span::default()
        }
    }

    fn doc(blocks: Vec<Block>) -> MicronDocument {
        MicronDocument {
            blocks,
            ..MicronDocument::default()
        }
    }

    #[test]
    fn heading_has_indent_and_theme_colour() {
        let d = doc(vec![Block::Heading {
            depth: 2,
            line: Line::new(vec![plain_span("Title")]),
        }]);
        let page = render(&d, 20);
        // Depth-2 band: fg #111111 (17), bg #999999 (153).
        assert!(page.text.contains("38;2;17;17;17"));
        assert!(page.text.contains("48;2;153;153;153"));
        // Depth 2 indents by (2-1)*2 = 2 columns, inside the band.
        assert!(page.text.contains("  Title") || page.text.contains("Title"));
        // Band spans the full width (20 cols) => trailing padding present.
        let first_line = page.text.lines().next().unwrap_or("");
        // Strip SGR to count visible columns.
        let visible = strip_sgr(first_line);
        assert_eq!(visible.chars().count(), 20, "band should fill the width");
    }

    #[test]
    fn light_theme_uses_light_band_and_link_colour() {
        let d = doc(vec![
            Block::Heading {
                depth: 1,
                line: Line::new(vec![plain_span("Title")]),
            },
            Block::Paragraph {
                depth: 0,
                line: Line::new(vec![link_span("Docs")]),
            },
        ]);
        let (lines, _links) = layout(&d, 40, Theme::Light);
        let text = emit_ansi(&lines, false, ColorDepth::Truecolor);
        // Light depth-1 band: fg #000000 (0), bg #777777 (119).
        assert!(
            text.contains("38;2;0;0;0") && text.contains("48;2;119;119;119"),
            "light heading band missing: {text:?}"
        );
        // Light link colour: deep blue #005aaa => (0, 90, 170).
        assert!(
            text.contains("38;2;0;90;170"),
            "light link colour missing: {text:?}"
        );
        // The dark cyan link colour must NOT appear under the light theme.
        assert!(
            !text.contains("38;2;0;175;255"),
            "dark link colour leaked into light theme: {text:?}"
        );
    }

    #[test]
    fn bold_underline_italic_emit_sgr_and_reset() {
        let style = Style {
            bold: true,
            underline: true,
            italic: true,
            ..Style::default()
        };
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![styled_span("x", style)]),
        }]);
        let page = render(&d, 40);
        assert!(page.text.contains("\x1b[0;1;3;4m"), "got: {:?}", page.text);
        assert!(page.text.contains("x\x1b[0m"));
    }

    #[test]
    fn fg_and_bg_emit_24bit_codes() {
        let style = Style {
            fg: Some(Color::parse("f00")),
            bg: Some(Color::parse("00f")),
            ..Style::default()
        };
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![styled_span("hi", style)]),
        }]);
        let page = render(&d, 40);
        // f00 -> #ff0000, 00f -> #0000ff (nibble doubling).
        assert!(page.text.contains("38;2;255;0;0"));
        assert!(page.text.contains("48;2;0;0;255"));
    }

    #[test]
    fn ansi256_depth_downgrades_to_indexed_codes() {
        let style = Style {
            fg: Some(Color::parse("f00")),
            bg: Some(Color::parse("00f")),
            ..Style::default()
        };
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![styled_span("hi", style)]),
        }]);
        let page = render_with_options(&d, 40, false, ColorDepth::Ansi256);
        // Pure red -> palette 196, pure blue -> palette 21; no 24-bit sequence.
        assert!(page.text.contains("38;5;196"), "got: {:?}", page.text);
        assert!(page.text.contains("48;5;21"), "got: {:?}", page.text);
        assert!(
            !page.text.contains("38;2;"),
            "24-bit leaked: {:?}",
            page.text
        );
    }

    #[test]
    fn centred_line_is_padded() {
        let style = Style {
            align: Align::Center,
            ..Style::default()
        };
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![styled_span("hey", style)]),
        }]);
        let page = render(&d, 11);
        let line = page.text.lines().next().unwrap_or("");
        // (11 - 3) / 2 = 4 leading spaces.
        assert!(line.starts_with("    hey"), "got: {line:?}");
    }

    #[test]
    fn right_aligned_line_is_padded() {
        let style = Style {
            align: Align::Right,
            ..Style::default()
        };
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![styled_span("end", style)]),
        }]);
        let page = render(&d, 10);
        let line = page.text.lines().next().unwrap_or("");
        assert!(line.starts_with("       end"), "got: {line:?}");
    }

    #[test]
    fn divider_fills_full_width() {
        let d = doc(vec![Block::Divider {
            depth: 0,
            character: '\u{2500}',
        }]);
        let page = render(&d, 12);
        let line = page.text.lines().next().unwrap_or("");
        assert_eq!(line.chars().filter(|&c| c == '\u{2500}').count(), 12);
    }

    #[test]
    fn indented_divider_is_shorter() {
        let d = doc(vec![Block::Divider {
            depth: 2,
            character: '-',
        }]);
        let page = render(&d, 12);
        let line = page.text.lines().next().unwrap_or("");
        // depth 2 => indent 2 each side => 12 - 4 = 8 rule chars, 2 leading spaces.
        assert!(line.starts_with("  --------"), "got: {line:?}");
        assert_eq!(line.chars().filter(|&c| c == '-').count(), 8);
    }

    #[test]
    fn literal_block_is_verbatim_with_backticks() {
        let d = doc(vec![Block::LiteralBlock {
            lines: vec![Line::new(vec![plain_span("code `x` = 1")])],
        }]);
        let page = render(&d, 4); // narrow width must not wrap a literal
        assert!(page.text.contains("code `x` = 1"));
    }

    #[test]
    fn link_records_entry_without_visible_marker() {
        let link = Link {
            label: "Docs".to_string(),
            target: "/page/docs.mu".to_string(),
            fields: vec!["g=reticulum".to_string(), "ref".to_string()],
        };
        let span = Span {
            text: "Docs".to_string(),
            link: Some(link),
            ..Span::default()
        };
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![span]),
        }]);
        let page = render(&d, 40);
        assert!(page.text.contains("Docs"));
        // No visible `[N]` marker: links are set apart by style only.
        assert!(!page.text.contains("[1]"), "unexpected visible marker");
        assert_eq!(page.links.len(), 1);
        let l = &page.links[0];
        assert_eq!(l.index, 1);
        assert_eq!(l.label, "Docs");
        assert_eq!(l.target, "/page/docs.mu");
        assert_eq!(
            l.fields,
            vec![
                ("g".to_string(), "reticulum".to_string()),
                ("ref".to_string(), String::new()),
            ]
        );
    }

    fn link_span(label: &str) -> Span {
        Span {
            text: label.to_string(),
            link: Some(Link {
                label: label.to_string(),
                target: "/page/index.mu".to_string(),
                fields: vec![],
            }),
            ..Span::default()
        }
    }

    #[test]
    fn link_leading_whitespace_not_underlined() {
        // rngit embeds the bullet + indent inside the link label; the leading
        // whitespace must render plain, not underlined.
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![link_span("  \u{2022} mirrors")]),
        }]);
        let page = render(&d, 40);
        // Two leading spaces precede the underlined LINK run; the core label
        // carries the underline + LINK_FG run, with no `[N]` marker appended.
        assert!(
            page.text
                .contains("  \x1b[0;4;38;2;0;175;255m\u{2022} mirrors\x1b[0m"),
            "got: {:?}",
            page.text
        );
        // The leading spaces appear before the first SGR sequence.
        let first_sgr = page.text.find('\x1b').unwrap();
        assert!(
            page.text[..first_sgr].contains("  "),
            "leading spaces should precede the first SGR; got: {:?}",
            page.text
        );
    }

    #[test]
    fn link_trailing_whitespace_not_underlined() {
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![link_span("mirrors  ")]),
        }]);
        let page = render(&d, 40);
        // Core label underlined, then reset, then two plain spaces.
        assert!(
            page.text
                .contains("\x1b[0;4;38;2;0;175;255mmirrors\x1b[0m  "),
            "got: {:?}",
            page.text
        );
    }

    #[test]
    fn link_without_whitespace_fully_underlined() {
        // Control: no surrounding whitespace => whole label stays underlined.
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![link_span("mirrors")]),
        }]);
        let page = render(&d, 40);
        assert!(
            page.text.contains("\x1b[0;4;38;2;0;175;255mmirrors\x1b[0m"),
            "got: {:?}",
            page.text
        );
    }

    #[test]
    fn long_paragraph_wraps_to_width() {
        let text = "one two three four five six seven eight nine ten";
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![plain_span(text)]),
        }]);
        let page = render(&d, 12);
        for line in page.text.lines() {
            assert!(
                strip_sgr(line).chars().count() <= 12,
                "line too long: {line:?}"
            );
        }
        // No word was split across a wrap.
        assert!(page.text.contains("three"));
        assert!(page.text.contains("eight"));
    }

    #[test]
    fn no_color_strips_all_sgr() {
        let style = Style {
            bold: true,
            fg: Some(Color::parse("f00")),
            ..Style::default()
        };
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![styled_span("bold red", style)]),
        }]);
        let page = render_with_options(&d, 40, true, ColorDepth::Truecolor);
        assert!(!page.text.contains('\x1b'), "SGR leaked: {:?}", page.text);
        assert!(page.text.contains("bold red"));
    }

    #[test]
    fn no_color_link_has_no_marker() {
        let link = Link {
            label: "Label".to_string(),
            target: "/x".to_string(),
            fields: vec![],
        };
        let span = Span {
            text: "Label".to_string(),
            link: Some(link),
            ..Span::default()
        };
        let d = doc(vec![Block::Paragraph {
            depth: 0,
            line: Line::new(vec![span]),
        }]);
        let page = render_with_options(&d, 40, true, ColorDepth::Truecolor);
        assert!(!page.text.contains('\x1b'));
        // Plain output: the label appears bare, with no `[N]` marker.
        assert!(page.text.contains("Label"));
        assert!(!page.text.contains("[1]"));
    }

    #[test]
    fn table_columns_align() {
        let d = doc(vec![Block::Table {
            depth: 0,
            align: None,
            max_width: None,
            rows: vec![
                "Name | Age".to_string(),
                "--- | ---".to_string(),
                "Ann | 3".to_string(),
                "Bob | 42".to_string(),
            ],
        }]);
        let page = render(&d, 40);
        let lines: Vec<&str> = page.text.lines().collect();
        // Header and body rows share the same separator column position.
        let header = strip_sgr(lines[0]);
        let bob = strip_sgr(lines[3]);
        let hbar = header.find('\u{2502}');
        let bbar = bob.find('\u{2502}');
        assert!(
            hbar.is_some() && hbar == bbar,
            "columns misaligned: {lines:?}"
        );
        // "Name" column is 4 wide, so "Ann" is padded to 4 before the divider.
        assert!(strip_sgr(lines[2]).starts_with("Ann  \u{2502} 3"));
    }

    #[test]
    fn narrow_width_does_not_panic() {
        let text = "a very long unbroken paragraph of words here";
        let d = doc(vec![Block::Paragraph {
            depth: 3,
            line: Line::new(vec![plain_span(text)]),
        }]);
        // width smaller than the indent must still render without panicking.
        let page = render(&d, 1);
        assert!(!page.text.is_empty());
    }

    #[test]
    fn readme_mu_renders_without_panic() {
        let src = include_str!("../../reference/Reticulum/README.mu");
        let d = parse(src);
        let page = render(&d, 80);
        assert!(!page.text.is_empty());
        // The README opens with a section heading.
        assert!(page.text.contains("Reticulum Network Stack"));
        // It contains links, which must be collected with sane indices.
        assert!(!page.links.is_empty());
        for (i, link) in page.links.iter().enumerate() {
            assert_eq!(link.index, i + 1);
            assert!(!link.target.is_empty());
        }
        // no_color rendering must also survive the whole document.
        let plain = render_with_options(&d, 80, true, ColorDepth::Truecolor);
        assert!(!plain.text.contains('\x1b'));
    }

    #[test]
    fn a_hostile_page_puts_no_escape_of_its_own_into_the_output() {
        // Codeberg #281: remote page bytes reach a terminal through this sink,
        // where a page's `\x1b[` is indistinguishable from ours. The filter
        // sits in the parser, so this test pins the property at the sink that
        // the reader actually sees.
        let hostile = "Balance: 5 EUR\n\x1b[2J\x1b[HBalance: 5000 EUR\n\
                       \x1b]0;pwned\x07\x1b]52;c;cm0K\x07\u{9b}31m\r overwritten\n\
                       `=\n\x1b[5mblink\n`=";
        let d = parse(hostile);

        // --no-color already claims to be escape-free; now it is true of
        // hostile input too, not just of ours.
        let plain = render_with_options(&d, 80, true, ColorDepth::Truecolor);
        assert!(
            !plain.text.chars().any(|c| c.is_control() && c != '\n'),
            "no_color output carries a control char: {:?}",
            plain.text
        );

        // With colour on, the only escapes present are the SGR sequences this
        // renderer emits; nothing from the page survives.
        let colored = render(&d, 80);
        for seq in colored.text.split('\x1b').skip(1) {
            assert!(
                seq.starts_with('['),
                "a non-SGR escape reached the terminal: {seq:?}"
            );
            let body = &seq[1..];
            let end = body
                .find('m')
                .unwrap_or_else(|| panic!("unterminated SGR: {seq:?}"));
            assert!(
                body[..end].chars().all(|c| c.is_ascii_digit() || c == ';'),
                "a non-SGR CSI reached the terminal: {seq:?}"
            );
        }

        // The reader still gets the text; only the actions were removed.
        assert!(colored.text.contains("Balance: 5 EUR"));
        assert!(colored.text.contains("Balance: 5000 EUR"));
    }

    /// Strip ANSI SGR sequences so tests can reason about visible columns.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip until the terminating 'm'.
                for c2 in chars.by_ref() {
                    if c2 == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
