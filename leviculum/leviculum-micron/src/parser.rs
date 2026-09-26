//! The stateful micron parser.
//!
//! An implementation of the micron markup format: [`parse`] handles line-level
//! constructs and inline formatting. Formatting and colour state persists
//! across lines until reset. The parser is lenient: unterminated sequences
//! degrade gracefully and no input can panic.

use crate::color::Color;
use crate::model::{Align, Block, Field, FieldKind, Line, Link, MicronDocument, Span, Style};
use std::collections::BTreeMap;

/// The persistent parser state, mirroring the reference `default_state`.
///
/// Foreground/background default to `None` (terminal default) rather than a
/// concrete theme colour, keeping the model render-agnostic.
#[derive(Clone, Debug)]
struct State {
    literal: bool,
    depth: u8,
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    underline: bool,
    italic: bool,
    default_align: Align,
    align: Align,
    /// Anchor names declared but not yet bound to a block, mirroring the
    /// canonical `pending_anchors`. Drained onto the next block that a line
    /// produces (`markup_to_attrmaps` binding).
    pending_anchors: Vec<String>,
    /// Whether the current line is a heading whose block index should be
    /// recorded in `header_rows` (canonical `_header_pending`).
    header_pending: bool,
    /// Whether lines are being buffered into a table (canonical `table_mode`).
    table_mode: bool,
    /// Raw rows buffered since the opening `` `t `` toggle.
    table_buffer: Vec<String>,
    /// Forced column alignment captured at the opening `` `t `` toggle.
    table_align: Option<Align>,
    /// Max table width captured at the opening `` `t `` toggle.
    table_max_width: Option<usize>,
}

impl Default for State {
    fn default() -> State {
        State {
            literal: false,
            depth: 0,
            fg: None,
            bg: None,
            bold: false,
            underline: false,
            italic: false,
            default_align: Align::Left,
            align: Align::Left,
            pending_anchors: Vec::new(),
            header_pending: false,
            table_mode: false,
            table_buffer: Vec::new(),
            table_align: None,
            table_max_width: None,
        }
    }
}

impl State {
    /// The current [`Style`], captured at span-emit time (like `make_style`).
    fn style(&self) -> Style {
        Style {
            fg: self.fg.clone(),
            bg: self.bg.clone(),
            bold: self.bold,
            underline: self.underline,
            italic: self.italic,
            align: self.align,
        }
    }
}

/// Strip terminal control characters from untrusted page bytes.
///
/// A micron page is content fetched from an arbitrary node in the mesh, and a
/// renderer writes it to a terminal that emits real escape sequences of its
/// own for colour — so a remote `\x1b[` is indistinguishable from ours once it
/// is in the output stream. Filtering here, where untrusted bytes first become
/// document text, covers every consumer of [`parse`] rather than one renderer:
/// spans, literal blocks, table rows, link labels and targets, field values
/// and anchor names all come out of this same input (Codeberg #281).
///
/// What survives: everything printable, plus `\n`, which is the format's own
/// line structure. Dropped: all other C0 controls including `ESC` (CSI/OSC/DCS
/// introducers), `CR` (rewrites the current line), `BEL`, `SO`/`SI` (charset
/// switching), `DEL`, and the C1 range `U+0080..=U+009F`, whose members are
/// single-character forms of the same introducers.
///
/// `\t` becomes a single space rather than being dropped: it is whitespace the
/// content may legitimately carry, but a tab occupies one cell in the model
/// and eight columns on the terminal, which desynchronises every width the
/// renderer computed.
fn strip_control_chars(input: &str) -> std::borrow::Cow<'_, str> {
    let unsafe_char = |c: char| c != '\n' && (c.is_control() || ('\u{80}'..='\u{9f}').contains(&c));
    if !input.chars().any(unsafe_char) {
        return std::borrow::Cow::Borrowed(input);
    }
    std::borrow::Cow::Owned(
        input
            .chars()
            .filter_map(|c| match c {
                '\t' => Some(' '),
                c if unsafe_char(c) => None,
                c => Some(c),
            })
            .collect(),
    )
}

