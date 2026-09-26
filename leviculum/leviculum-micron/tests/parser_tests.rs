//! Construct-by-construct unit tests for the micron parser.
//!
//! Each test asserts the resulting document model, not a rendering. Snippets
//! are a mix of hand-crafted lines and lines drawn from `README.mu`.

use leviculum_micron::{parse, Align, Block, Color, FieldKind, Line, Span};

/// Return the spans of the single paragraph produced by `input`.
fn para_spans(input: &str) -> Vec<Span> {
    let doc = parse(input);
    assert_eq!(
        doc.blocks.len(),
        1,
        "expected exactly one block for {input:?}"
    );
    match &doc.blocks[0] {
        Block::Paragraph { line, .. } => line.spans.clone(),
        other => panic!("expected a paragraph, got {other:?}"),
    }
}

// --- Plain text ----------------------------------------------------------

#[test]
fn plain_text_is_one_span() {
    let spans = para_spans("just some plain text");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "just some plain text");
    assert_eq!(spans[0].style, Default::default());
    assert!(spans[0].link.is_none());
    assert!(spans[0].field.is_none());
    assert!(spans[0].anchor.is_none());
}

// --- Headings + depth reset ---------------------------------------------

#[test]
fn headings_depth_1_2_3() {
    for (src, depth) in [(">H1", 1u8), (">>H2", 2), (">>>H3", 3)] {
        let doc = parse(src);
        match &doc.blocks[0] {
            Block::Heading { depth: d, line } => {
                assert_eq!(*d, depth, "depth for {src:?}");
                assert_eq!(line.spans[0].text, src.trim_start_matches('>'));
            }
            other => panic!("expected heading for {src:?}, got {other:?}"),
        }
    }
}

#[test]
fn depth_persists_and_resets() {
    let doc = parse(">>>Deep\ninside\n<\noutside");
    // Heading depth 3, paragraph inheriting depth 3, then reset to 0.
    assert!(matches!(doc.blocks[0], Block::Heading { depth: 3, .. }));
    assert!(matches!(doc.blocks[1], Block::Paragraph { depth: 3, .. }));
    // The lone `<` emits no block; the next line is depth 0.
    assert!(matches!(doc.blocks[2], Block::Paragraph { depth: 0, .. }));
    assert_eq!(doc.blocks.len(), 3);
}

#[test]
fn bare_heading_marker_sets_depth_but_emits_nothing() {
    let doc = parse(">>\ntext");
    // `>>` alone sets depth 2 and produces no block; the text inherits it.
    assert_eq!(doc.blocks.len(), 1);
    assert!(matches!(doc.blocks[0], Block::Paragraph { depth: 2, .. }));
}

// --- Literal block -------------------------------------------------------

#[test]
fn literal_block_passes_backticks_through() {
    let doc = parse("`=\ntext with `backticks` and `!markup`!\n`=");
    assert_eq!(doc.blocks.len(), 1);
    match &doc.blocks[0] {
        Block::LiteralBlock { lines } => {
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].spans.len(), 1);
            assert_eq!(
                lines[0].spans[0].text,
                "text with `backticks` and `!markup`!"
            );
        }
        other => panic!("expected literal block, got {other:?}"),
    }
}

#[test]
fn literal_block_unescapes_toggle() {
    let doc = parse("`=\n\\`=\n`=");
    match &doc.blocks[0] {
        Block::LiteralBlock { lines } => assert_eq!(lines[0].spans[0].text, "`="),
        other => panic!("expected literal block, got {other:?}"),
    }
}

#[test]
fn unterminated_literal_block_is_flushed() {
    // No closing `= — still yields a literal block (lenient).
    let doc = parse("`=\nhanging literal");
    assert!(matches!(
        doc.blocks.last(),
        Some(Block::LiteralBlock { .. })
    ));
}

// --- Comment -------------------------------------------------------------

#[test]
fn comment_is_dropped() {
    let doc = parse("before\n# a comment line\nafter");
    assert_eq!(doc.blocks.len(), 2);
    assert!(matches!(doc.blocks[0], Block::Paragraph { .. }));
    assert!(matches!(doc.blocks[1], Block::Paragraph { .. }));
}

// --- Divider -------------------------------------------------------------

