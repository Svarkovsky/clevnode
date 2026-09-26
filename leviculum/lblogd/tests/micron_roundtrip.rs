//! Round-trip tests for the Markdown-to-micron renderer: every construct is
//! rendered to micron and then parsed back with `leviculum-micron` (the
//! parser that defines what valid micron is). Asserting on the parsed
//! document structure proves the generator emits micron that our own parser,
//! and hence lnomad and NomadNet, accept.

use lblogd::post::{parse_post, PostDefaults};
use lblogd::render::{markdown_to_micron, render_index_micron, render_post_micron, BlogMeta};
use leviculum_micron::{parse, Block, Color, Line, MicronDocument};

/// Render Markdown to micron and parse it back.
fn roundtrip(md: &str) -> MicronDocument {
    parse(&markdown_to_micron(md))
}

/// The concatenated span text of a line.
fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.text.as_str()).collect()
}

/// All blocks that are not blanks, in order.
fn content_blocks(doc: &MicronDocument) -> Vec<&Block> {
    doc.blocks
        .iter()
        .filter(|b| !matches!(b, Block::Blank))
        .collect()
}

#[test]
fn headings_map_to_micron_depths_and_clamp() {
    // Body headings are demoted one level: the page template already spent
    // depth 1 on the post title, so `#` in the body is a section under it,
    // not a second title. Micron has three levels, so the clamp bites sooner.
    let doc = roundtrip("# One\n\n## Two\n\n### Three\n\n#### Four\n");
    let blocks = content_blocks(&doc);
    let heads: Vec<(u8, String)> = blocks
        .iter()
        .map(|b| match b {
            Block::Heading { depth, line } => (*depth, line_text(line)),
            other => panic!("expected only headings, got {other:?}"),
        })
        .collect();
    assert_eq!(
        heads,
        [
            (2, "One".to_string()),
            (3, "Two".to_string()),
            (3, "Three".to_string()), // clamped
            (3, "Four".to_string()),  // clamped
        ]
    );
}

#[test]
fn bold_becomes_a_bold_span() {
    let doc = roundtrip("some **bold** text");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    assert_eq!(line_text(line), "some bold text");
    let bold = line.spans.iter().find(|s| s.text == "bold").unwrap();
    assert!(bold.style.bold);
    let plain = line.spans.iter().find(|s| s.text == "some ").unwrap();
    assert!(!plain.style.bold);
}

#[test]
fn italic_becomes_an_italic_span() {
    let doc = roundtrip("an *italic* word");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    let italic = line.spans.iter().find(|s| s.text == "italic").unwrap();
    assert!(italic.style.italic);
    assert!(!line.spans[0].style.italic);
}

#[test]
fn styles_do_not_leak_into_the_next_paragraph() {
    let doc = roundtrip("**bold** here\n\nplain there");
    let blocks = content_blocks(&doc);
    let Block::Paragraph { line, .. } = blocks[1] else {
        panic!("expected paragraph, got {:?}", blocks[1]);
    };
    assert!(line.spans.iter().all(|s| !s.style.bold));
}

#[test]
fn inline_code_is_set_off_with_a_background() {
    let doc = roundtrip("run `cargo test` now");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    let code = line.spans.iter().find(|s| s.text == "cargo test").unwrap();
    assert_eq!(code.style.bg, Some(Color::parse("333")));
    let after = line.spans.iter().find(|s| s.text == " now").unwrap();
    assert_eq!(after.style.bg, None);
}

#[test]
fn fenced_code_becomes_a_literal_block() {
    // Includes a line reading exactly `= to prove the toggle gets escaped.
    let doc = roundtrip("```\nlet x = 1;\n`=\nfn f() {}\n```\n");
    let [Block::LiteralBlock { lines }] = content_blocks(&doc)[..] else {
        panic!("expected one literal block, got {:?}", doc.blocks);
    };
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(texts, ["let x = 1;", "`=", "fn f() {}"]);
}

#[test]
fn link_becomes_a_micron_link_span() {
    let doc = roundtrip("see [the docs](https://example.com/x) here");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    let link_span = line.spans.iter().find(|s| s.link.is_some()).unwrap();
    let link = link_span.link.as_ref().unwrap();
    assert_eq!(link.label, "the docs");
    assert_eq!(link.target, "https://example.com/x");
    assert_eq!(link_span.text, "the docs");
}

#[test]
fn styles_inside_link_labels_degrade_to_plain_label_text() {
    let doc = roundtrip("[**bold** label](https://example.com)");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    let link = line.spans[0].link.as_ref().unwrap();
    assert_eq!(link.label, "bold label");
}