/// Parse a micron (`.mu`) document into a [`MicronDocument`].
///
/// Stateful and line-oriented: the document is split on `\n` and each line is
/// processed in turn, with formatting/colour state carried across lines. Never
/// panics on malformed input.
///
/// Terminal control characters are stripped from the input first: no byte a
/// remote page supplies can reach a terminal as an escape sequence. Everything
/// printable survives, plus `\n`; `\t` becomes a space. See the private
/// `strip_control_chars` for the full rule (Codeberg #281).
pub fn parse(input: &str) -> MicronDocument {
    let input = strip_control_chars(input);
    let input: &str = &input;
    let mut state = State::default();
    let mut blocks: Vec<Block> = Vec::new();
    let mut anchors: BTreeMap<String, usize> = BTreeMap::new();
    let mut header_rows: Vec<usize> = Vec::new();
    // Consecutive literal lines are grouped into a single LiteralBlock.
    let mut literal_lines: Vec<Line> = Vec::new();

    for raw_line in input.split('\n') {
        // The `= toggle is checked first and works in and out of literal mode,
        // mirroring the reference (parse_line lines 93-96). It emits nothing.
        if raw_line == "`=" {
            let was_literal = state.literal;
            state.literal = !state.literal;
            if was_literal && !state.literal && !literal_lines.is_empty() {
                bind_and_push(
                    Block::LiteralBlock {
                        lines: std::mem::take(&mut literal_lines),
                    },
                    &mut blocks,
                    &mut anchors,
                    &mut header_rows,
                    &mut state,
                );
            }
            continue;
        }

        if state.literal {
            literal_lines.push(make_literal_line(&state, raw_line));
            continue;
        }

        // Empty lines never reach parse_line in the reference; they are blanks.
        if raw_line.is_empty() {
            bind_and_push(
                Block::Blank,
                &mut blocks,
                &mut anchors,
                &mut header_rows,
                &mut state,
            );
            continue;
        }

        if let Some(block) = parse_line(raw_line, &mut state) {
            bind_and_push(
                block,
                &mut blocks,
                &mut anchors,
                &mut header_rows,
                &mut state,
            );
        }
    }

    // An unterminated literal block is flushed at end of input (lenient).
    if !literal_lines.is_empty() {
        bind_and_push(
            Block::LiteralBlock {
                lines: literal_lines,
            },
            &mut blocks,
            &mut anchors,
            &mut header_rows,
            &mut state,
        );
    }

    // An unterminated table is flushed at end of input (lenient); the canonical
    // parser drops it, but we surface the buffered rows rather than lose them.
    if state.table_mode && !state.table_buffer.is_empty() {
        let block = Block::Table {
            depth: state.depth,
            align: state.table_align.take(),
            max_width: state.table_max_width.take(),
            rows: std::mem::take(&mut state.table_buffer),
        };
        bind_and_push(
            block,
            &mut blocks,
            &mut anchors,
            &mut header_rows,
            &mut state,
        );
    }

    MicronDocument {
        blocks,
        anchors,
        header_rows,
    }
}

/// Push `block`, binding any [`State::pending_anchors`] to its index and
/// recording the index in `header_rows` when a heading is pending. Mirrors the
/// canonical `markup_to_attrmaps` binding: an anchor maps to the index of the
/// block produced by the line that declared it; the first binding for a name
/// wins.
fn bind_and_push(
    block: Block,
    blocks: &mut Vec<Block>,
    anchors: &mut BTreeMap<String, usize>,
    header_rows: &mut Vec<usize>,
    state: &mut State,
) {
    let idx = blocks.len();
    for name in state.pending_anchors.drain(..) {
        anchors.entry(name).or_insert(idx);
    }
    if state.header_pending {
        header_rows.push(idx);
        state.header_pending = false;
    }
    blocks.push(block);
}

/// Build a single verbatim literal line (mirrors `make_output` literal branch,
/// lines 446-449): the escaped toggle `` \`= `` is unescaped to `` `= ``.
fn make_literal_line(state: &State, raw_line: &str) -> Line {
    let text = if raw_line == "\\`=" {
        "`=".to_string()
    } else {
        raw_line.to_string()
    };
    Line::new(vec![Span {
        text,
        style: state.style(),
        ..Span::default()
    }])
}