#[test]
fn divider_default_and_custom() {
    match parse("-").blocks[0] {
        Block::Divider { character, .. } => assert_eq!(character, '\u{2500}'),
        ref other => panic!("expected divider, got {other:?}"),
    }
    match parse("-=").blocks[0] {
        Block::Divider { character, .. } => assert_eq!(character, '='),
        ref other => panic!("expected divider, got {other:?}"),
    }
    // A longer run of dashes is still a divider with the default glyph.
    match parse("-----").blocks[0] {
        Block::Divider { character, .. } => assert_eq!(character, '\u{2500}'),
        ref other => panic!("expected divider, got {other:?}"),
    }
}

// --- Text attribute toggles ---------------------------------------------

#[test]
fn bold_underline_italic_toggles() {
    let spans = para_spans("`!bold`! `_under`_ `*italic`*");
    // "bold" bold, then a plain " ", then "under" underlined, plain " ",
    // then "italic" italic.
    assert_eq!(spans[0].text, "bold");
    assert!(spans[0].style.bold);
    assert!(spans.iter().any(|s| s.text == "under" && s.style.underline));
    assert!(spans.iter().any(|s| s.text == "italic" && s.style.italic));
}

#[test]
fn nested_toggles_combine() {
    let spans = para_spans("`!`_both`_`!");
    assert_eq!(spans[0].text, "both");
    assert!(spans[0].style.bold);
    assert!(spans[0].style.underline);
    assert!(!spans[0].style.italic);
}

#[test]
fn backtick_resets_all_formatting() {
    // bold+italic+fg, then `` resets, then plain text.
    let spans = para_spans("`!`*`Ff00red``plain");
    let red = spans.iter().find(|s| s.text == "red").expect("red span");
    assert!(red.style.bold && red.style.italic);
    assert_eq!(red.style.fg, Some(Color::parse("f00")));
    let plain = spans
        .iter()
        .find(|s| s.text == "plain")
        .expect("plain span");
    assert!(!plain.style.bold && !plain.style.italic);
    assert_eq!(plain.style.fg, None);
}

// --- Colour --------------------------------------------------------------

#[test]
fn foreground_colour_set_and_reset() {
    let spans = para_spans("`Ff00red`fdefault");
    let red = spans.iter().find(|s| s.text == "red").expect("red span");
    assert_eq!(red.style.fg, Some(Color::parse("f00")));
    assert_eq!(red.style.fg.as_ref().unwrap().rgb, Some((255, 0, 0)));
    let def = spans
        .iter()
        .find(|s| s.text == "default")
        .expect("default span");
    assert_eq!(def.style.fg, None);
}

#[test]
fn background_colour_set_and_reset() {
    let spans = para_spans("`B00ftint`bplain");
    let tint = spans.iter().find(|s| s.text == "tint").expect("tint span");
    assert_eq!(tint.style.bg, Some(Color::parse("00f")));
    assert_eq!(tint.style.bg.as_ref().unwrap().rgb, Some((0, 0, 255)));
    let plain = spans
        .iter()
        .find(|s| s.text == "plain")
        .expect("plain span");
    assert_eq!(plain.style.bg, None);
}

#[test]
fn grayscale_colour_resolves() {
    let c = Color::parse("g50");
    assert_eq!(c.rgb, Some((127, 127, 127)));
    let black = Color::parse("g00");
    assert_eq!(black.rgb, Some((0, 0, 0)));
}

#[test]
fn invalid_colour_keeps_raw_without_rgb() {
    let c = Color::parse("zzz");
    assert_eq!(c.raw, "zzz");
    assert_eq!(c.rgb, None);
}

#[test]
fn true_colour_six_hex_resolves() {
    // A `FT<rrggbb> foreground selects a full 24-bit colour.
    let spans = para_spans("`FT00ff80colour");
    let coloured = spans.iter().find(|s| s.text == "colour").expect("span");
    let fg = coloured.style.fg.as_ref().expect("fg colour");
    assert_eq!(fg.raw, "00ff80");
    assert_eq!(fg.rgb, Some((0x00, 0xff, 0x80)));
}

#[test]
fn true_colour_background_resolves() {
    let spans = para_spans("`BTff0000onred");
    let s = spans.iter().find(|s| s.text == "onred").expect("span");
    let bg = s.style.bg.as_ref().expect("bg colour");
    assert_eq!(bg.rgb, Some((0xff, 0x00, 0x00)));
    // The legacy three-nibble form still resolves alongside true colour.
    let legacy = para_spans("`Ff00red");
    let lspan = legacy
        .iter()
        .find(|s| s.text == "red")
        .expect("legacy span");
    let lfg = lspan.style.fg.as_ref().expect("legacy fg");
    assert_eq!(lfg.rgb, Some((0xff, 0x00, 0x00)));
}

