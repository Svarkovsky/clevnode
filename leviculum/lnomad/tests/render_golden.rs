//! Golden + layout tests for the render refactor.
//!
//! The golden test is the byte-identity guard for the `--print` / non-tty path:
//! the two golden files were captured from the renderer BEFORE the
//! target-agnostic-styled-lines refactor, so an exact match proves the refactor
//! changed no output bytes. The layout tests exercise the new intermediate
//! (`layout` -> `RLine` + positioned `RenderedLink`) directly.

use leviculum_micron::parse;
use lnomad::color::ColorDepth;
use lnomad::render::{layout, render_with_options};
use lnomad::theme::Theme;

const SAMPLE: &str = include_str!("fixtures/sample.mu");
const GOLDEN_COLOR: &str = include_str!("fixtures/golden_80_color.ansi");
const GOLDEN_PLAIN: &str = include_str!("fixtures/golden_80_plain.txt");

#[test]
fn print_output_is_byte_identical_to_golden_color() {
    let doc = parse(SAMPLE);
    // Pinned to true colour so the frozen golden stays 24-bit `38;2;r;g;b`.
    let page = render_with_options(&doc, 80, false, ColorDepth::Truecolor);
    assert_eq!(
        page.text, GOLDEN_COLOR,
        "--print (colour) output drifted from the frozen golden"
    );
}

#[test]
fn print_output_is_byte_identical_to_golden_plain() {
    let doc = parse(SAMPLE);
    let page = render_with_options(&doc, 80, true, ColorDepth::Truecolor);
    assert_eq!(
        page.text, GOLDEN_PLAIN,
        "--print (no_color) output drifted from the frozen golden"
    );
}

#[test]
fn narrower_width_produces_more_lines() {
    let doc = parse(SAMPLE);
    let (wide, _) = layout(&doc, 80, Theme::Dark);
    let (narrow, _) = layout(&doc, 40, Theme::Dark);
    // The plain paragraph rewraps: 40 columns needs strictly more rows than 80.
    assert!(
        narrow.len() > wide.len(),
        "expected rewrap to add lines: 80 -> {} rows, 40 -> {} rows",
        wide.len(),
        narrow.len()
    );
}

/// Visible text of an `RLine` (drops style), for locating a laid-out link.
fn line_text(line: &lnomad::render::RLine) -> String {
    line.cells.iter().map(|c| c.ch).collect()
}

#[test]
fn bullet_link_records_its_laid_out_position() {
    let doc = parse(SAMPLE);
    let (lines, links) = layout(&doc, 80, Theme::Dark);

    // The plain "• Alpha" link is link 1; its label starts at column 0 (no
    // leading whitespace) and the clickable span covers just "• Alpha" (no
    // visible `[N]` marker is appended).
    let alpha = links
        .iter()
        .find(|l| l.target == ":/page/alpha.mu")
        .expect("alpha link");
    let alpha_line = &lines[alpha.line];
    assert!(
        line_text(alpha_line).starts_with("• Alpha"),
        "alpha not at line start: {:?}",
        line_text(alpha_line)
    );
    assert_eq!(alpha.col_start, 0, "alpha label should start at column 0");
    // "• Alpha" = 7 chars -> exclusive end 7.
    assert_eq!(alpha.col_end, 7, "alpha clickable span end");
    // Every cell in the recorded span is underline-styled (the clickable core).
    for ci in alpha.col_start..alpha.col_end {
        assert!(
            alpha_line.cells[ci].st.underline,
            "cell {ci} in alpha span not underlined"
        );
        assert_eq!(alpha_line.cells[ci].link, Some(alpha.index));
    }
}

#[test]
fn leading_whitespace_of_link_is_not_underlined() {
    let doc = parse(SAMPLE);
    let (lines, links) = layout(&doc, 80, Theme::Dark);

    // The "  • Beta" link's label carries two leading spaces; those must render
    // plain (not underlined, not tagged), and col_start must point past them.
    let beta = links
        .iter()
        .find(|l| l.target == ":/page/beta.mu")
        .expect("beta link");
    let beta_line = &lines[beta.line];
    assert_eq!(
        beta.col_start, 2,
        "col_start should skip the two leading spaces"
    );
    for ci in 0..beta.col_start {
        let leading = &beta_line.cells[ci];
        assert_eq!(leading.ch, ' ', "expected a leading space at {ci}");
        assert!(!leading.st.underline, "leading whitespace was underlined");
        assert_eq!(leading.link, None, "leading whitespace was tagged as link");
    }
    // The clickable core itself is underlined + tagged.
    assert!(beta_line.cells[beta.col_start].st.underline);
    assert_eq!(beta_line.cells[beta.col_start].link, Some(beta.index));
}