/// Handle one non-empty, non-literal source line (mirrors `parse_line`).
fn parse_line(raw: &str, state: &mut State) -> Option<Block> {
    // Heading-with-field sanitization (canonical parse_line lines 233-236): a
    // line that starts a heading (`>`) but also contains a `` `< `` form field
    // is demoted to a normal line by stripping its leading `>` markers.
    let line: &str = if raw.starts_with('>') && raw.contains("`<") {
        raw.trim_start_matches('>')
    } else {
        raw
    };

    let first = line.chars().next()?;

    // Escape: strip the leading `\` and emit the rest literally-armed.
    if first == '\\' {
        let rest = &line[1..];
        let spans = make_output(state, rest, true);
        if spans.is_empty() {
            return None;
        }
        return Some(Block::Paragraph {
            depth: state.depth,
            line: Line::new(spans),
        });
    }

    // Comment: dropped entirely.
    if first == '#' {
        return None;
    }

    // Table toggle (canonical parse_line lines 247-270). Checked before the
    // block constructs so that, while buffering, rows with any leading
    // character are captured verbatim.
    if let Some(rest) = line.strip_prefix("`t") {
        return handle_table_toggle(rest, state);
    }
    if state.table_mode {
        state.table_buffer.push(line.to_string());
        return None;
    }

    // Partial (canonical parse_line lines 277-279).
    if let Some(rest) = line.strip_prefix("`{") {
        return parse_partial(rest, state.depth);
    }

    // Section-heading depth reset: reset depth then re-parse the remainder.
    if first == '<' {
        state.depth = 0;
        let rest = &line[1..];
        if rest.is_empty() {
            return None;
        }
        return parse_line(rest, state);
    }

    // Section heading: depth = count of leading `>`.
    if first == '>' {
        let depth = line.bytes().take_while(|&b| b == b'>').count();
        state.depth = depth as u8;
        let rest = &line[depth..];
        if rest.is_empty() {
            return None;
        }
        // The reference temporarily swaps in the theme heading style, renders,
        // then restores the pre-heading colour/format state. We drop the theme
        // (heading styling is applied by depth at render time) but preserve the
        // restore so inline commands inside a heading do not leak onto later
        // lines.
        let saved = (
            state.fg.clone(),
            state.bg.clone(),
            state.bold,
            state.underline,
            state.italic,
        );
        let spans = make_output(state, rest, false);
        state.fg = saved.0;
        state.bg = saved.1;
        state.bold = saved.2;
        state.underline = saved.3;
        state.italic = saved.4;

        // A heading contributes an auto-generated slug anchor and marks its row
        // as a header (canonical parse_line lines 308-311). Set before the empty
        // check so an anchor/header on a text-less heading still carries to the
        // next produced block, exactly as the reference does.
        let slug = slugify_micron(rest);
        if !slug.is_empty() {
            state.pending_anchors.push(slug);
        }
        state.header_pending = true;

        if spans.is_empty() {
            return None;
        }
        return Some(Block::Heading {
            depth: state.depth,
            line: Line::new(spans),
        });
    }

    // Horizontal divider: any line starting with `-`. A two-character line
    // `-X` uses X as the divider glyph (control chars fall back to `\u{2500}`);
    // otherwise the default glyph is used. (Reference parse_line lines 148-159;
    // note it keys on the first char, not an exact `-` line.)
    if first == '-' {
        let chars: Vec<char> = line.chars().collect();
        let character = if chars.len() == 2 {
            let c = chars[1];
            if (c as u32) < 32 {
                '\u{2500}'
            } else {
                c
            }
        } else {
            '\u{2500}'
        };
        return Some(Block::Divider {
            depth: state.depth,
            character,
        });
    }

    // Normal text line.
    let spans = make_output(state, line, false);
    if spans.is_empty() {
        return None;
    }
    Some(Block::Paragraph {
        depth: state.depth,
        line: Line::new(spans),
    })
}

/// Parsing sub-mode inside `make_output`.
enum Mode {
    Text,
    Formatting,
}

/// Emit a styled text span with the current state.
fn make_part(state: &State, part: &str) -> Span {
    Span {
        text: part.to_string(),
        style: state.style(),
        ..Span::default()
    }
}

/// Find the position of `needle` in `chars` at or after `from`.
fn find_char(chars: &[char], needle: char, from: usize) -> Option<usize> {
    chars
        .iter()
        .skip(from)
        .position(|&c| c == needle)
        .map(|p| p + from)
}