// --- Alignment -----------------------------------------------------------

#[test]
fn alignment_center_right_default() {
    assert_eq!(para_spans("`ccentered")[0].style.align, Align::Center);
    assert_eq!(para_spans("`rright")[0].style.align, Align::Right);
    // `a resets to the default (left).
    assert_eq!(para_spans("`r`aback")[0].style.align, Align::Left);
    // The canonical parser no longer toggles an alignment off when the same
    // command repeats: `c`c stays centered (older NomadNet reverted to default).
    assert_eq!(para_spans("`c`ctwice")[0].style.align, Align::Center);
    // The `` ` `` reset still returns alignment to the default.
    assert_eq!(para_spans("`c`\u{0060}reset")[0].style.align, Align::Left);
}

// --- Links ---------------------------------------------------------------

#[test]
fn link_with_fields() {
    let spans = para_spans("`[Label`dest:/page/x.mu`f1=v1|f2=v2]");
    let link = spans[0].link.as_ref().expect("link");
    assert_eq!(spans[0].text, "Label");
    assert_eq!(link.label, "Label");
    assert_eq!(link.target, "dest:/page/x.mu");
    assert_eq!(link.fields, vec!["f1=v1".to_string(), "f2=v2".to_string()]);
}

#[test]
fn link_without_fields() {
    let spans = para_spans("`[Label`dest]");
    let link = spans[0].link.as_ref().expect("link");
    assert_eq!(link.label, "Label");
    assert_eq!(link.target, "dest");
    assert!(link.fields.is_empty());
}

#[test]
fn link_without_label_uses_target() {
    let spans = para_spans("`[https://example.org/]");
    let link = spans[0].link.as_ref().expect("link");
    assert_eq!(link.label, "https://example.org/");
    assert_eq!(link.target, "https://example.org/");
    assert_eq!(spans[0].text, "https://example.org/");
}

#[test]
fn unterminated_link_degrades_to_text() {
    // No closing `]`: the bracket is consumed, the rest is plain text.
    let spans = para_spans("`[no closing bracket");
    assert!(spans.iter().all(|s| s.link.is_none()));
    assert!(spans.iter().any(|s| s.text.contains("no closing bracket")));
}

// --- Anchors -------------------------------------------------------------

#[test]
fn anchor_declaration() {
    let spans = para_spans("`:section1 following text");
    assert_eq!(spans[0].anchor.as_deref(), Some("section1"));
    assert!(spans[0].text.is_empty());
    assert!(spans.iter().any(|s| s.text == " following text"));
}

#[test]
fn anchor_surfaces_at_document_level() {
    // An inline anchor is bound to the index of the block it was declared on.
    let doc = parse("first line\n`:target anchored line\nlast line");
    assert_eq!(doc.anchors.get("target"), Some(&1));
    // The span still carries the anchor for fine-grained navigation.
    if let Block::Paragraph { line, .. } = &doc.blocks[1] {
        assert!(line
            .spans
            .iter()
            .any(|s| s.anchor.as_deref() == Some("target")));
    } else {
        panic!("expected paragraph at block 1");
    }
}

#[test]
fn heading_generates_slug_anchor() {
    let doc = parse(">> Getting Started");
    // The heading auto-slug is lowercased and hyphenated.
    assert_eq!(doc.anchors.get("getting-started"), Some(&0));
    // The heading row is recorded in header_rows.
    assert_eq!(doc.header_rows, vec![0]);
}

#[test]
fn heading_slug_strips_markup() {
    // Colour/format commands are stripped before slugifying (canonical).
    let doc = parse("> `!Bold`! `F00ftitle Here");
    assert!(doc.anchors.contains_key("bold-title-here"));
}

#[test]
fn anchor_first_declaration_wins() {
    let doc = parse("`:dup one\n`:dup two");
    // The first block index for a repeated name is kept.
    assert_eq!(doc.anchors.get("dup"), Some(&0));
}

// --- Tables --------------------------------------------------------------