/// A row's cells as `lnomad` splits them, through the public layout: the
/// splitting itself is private, so this drives it the way a page does.
fn table_cells(mu: &str) -> Vec<String> {
    let (lines, _) = layout(&parse(mu), 80, Theme::Dark);
    lines
        .iter()
        .map(|line| line.cells.iter().map(|c| c.ch).collect::<String>())
        .filter(|text| !text.trim().is_empty())
        .collect()
}

#[test]
fn a_table_row_splits_on_unescaped_pipes_only() {
    // lblogd escapes a literal pipe in a cell as `\|`, following the
    // reference parser. It must stay one cell, and the backslash must not
    // reach the screen.
    let rendered = table_cells("`t\na | b\n--- | ---\npipe \\| inside | 2\n`t\n");
    let row = rendered
        .iter()
        .find(|line| line.contains("pipe"))
        .expect("the data row must render");
    assert!(
        row.contains("pipe | inside"),
        "the escaped pipe must survive as text: {row:?}"
    );
    assert!(
        !row.contains('\\'),
        "and its backslash must not be shown: {row:?}"
    );
    // Two columns, so exactly one column separator.
    assert_eq!(
        row.matches('\u{2502}').count(),
        1,
        "the escaped pipe must not open a third column: {row:?}"
    );
}

#[test]
fn a_table_still_has_its_ordinary_columns() {
    let rendered = table_cells("`t\na | b\n--- | ---\n1 | 2\n`t\n");
    let row = rendered
        .iter()
        .find(|line| line.contains('1'))
        .expect("the data row must render");
    assert_eq!(row.matches('\u{2502}').count(), 1, "{row:?}");
}

#[test]
fn markup_inside_a_cell_renders_instead_of_showing_itself() {
    // A cell is inline micron. The reference re-parses each formatted row line
    // (NomadNet MicronParser.render_table), so a style in a cell has to render
    // — and lblogd emits exactly this for a Markdown code span.
    let (lines, _) = layout(
        &parse("`t\na | b\n--- | ---\n`!loud`! | `B333code`b\n`t\n"),
        80,
        Theme::Dark,
    );
    let row = lines
        .iter()
        .find(|line| line.cells.iter().any(|c| c.ch == 'l'))
        .expect("the data row must render");
    let text: String = row.cells.iter().map(|c| c.ch).collect();
    assert!(text.contains("loud"), "{text:?}");
    assert!(text.contains("code"), "{text:?}");
    assert!(
        !text.contains('`'),
        "no markup may reach the screen: {text:?}"
    );
    let l = row.cells.iter().find(|c| c.ch == 'l').unwrap();
    assert!(l.st.bold, "the bold toggle must apply to the cell text");
}

#[test]
fn a_column_is_sized_by_visible_width_not_by_its_markup() {
    // `B333` + `b is nine characters of markup around four of text. Sizing on
    // the raw string would pad every other row to thirteen.
    let (lines, _) = layout(&parse("`t\nh\n---\n`B333code`b\n`t\n"), 80, Theme::Dark);
    let widths: Vec<usize> = lines
        .iter()
        .map(|line| line.cells.iter().filter(|c| c.ch != ' ').count())
        .collect();
    assert!(
        widths.iter().all(|w| *w <= 4),
        "columns must be as wide as the text, not the markup: {widths:?}"
    );
}

#[test]
fn a_link_inside_a_cell_stays_a_link() {
    let (_, links) = layout(
        &parse("`t\nwhere\n---\n`[docs`:/page/x.mu]\n`t\n"),
        80,
        Theme::Dark,
    );
    let link = links.first().expect("the cell's link must be collected");
    assert_eq!(link.label, "docs");
    assert_eq!(link.target, ":/page/x.mu");
}

#[test]
fn a_cell_that_parses_to_nothing_still_shows_its_text() {
    // "- gone" is a divider at line start, so parsing the cell on its own
    // yields no text. Dropping a reader's content would be worse than showing
    // the stray character.
    let (lines, _) = layout(
        &parse("`t\nh | h2\n--- | ---\n- gone | kept\n`t\n"),
        80,
        Theme::Dark,
    );
    let text: String = lines
        .iter()
        .map(|line| line.cells.iter().map(|c| c.ch).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("gone"), "{text:?}");
    assert!(text.contains("kept"), "{text:?}");
}