/// Whether `c` is a valid anchor-name character.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Parse the inline content of a line into spans (mirrors `make_output`).
///
/// `pre_escape` arms the escape state for the first character (used by the
/// line-level `\` escape). Formatting commands mutate `state` so their effect
/// persists across lines.
fn make_output(state: &mut State, line: &str, pre_escape: bool) -> Vec<Span> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut output: Vec<Span> = Vec::new();
    let mut part = String::new();
    let mut mode = Mode::Text;
    let mut escape = pre_escape;
    let mut skip: usize = 0;

    for i in 0..n {
        let c = chars[i];
        if skip > 0 {
            skip -= 1;
        } else {
            match mode {
                Mode::Formatting => {
                    match c {
                        '_' => state.underline = !state.underline,
                        '!' => state.bold = !state.bold,
                        '*' => state.italic = !state.italic,
                        // Foreground colour: `Fxxx` (12-bit) or `FT<rrggbb>`
                        // (24-bit true colour). Canonical make_output 617-626.
                        'F' if n >= i + 4 => {
                            let (color, adv) = read_color(&chars, i, n);
                            state.fg = Some(Color::parse(&color));
                            skip = adv;
                        }
                        'f' => state.fg = None,
                        // Background colour: `Bxxx` or `BT<rrggbb>`. 629-638.
                        'B' if n >= i + 4 => {
                            let (color, adv) = read_color(&chars, i, n);
                            state.bg = Some(Color::parse(&color));
                            skip = adv;
                        }
                        'b' => state.bg = None,
                        '`' => {
                            state.bold = false;
                            state.underline = false;
                            state.italic = false;
                            state.fg = None;
                            state.bg = None;
                            state.align = state.default_align;
                        }
                        // Alignment commands set the alignment; unlike older
                        // NomadNet, the canonical parser (make_output 648-653)
                        // no longer toggles back to the default when the command
                        // repeats the current alignment. `` `a `` and the `` ` ``
                        // reset return to the default.
                        'c' => state.align = Align::Center,
                        'l' => state.align = Align::Left,
                        'r' => state.align = Align::Right,
                        'a' => state.align = state.default_align,
                        ':' => {
                            // Anchor declaration `:name` (name runs until a
                            // non-name char). Canonical make_output 657-667
                            // registers the name in `pending_anchors`, which
                            // `markup_to_attrmaps` binds to this line's block.
                            // We mirror that binding (see `bind_and_push`) and,
                            // as a render-agnostic superset, also emit a
                            // zero-width span carrying the anchor so a renderer
                            // can mark the exact in-line position.
                            let mut j = i + 1;
                            while j < n && is_name_char(chars[j]) {
                                j += 1;
                            }
                            let name: String = chars[i + 1..j].iter().collect();
                            skip = j - (i + 1);
                            if !name.is_empty() {
                                state.pending_anchors.push(name.clone());
                            }
                            output.push(Span {
                                anchor: Some(name),
                                style: state.style(),
                                ..Span::default()
                            });
                        }
                        '<' => {
                            if let Some((span, consumed)) = parse_field(state, &chars, i) {
                                output.push(span);
                                skip = consumed;
                            }
                        }
                        '[' => {
                            if let Some((span, consumed)) = parse_link(state, &chars, i) {
                                if let Some(span) = span {
                                    output.push(span);
                                }
                                skip = consumed;
                            }
                        }
                        _ => {} // Unknown command: ignored, like the reference.
                    }

                    mode = Mode::Text;
                    if !part.is_empty() {
                        output.push(make_part(state, &part));
                        part.clear();
                    }
                }
                Mode::Text => {
                    if c == '\\' {
                        if escape {
                            part.push('\\');
                            escape = false;
                        } else {
                            escape = true;
                        }
                    } else if c == '`' {
                        if escape {
                            part.push('`');
                            escape = false;
                        } else {
                            mode = Mode::Formatting;
                            if !part.is_empty() {
                                output.push(make_part(state, &part));
                                part.clear();
                            }
                        }
                    } else {
                        part.push(c);
                        escape = false;
                    }
                }
            }
        }

        // End-of-line flush runs every iteration's last index (even under skip).
        if i == n - 1 && !part.is_empty() {
            output.push(make_part(state, &part));
            part.clear();
        }
    }

    output
}