#[test]
fn table_toggle_buffers_rows() {
    let doc = parse("`t\nName | Age\n--- | ---\nAda | 36\n`t");
    let table = doc
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Table { rows, .. } => Some(rows),
            _ => None,
        })
        .expect("table block");
    assert_eq!(table.len(), 3);
    assert_eq!(table[0], "Name | Age");
    assert_eq!(table[2], "Ada | 36");
}

#[test]
fn table_captures_align_and_width() {
    let doc = parse("`tc80\nA | B\n- | -\n`t");
    match doc.blocks.iter().find(|b| matches!(b, Block::Table { .. })) {
        Some(Block::Table {
            align, max_width, ..
        }) => {
            assert_eq!(*align, Some(Align::Center));
            assert_eq!(*max_width, Some(80));
        }
        _ => panic!("expected table block"),
    }
}

#[test]
fn unterminated_table_is_flushed() {
    // A table left open at end of input surfaces its rows rather than vanishing.
    let doc = parse("`t\nA | B\n- | -");
    assert!(doc.blocks.iter().any(|b| matches!(b, Block::Table { .. })));
}

// --- Partials ------------------------------------------------------------

#[test]
fn partial_parses_url_refresh_fields() {
    let doc = parse("`{12ab:/page/x`5`a=1|b=2}");
    match doc.blocks.first() {
        Some(Block::Partial {
            url,
            refresh,
            fields,
            ..
        }) => {
            assert_eq!(url, "12ab:/page/x");
            assert_eq!(*refresh, Some(5.0));
            assert_eq!(fields, &vec!["a=1".to_string(), "b=2".to_string()]);
        }
        other => panic!("expected partial, got {other:?}"),
    }
}

#[test]
fn partial_refresh_below_one_is_none() {
    let doc = parse("`{12ab:/x`0.5}");
    match doc.blocks.first() {
        Some(Block::Partial { refresh, .. }) => assert_eq!(*refresh, None),
        other => panic!("expected partial, got {other:?}"),
    }
}

// --- Heading-with-field sanitization ------------------------------------

#[test]
fn heading_with_field_is_demoted_to_paragraph() {
    // A `>`-line that also contains a form field is not a heading.
    let doc = parse(">`<32|name`>");
    assert!(matches!(doc.blocks[0], Block::Paragraph { .. }));
    if let Block::Paragraph { line, .. } = &doc.blocks[0] {
        assert!(line.spans.iter().any(|s| s.field.is_some()));
    }
}

// --- Form fields ---------------------------------------------------------

#[test]
fn text_field_with_components() {
    let spans = para_spans("`<32|username`initial value>");
    let field = spans[0].field.as_ref().expect("field");
    assert_eq!(field.name, "username");
    assert_eq!(field.width, 32);
    assert_eq!(field.kind, FieldKind::Text);
    assert_eq!(field.value, "initial value");
    assert!(!field.masked);
}

#[test]
fn masked_field() {
    let spans = para_spans("`<!8|pin`>");
    let field = spans[0].field.as_ref().expect("field");
    assert_eq!(field.name, "pin");
    assert_eq!(field.width, 8);
    assert!(field.masked);
    assert_eq!(field.kind, FieldKind::Text);
}

#[test]
fn plain_field_without_components() {
    let spans = para_spans("`<simplename`seed>");
    let field = spans[0].field.as_ref().expect("field");
    assert_eq!(field.name, "simplename");
    assert_eq!(field.width, 24); // default
    assert_eq!(field.value, "seed");
}

#[test]
fn checkbox_field() {
    let spans = para_spans("`<?|agree`I agree>");
    let field = spans[0].field.as_ref().expect("field");
    assert_eq!(field.kind, FieldKind::Checkbox);
    assert_eq!(field.name, "agree");
    assert_eq!(field.label, "I agree");
    assert_eq!(field.value, "I agree"); // no explicit value -> label
}

#[test]
fn radio_field_with_value_and_prechecked() {
    let spans = para_spans("`<^|choice|val1|*`Option One>");
    let field = spans[0].field.as_ref().expect("field");
    assert_eq!(field.kind, FieldKind::Radio);
    assert_eq!(field.name, "choice");
    assert_eq!(field.value, "val1");
    assert_eq!(field.label, "Option One");
    assert!(field.prechecked);
}

#[test]
fn unterminated_field_degrades() {
    // No `` ` `` inside: not a valid field, consumed as text (no panic).
    let spans = para_spans("`<name with no backtick");
    assert!(spans.iter().all(|s| s.field.is_none()));
}

