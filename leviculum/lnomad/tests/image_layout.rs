//! Which links can become inline pictures, and what reserving rows for one
//! does to the laid-out page.
//!
//! Both are pure functions over the layout, so they are asserted here without
//! a terminal, a fetch or a decoder.

use leviculum_micron::parse;
use lnomad::render::{insert_image_rows, layout, layout_blocks, standalone_links, RLine};
use lnomad::theme::Theme;

/// Lay a page out at 60 columns, dark theme.
fn lay(mu: &str) -> (Vec<RLine>, Vec<lnomad::render::RenderedLink>) {
    layout(&parse(mu), 60, Theme::Dark)
}

/// The visible text of a laid-out line.
fn text(line: &RLine) -> String {
    line.cells.iter().map(|c| c.ch).collect()
}

#[test]
fn a_link_alone_on_its_line_is_standalone() {
    let (lines, links) = lay("`[Antenne`:/file/antenne.png]\n");
    assert_eq!(standalone_links(&lines, &links), vec![1]);
}

#[test]
fn a_link_inside_a_sentence_is_not() {
    // There is no rectangle to grow into without tearing the sentence apart,
    // so this stays an ordinary link.
    let (lines, links) = lay("See `[Antenne`:/file/antenne.png] for the mast.\n");
    assert!(standalone_links(&lines, &links).is_empty());
}

#[test]
fn indentation_and_trailing_space_do_not_disqualify_a_link() {
    let (lines, links) = lay(">> Section\n\n`[Antenne`:/file/antenne.png]\n");
    assert_eq!(
        standalone_links(&lines, &links),
        vec![1],
        "a link indented by its section is still alone on its line"
    );
}

#[test]
fn two_links_on_one_line_are_neither_standalone() {
    let (lines, links) = lay("`[A`:/file/a.png] `[B`:/file/b.png]\n");
    assert!(standalone_links(&lines, &links).is_empty());
}

#[test]
fn reserving_rows_pushes_the_text_below_down_and_moves_every_index_with_it() {
    let mu = "\
>> Kopf

`[Antenne`:/file/antenne.png]

Text unter dem Bild.
";
    let doc = parse(mu);
    let (mut lines, mut links, mut block_lines, mut fields) =
        layout_blocks(&doc, 60, Theme::Dark, &[]);
    let before_lines = lines.len();
    let link_line = links[0].line;
    let below: Vec<String> = lines[link_line + 1..].iter().map(text).collect();
    let blocks_before = block_lines.clone();

    insert_image_rows(
        &mut lines,
        &mut links,
        &mut fields,
        &mut block_lines,
        &[(1, 5)],
    );

    assert_eq!(lines.len(), before_lines + 5, "five rows must appear");
    assert_eq!(
        links[0].line, link_line,
        "the link itself must not move: the rows go under it"
    );
    for row in &lines[link_line + 1..link_line + 6] {
        assert!(row.cells.is_empty(), "the reserved rows must be blank");
    }
    let after: Vec<String> = lines[link_line + 6..].iter().map(text).collect();
    assert_eq!(after, below, "the text below must be intact, only moved");

    // Anchors are stored as block indices mapped to line numbers; a block
    // below the picture has to move with it or `#anchor` lands in the wrong
    // place.
    for (before, after) in blocks_before.iter().zip(block_lines.iter()) {
        let expected = match *before > link_line {
            true => before + 5,
            false => *before,
        };
        assert_eq!(*after, expected);
    }
}

#[test]
fn reserving_nothing_leaves_the_layout_byte_identical() {
    let mu = "`[Antenne`:/file/antenne.png]\n\nText.\n";
    let doc = parse(mu);
    let (mut lines, mut links, mut block_lines, mut fields) =
        layout_blocks(&doc, 60, Theme::Dark, &[]);
    let before: Vec<String> = lines.iter().map(text).collect();
    let links_before = links.clone();

    // A slot that reserves no rows (the "too many images on this page" state)
    // must leave the page exactly as it was.
    insert_image_rows(
        &mut lines,
        &mut links,
        &mut fields,
        &mut block_lines,
        &[(1, 0)],
    );

    assert_eq!(lines.iter().map(text).collect::<Vec<_>>(), before);
    assert_eq!(links, links_before);
}

#[test]
fn several_pictures_on_one_page_each_get_their_own_rows() {
    let mu = "\
`[Eins`:/file/eins.png]

`[Zwei`:/file/zwei.png]

Schluss.
";
    let doc = parse(mu);
    let (mut lines, mut links, mut block_lines, mut fields) =
        layout_blocks(&doc, 60, Theme::Dark, &[]);
    let first_line = links[0].line;
    let second_line = links[1].line;
    let tail = lines.len();

    insert_image_rows(
        &mut lines,
        &mut links,
        &mut fields,
        &mut block_lines,
        &[(1, 3), (2, 2)],
    );

    assert_eq!(lines.len(), tail + 5);
    assert_eq!(links[0].line, first_line, "the first link stays put");
    assert_eq!(
        links[1].line,
        second_line + 3,
        "the second link moves down by the first picture's rows"
    );
    // And its own rows are under it, not under the first picture.
    for row in &lines[links[1].line + 1..links[1].line + 3] {
        assert!(row.cells.is_empty());
    }
}

#[test]
fn a_reservation_for_an_unknown_link_is_ignored() {
    let (mut lines, mut links) = lay("`[A`:/file/a.png]\n");
    let mut fields = Vec::new();
    let mut block_lines = Vec::new();
    let before = lines.len();
    insert_image_rows(
        &mut lines,
        &mut links,
        &mut fields,
        &mut block_lines,
        &[(99, 4)],
    );
    assert_eq!(lines.len(), before);
}