/// Parse a `` `<flags|name`data> `` form field starting at the `<` at index
/// `open`. Returns the span and the number of following characters to skip, or
/// `None` when the field is malformed (missing `` ` `` or `>`). Mirrors the
/// reference field parse (`make_output` lines 507-598).
fn parse_field(state: &State, chars: &[char], open: usize) -> Option<(Span, usize)> {
    let field_start = open + 1;
    let backtick_pos = find_char(chars, '`', field_start)?;
    let field_end = find_char(chars, '>', backtick_pos)?;

    let field_content: String = chars[field_start..backtick_pos].iter().collect();
    let field_data: String = chars[backtick_pos + 1..field_end].iter().collect();

    let mut masked = false;
    let mut width: usize = 24;
    let mut kind = FieldKind::Text;
    let mut name = field_content.clone();
    let mut value = String::new();
    let mut prechecked = false;

    if field_content.contains('|') {
        let components: Vec<&str> = field_content.split('|').collect();
        let mut flags = components[0].to_string();
        // components[0] is flags; components[1] is the name (may be absent).
        name = components.get(1).map(|s| s.to_string()).unwrap_or_default();

        if flags.contains('^') {
            kind = FieldKind::Radio;
            flags = flags.replace('^', "");
        } else if flags.contains('?') {
            kind = FieldKind::Checkbox;
            flags = flags.replace('?', "");
        } else if flags.contains('!') {
            masked = true;
            flags = flags.replace('!', "");
        }

        if !flags.is_empty() {
            if let Ok(w) = flags.parse::<i64>() {
                width = w.clamp(0, 256) as usize;
            }
        }

        if components.len() > 2 {
            value = components[2].to_string();
        }
        if components.len() > 3 && components[3] == "*" {
            prechecked = true;
        }
    }

    let consumed = field_end - open;
    let field = match kind {
        FieldKind::Text => Field {
            name,
            width,
            masked,
            kind,
            value: field_data,
            label: String::new(),
            prechecked: false,
        },
        FieldKind::Checkbox | FieldKind::Radio => {
            let resolved_value = if value.is_empty() {
                field_data.clone()
            } else {
                value
            };
            Field {
                name,
                width,
                masked,
                kind,
                value: resolved_value,
                label: field_data,
                prechecked,
            }
        }
    };

    Some((
        Span {
            field: Some(field),
            style: state.style(),
            ..Span::default()
        },
        consumed,
    ))
}

/// Parse a `` `[Label`target`fields] `` link starting at the `[` at index
/// `open`. Returns `(Option<Span>, skip)`: the span is `None` when the link has
/// no target (reference emits nothing) but the bracket span is still consumed.
/// Returns `None` only when there is no closing `]`. Mirrors `make_output`
/// lines 601-660.
fn parse_link(state: &State, chars: &[char], open: usize) -> Option<(Option<Span>, usize)> {
    let close = find_char(chars, ']', open)?;
    let endpos = close - open; // offset of `]` from `[`, matches reference skip.
    let link_data: String = chars[open + 1..close].iter().collect();

    let components: Vec<&str> = link_data.split('`').collect();
    let (label, url, fields_raw) = match components.len() {
        1 => (String::new(), components[0].to_string(), String::new()),
        2 => (
            components[0].to_string(),
            components[1].to_string(),
            String::new(),
        ),
        3 => (
            components[0].to_string(),
            components[1].to_string(),
            components[2].to_string(),
        ),
        _ => (String::new(), String::new(), String::new()),
    };

    if url.is_empty() {
        // No target: reference emits no output but still consumes the bracket.
        return Some((None, endpos));
    }

    let label = if label.is_empty() { url.clone() } else { label };
    let fields = if fields_raw.is_empty() {
        Vec::new()
    } else {
        fields_raw.split('|').map(|s| s.to_string()).collect()
    };

    Some((
        Some(Span {
            text: label.clone(),
            link: Some(Link {
                label,
                target: url,
                fields,
            }),
            style: state.style(),
            ..Span::default()
        }),
        endpos,
    ))
}

/// Read a colour argument following an `F`/`B` command char at index `i`.
///
/// Returns the raw colour string and the number of following characters to
/// skip. The caller guarantees `n >= i + 4`. The `T<rrggbb>` true-colour form
/// (six hex chars) takes precedence when a `T` follows and enough characters
/// remain; otherwise the legacy three-character form is used. Mirrors the
/// canonical make_output branches (617-626 / 629-638).
fn read_color(chars: &[char], i: usize, n: usize) -> (String, usize) {
    if chars[i + 1] == 'T' && n >= i + 8 {
        (chars[i + 2..i + 8].iter().collect(), 7)
    } else {
        (chars[i + 1..i + 4].iter().collect(), 3)
    }
}