// --- Escapes -------------------------------------------------------------

#[test]
fn escape_line_disables_comment() {
    let spans = para_spans("\\# not a comment");
    assert_eq!(spans[0].text, "# not a comment");
}

#[test]
fn escaped_backtick_is_literal() {
    let spans = para_spans("a \\`b\\` c");
    let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(joined, "a `b` c");
}

// --- Graceful degradation -----------------------------------------------

#[test]
fn unterminated_colour_sequence_is_graceful() {
    // `F with fewer than 3 following chars: the command is ignored.
    let spans = para_spans("`F1");
    assert!(spans.iter().all(|s| s.style.fg.is_none()));
    assert!(spans.iter().any(|s| s.text == "1"));
}

#[test]
fn state_persists_across_lines() {
    // A colour set on one line carries to the next until reset.
    let doc = parse("`Ff00first line\nsecond line");
    let l1 = match &doc.blocks[0] {
        Block::Paragraph { line, .. } => line,
        other => panic!("{other:?}"),
    };
    let l2 = match &doc.blocks[1] {
        Block::Paragraph { line, .. } => line,
        other => panic!("{other:?}"),
    };
    assert_eq!(l1.spans[0].style.fg, Some(Color::parse("f00")));
    assert_eq!(l2.spans[0].style.fg, Some(Color::parse("f00")));
}

#[test]
fn blank_lines_become_blank_blocks() {
    let doc = parse("a\n\nb");
    assert!(matches!(doc.blocks[0], Block::Paragraph { .. }));
    assert!(matches!(doc.blocks[1], Block::Blank));
    assert!(matches!(doc.blocks[2], Block::Paragraph { .. }));
}

#[test]
fn empty_input_is_one_blank() {
    let doc = parse("");
    assert_eq!(doc.blocks, vec![Block::Blank]);
}

// --- README.mu whole-document parse -------------------------------------

#[test]
fn parses_full_readme_without_panicking() {
    let readme = include_str!("../../reference/Reticulum/README.mu");
    let doc = parse(readme);

    // Plausible block count: the file is a few hundred lines.
    assert!(
        doc.blocks.len() > 150,
        "unexpectedly few blocks: {}",
        doc.blocks.len()
    );

    // Structural sanity: headings, links and literal blocks are all present.
    assert!(doc
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Heading { depth: 2, .. })));
    assert!(doc
        .blocks
        .iter()
        .any(|b| matches!(b, Block::LiteralBlock { .. })));

    let has_link = doc.blocks.iter().any(|b| match b {
        Block::Paragraph { line, .. } | Block::Heading { line, .. } => line_has_link(line),
        _ => false,
    });
    assert!(has_link, "expected at least one link in README.mu");
}

fn line_has_link(line: &Line) -> bool {
    line.spans.iter().any(|s| s.link.is_some())
}

// --- Control-sequence filtering (Codeberg #281) --------------------------

/// Every string a document carries, so a test can assert that nothing hostile
/// survived anywhere in the model — not just in span text.
fn all_strings(doc: &leviculum_micron::MicronDocument) -> Vec<String> {
    fn from_line(line: &Line, out: &mut Vec<String>) {
        for span in &line.spans {
            out.push(span.text.clone());
            if let Some(link) = &span.link {
                out.push(link.label.clone());
                out.push(link.target.clone());
                out.extend(link.fields.iter().cloned());
            }
            if let Some(field) = &span.field {
                out.push(field.name.clone());
                out.push(field.value.clone());
                out.push(field.label.clone());
            }
            if let Some(anchor) = &span.anchor {
                out.push(anchor.clone());
            }
        }
    }
    let mut out = Vec::new();
    for block in &doc.blocks {
        match block {
            Block::Heading { line, .. } | Block::Paragraph { line, .. } => {
                from_line(line, &mut out)
            }
            Block::LiteralBlock { lines } => {
                for line in lines {
                    from_line(line, &mut out);
                }
            }
            Block::Table { rows, .. } => out.extend(rows.iter().cloned()),
            Block::Partial { url, fields, .. } => {
                out.push(url.clone());
                out.extend(fields.iter().cloned());
            }
            Block::Divider { character, .. } => out.push(character.to_string()),
            Block::Blank => {}
        }
    }
    out.extend(doc.anchors.keys().cloned());
    out
}