#[test]
fn bullet_list_becomes_bullet_lines() {
    let doc = roundtrip("- one\n- two\n");
    let blocks = content_blocks(&doc);
    let texts: Vec<String> = blocks
        .iter()
        .map(|b| match b {
            Block::Paragraph { line, .. } => line_text(line),
            other => panic!("expected paragraphs, got {other:?}"),
        })
        .collect();
    assert_eq!(texts, ["\u{2022} one", "\u{2022} two"]);
}

#[test]
fn numbered_list_becomes_numbered_lines() {
    let doc = roundtrip("1. first\n2. second\n");
    let blocks = content_blocks(&doc);
    let texts: Vec<String> = blocks
        .iter()
        .map(|b| match b {
            Block::Paragraph { line, .. } => line_text(line),
            other => panic!("expected paragraphs, got {other:?}"),
        })
        .collect();
    assert_eq!(texts, ["1. first", "2. second"]);
}

#[test]
fn nested_list_is_indented() {
    let doc = roundtrip("- outer\n  - inner\n");
    let blocks = content_blocks(&doc);
    let texts: Vec<String> = blocks
        .iter()
        .map(|b| match b {
            Block::Paragraph { line, .. } => line_text(line),
            other => panic!("expected paragraphs, got {other:?}"),
        })
        .collect();
    assert_eq!(texts, ["\u{2022} outer", "  \u{2022} inner"]);
}

#[test]
fn paragraphs_are_blank_separated() {
    let doc = roundtrip("first para\n\nsecond para\n");
    let kinds: Vec<&Block> = doc.blocks.iter().collect();
    assert!(matches!(kinds[0], Block::Paragraph { .. }));
    assert!(matches!(kinds[1], Block::Blank));
    assert!(matches!(kinds[2], Block::Paragraph { .. }));
}

#[test]
fn rule_becomes_a_divider() {
    let doc = roundtrip("before\n\n---\n\nafter\n");
    assert!(doc
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Divider { .. })));
}

#[test]
fn image_in_the_file_area_becomes_a_link_to_the_file() {
    // Micron has no image construct, so the picture reaches a mesh reader the
    // only way NomadNet offers: as a file link. NomadNet saves it to the
    // reader's download directory; lnomad draws it in the page.
    let doc = roundtrip("![a rig photo](rig.png)\n");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    let link = line.spans[0].link.as_ref().expect("image must be a link");
    assert_eq!(link.label, "a rig photo");
    assert_eq!(link.target, ":/file/rig.png");
    assert_eq!(line_text(line), "a rig photo");
}

#[test]
fn image_without_alt_text_is_labelled_with_its_file_name() {
    let doc = roundtrip("![](rig.png)\n");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    let link = line.spans[0].link.as_ref().expect("image must be a link");
    assert_eq!(link.label, "rig.png");
    assert_eq!(link.target, ":/file/rig.png");
}

#[test]
fn the_file_area_spellings_all_reach_the_same_target() {
    for md in [
        "![p](rig.png)",
        "![p](./rig.png)",
        "![p](files/rig.png)",
        "![p](/files/rig.png)",
    ] {
        let doc = roundtrip(md);
        let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
            panic!("expected one paragraph for {md}, got {:?}", doc.blocks);
        };
        let link = line.spans[0].link.as_ref().expect("image must be a link");
        assert_eq!(link.target, ":/file/rig.png", "input {md}");
    }
}

#[test]
fn an_external_image_still_degrades_to_alt_text() {
    // Nothing on the mesh can fetch it, and a micron link to an `https://`
    // target would be a destination hash NomadNet cannot resolve.
    let doc = roundtrip("An image: ![a rig photo](https://example.com/rig.png) inline.\n");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    assert_eq!(line_text(line), "An image: [image: a rig photo] inline.");
    assert!(line.spans.iter().all(|s| s.link.is_none()));
}