/// Handle a `` `t `` table toggle. On the opening toggle the optional alignment
/// (`l`/`c`/`r`) and max width are captured and buffering begins; on the
/// closing toggle the buffered rows become a [`Block::Table`]. Mirrors the
/// canonical parse_line table handling (248-270); fewer than two buffered rows
/// yields nothing, as `render_table` requires a header and separator row.
fn handle_table_toggle(rest: &str, state: &mut State) -> Option<Block> {
    if state.table_mode {
        state.table_mode = false;
        let rows = std::mem::take(&mut state.table_buffer);
        let align = state.table_align.take();
        let max_width = state.table_max_width.take();
        if rows.len() < 2 {
            return None;
        }
        return Some(Block::Table {
            depth: state.depth,
            align,
            max_width,
            rows,
        });
    }

    let align = match rest.chars().next() {
        Some('l') => Some(Align::Left),
        Some('c') => Some(Align::Center),
        Some('r') => Some(Align::Right),
        _ => None,
    };
    let width_str = if align.is_some() { &rest[1..] } else { rest };
    let max_width = if width_str.is_empty() {
        None
    } else {
        width_str.parse::<usize>().ok()
    };

    state.table_mode = true;
    state.table_buffer = Vec::new();
    state.table_align = align;
    state.table_max_width = max_width;
    None
}

/// Parse a `` `{url`refresh`fields} `` partial into a [`Block::Partial`].
///
/// `rest` is the text after the opening `` `{ ``; the descriptor ends at the
/// first `}`. Mirrors the canonical `parse_partial` (149-195); the fetch,
/// content hash and refresh scheduling are deferred to a later phase.
fn parse_partial(rest: &str, depth: u8) -> Option<Block> {
    let end = rest.find('}')?;
    let data = &rest[..end];
    let components: Vec<&str> = data.split('`').collect();
    let (url, refresh, fields_raw): (&str, Option<f64>, &str) = match components.len() {
        1 => (components[0], None, ""),
        2 => (components[0], parse_refresh(components[1]), ""),
        3 => (components[0], parse_refresh(components[1]), components[2]),
        _ => ("", None, ""),
    };
    if url.is_empty() {
        return None;
    }
    let fields = if fields_raw.is_empty() {
        Vec::new()
    } else {
        fields_raw.split('|').map(|s| s.to_string()).collect()
    };
    Some(Block::Partial {
        depth,
        url: url.to_string(),
        refresh,
        fields,
    })
}

/// Parse a partial refresh interval in seconds. Values below 1 normalise to
/// `None`, matching the canonical `if partial_refresh < 1: partial_refresh = None`.
fn parse_refresh(s: &str) -> Option<f64> {
    match s.parse::<f64>() {
        Ok(v) if v >= 1.0 => Some(v),
        _ => None,
    }
}

/// Slugify heading text into an anchor name (canonical `slugify_micron`,
/// 79-88): strip micron control sequences, replace each run of non-alphanumeric
/// ASCII with a single `-`, trim leading/trailing `-`, and lowercase.
fn slugify_micron(text: &str) -> String {
    let stripped = strip_micron(text);
    let mut out = String::new();
    let mut prev_dash = false;
    for c in stripped.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Remove micron control sequences from `text`, mirroring the canonical
/// `_MICRON_STRIP_RE` alternation (70-77): true-colour `` `[FB]T<6hex> ``,
/// 12-bit `` `[FB]<3hex> ``, anchor `` `:name ``, and single-character commands.
fn strip_micron(text: &str) -> String {
    const CMDS: &str = "!*_=fbacrl`<>{";
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    let mut out = String::new();
    let mut i = 0;
    while i < n {
        if cs[i] == '`' && i + 1 < n {
            let c1 = cs[i + 1];
            // `[FB]T<6hex>
            if (c1 == 'F' || c1 == 'B')
                && i + 8 < n
                && cs[i + 2] == 'T'
                && cs[i + 3..i + 9].iter().all(|c| c.is_ascii_hexdigit())
            {
                i += 9;
                continue;
            }
            // `[FB]<3hex>
            if (c1 == 'F' || c1 == 'B')
                && i + 4 < n
                && cs[i + 2..i + 5].iter().all(|c| c.is_ascii_hexdigit())
            {
                i += 5;
                continue;
            }
            // `:name
            if c1 == ':' {
                let mut j = i + 2;
                while j < n && is_name_char(cs[j]) {
                    j += 1;
                }
                i = j;
                continue;
            }
            // `<single command char>
            if CMDS.contains(c1) {
                i += 2;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}