/// Assert that no string in the document can act on a terminal.
fn assert_escape_free(doc: &leviculum_micron::MicronDocument, what: &str) {
    for s in all_strings(doc) {
        if let Some(c) = s
            .chars()
            .find(|c| c.is_control() || ('\u{80}'..='\u{9f}').contains(c))
        {
            panic!("{what}: control char U+{:04X} survived in {s:?}", c as u32);
        }
    }
}

#[test]
fn hostile_csi_sequences_never_reach_the_document() {
    // Cursor positioning + erase: rewrites scrollback so earlier output lies.
    let doc = parse("Harmless\n\x1b[2J\x1b[HI am the bank\nMore text");
    assert_escape_free(&doc, "CSI");
    // The printable remainder is kept, so the page is still readable.
    let text: String = all_strings(&doc).join("");
    assert!(text.contains("Harmless"), "text was lost: {text:?}");
    assert!(text.contains("I am the bank"), "text was lost: {text:?}");
    // The sequence's printable tail (`[2J`) stays as inert text rather than
    // being swallowed: without the ESC it cannot act, and a reader who sees
    // the residue is told plainly that the page tried something.
    assert!(
        text.contains("[2J"),
        "the inert residue was eaten: {text:?}"
    );
}

#[test]
fn hostile_osc_sequences_never_reach_the_document() {
    // OSC 0 (window title), OSC 8 (hyperlink anywhere), OSC 52 (clipboard
    // write on terminals that still honour it).
    for src in [
        "title\x1b]0;pwned\x07 rest",
        "link\x1b]8;;https://evil.example/\x1b\\label\x1b]8;;\x1b\\ rest",
        "clip\x1b]52;c;cm0gLXJmIH4K\x07 rest",
    ] {
        let doc = parse(src);
        assert_escape_free(&doc, "OSC");
    }
}

#[test]
fn hostile_dcs_and_c1_sequences_never_reach_the_document() {
    // DCS via ESC P, and the single-byte C1 forms of CSI/OSC/DCS, which a
    // terminal in an 8-bit mode accepts without any ESC at all.
    let doc = parse("a\x1bPq#0;2;0;0;0#0~~\x1b\\b\u{9b}31mc\u{9d}0;x\u{9c}d\u{90}e\u{9f}f");
    assert_escape_free(&doc, "DCS/C1");
}

#[test]
fn carriage_return_cannot_rewrite_the_rendered_line() {
    // CR alone repaints the line the reader already saw. It is dropped, not
    // treated as a line break: the page keeps one line, with both halves.
    let doc = parse("You owe 5 EUR\rYou owe 5000 EUR");
    assert_escape_free(&doc, "CR");
    assert_eq!(doc.blocks.len(), 1, "CR must not split the line");
    let text: String = all_strings(&doc).join("");
    assert_eq!(text, "You owe 5 EURYou owe 5000 EUR");
}

#[test]
fn control_chars_are_stripped_inside_literal_blocks_and_tables() {
    // Literal blocks bypass inline parsing entirely, and table rows are kept
    // verbatim — both would carry an escape straight to the renderer.
    let doc = parse("`=\n\x1b[31mred forever\n`=");
    assert_escape_free(&doc, "literal block");
    let doc = parse("`t\na\x1b[2J|b\u{9b}0m\n`t");
    assert_escape_free(&doc, "table row");
}

#[test]
fn control_chars_are_stripped_from_link_targets_and_anchors() {
    let doc = parse("`[La\x1bbel`:/page/\x1b]0;x\x07evil]\n`:anc\x1bhor");
    assert_escape_free(&doc, "link/anchor");
}

#[test]
fn tab_becomes_a_space_so_one_cell_stays_one_column() {
    let spans = para_spans("col\tumn");
    assert_eq!(spans[0].text, "col umn");
}

#[test]
fn ordinary_text_survives_the_filter_untouched() {
    // The positive control: the filter must not eat legitimate content.
    let spans = para_spans("Grüße, 日本語, emoji 🛰, and math ∑ — all fine");
    assert_eq!(
        spans[0].text,
        "Grüße, 日本語, emoji 🛰, and math ∑ — all fine"
    );
    // The full README still parses to the same document it did before.
    let readme = include_str!("../../reference/Reticulum/README.mu");
    assert_escape_free(&parse(readme), "README.mu");
}