#[test]
fn a_table_becomes_a_micron_table() {
    // Micron has tables — NomadNet's parser buffers the lines between two
    // `` `t `` toggles and hands them to the same Markdown-shaped formatter
    // (RNS rngit util.py:530-547) — so a table stays a table on the mesh.
    let doc = roundtrip("| a | b |\n|---|---|\n| 1 | 2 |\n");
    let [Block::Table { rows, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one table, got {:?}", doc.blocks);
    };
    assert_eq!(
        rows,
        &[
            "a | b".to_string(),
            "--- | ---".to_string(),
            "1 | 2".to_string()
        ],
        "the second line must be the alignment row the reference reads"
    );
}

#[test]
fn the_alignment_row_carries_the_markdown_alignment() {
    // The reference reads `:x:` as centre and `x:` as right
    // (rngit util.py:656-664); anything else is left.
    let doc = roundtrip("| l | c | r |\n|:---|:-:|---:|\n| 1 | 2 | 3 |\n");
    let [Block::Table { rows, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one table, got {:?}", doc.blocks);
    };
    assert_eq!(rows[1], "--- | :---: | ---:");
}

#[test]
fn a_pipe_inside_a_cell_is_escaped_so_it_stays_one_cell() {
    let doc = roundtrip("| a | b |\n|---|---|\n| pipe \\| inside | 2 |\n");
    let [Block::Table { rows, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one table, got {:?}", doc.blocks);
    };
    assert_eq!(rows[2], "pipe \\| inside | 2");
}

#[test]
fn a_link_inside_a_cell_survives_as_a_link() {
    let doc = roundtrip("| where |\n|---|\n| [docs](https://example.com/x) |\n");
    let [Block::Table { rows, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one table, got {:?}", doc.blocks);
    };
    assert_eq!(rows[2], "`[docs`https://example.com/x]");
}

#[test]
fn blockquote_degrades_to_indented_text() {
    let doc = roundtrip("> quoted text\n");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    assert_eq!(line_text(line), "  quoted text");
}

#[test]
fn leading_control_characters_in_text_are_escaped() {
    // Each would be swallowed as micron markup (heading, comment, divider,
    // depth reset) if the generator did not line-escape it.
    for (md, expected) in [
        ("\\> not a heading", "> not a heading"),
        ("\\# not a comment", "# not a comment"),
        ("\\- not a divider", "- not a divider"),
        ("< not a depth reset", "< not a depth reset"),
    ] {
        let doc = roundtrip(md);
        let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
            panic!("{md:?}: expected one paragraph, got {:?}", doc.blocks);
        };
        assert_eq!(line_text(line), expected);
    }
}

#[test]
fn backticks_and_backslashes_in_text_are_escaped() {
    let doc = roundtrip("tick \\` mark and back \\\\ slash");
    let [Block::Paragraph { line, .. }] = content_blocks(&doc)[..] else {
        panic!("expected one paragraph, got {:?}", doc.blocks);
    };
    assert_eq!(line_text(line), "tick ` mark and back \\ slash");
}

#[test]
fn empty_input_renders_and_parses_cleanly() {
    assert_eq!(markdown_to_micron(""), "");
    let _ = roundtrip("");
}

fn sample_post(title: &str, date: &str) -> lblogd::post::Post {
    let src = format!("+++\ntitle = \"{title}\"\ndate = \"{date}\"\n+++\n\nBody of {title}.\n");
    parse_post(&src, &fixture_defaults()).unwrap()
}

#[test]
fn index_micron_lists_posts_as_local_page_links() {
    let posts = vec![
        sample_post("First Post", "2026-07-12"),
        sample_post("Second Post", "2026-07-01"),
    ];
    let doc = parse(&render_index_micron(&fixture_meta(), &posts));
    let Block::Heading { depth: 1, line } = &doc.blocks[0] else {
        panic!("expected index heading, got {:?}", doc.blocks[0]);
    };
    // The index is headed by the blog, not by the word "Posts": a reader
    // arriving here should learn whose blog this is.
    assert_eq!(line_text(line), "Test Blog");

    let links: Vec<_> = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Paragraph { line, .. } => line.spans.iter().find_map(|s| s.link.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].target, ":/page/first-post.mu");
    assert_eq!(links[0].label, "2026-07-12 First Post");
    assert_eq!(links[1].target, ":/page/second-post.mu");
}

#[test]
fn post_micron_has_title_heading_and_date() {
    let post = sample_post("A Field Report", "2026-07-12");
    let doc = parse(&render_post_micron(&fixture_meta(), &post));
    let Block::Heading { depth: 1, line } = &doc.blocks[0] else {
        panic!("expected title heading, got {:?}", doc.blocks[0]);
    };
    assert_eq!(line_text(line), "A Field Report");
    let all_text: String = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Paragraph { line, .. } => Some(line_text(line)),
            _ => None,
        })
        .collect();
    assert!(all_text.contains("2026-07-12"));
    assert!(all_text.contains("Body of A Field Report."));
    assert!(doc
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Divider { .. })));
}

/// Defaults for fixtures that always set title and date themselves.
fn fixture_defaults() -> PostDefaults {
    PostDefaults {
        title: "fixture".to_string(),
        date: "2000-01-01".parse().unwrap(),
    }
}

/// Blog metadata for fixtures that are about rendering, not about identity.
fn fixture_meta() -> BlogMeta {
    BlogMeta {
        title: "Test Blog".to_string(),
        author: None,
        description: None,
        language: "en".to_string(),
        web_url: None,
        nomadnet_address: None,
        email: None,
        lxmf: None,
        has_about: false,
    }
}
