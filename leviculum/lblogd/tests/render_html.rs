//! Tests for the Markdown-to-HTML renderer and the HTML page templates.

use lblogd::post::{parse_post, PostDefaults};
use lblogd::render::{
    markdown_to_html, render_index_html, render_post_html, BlogMeta, DEFAULT_STYLE,
};

#[test]
fn headings_are_demoted_one_level() {
    // The page template owns <h1> for the post title, so a body heading
    // starts at <h2>; otherwise a post would have two top-level headings.
    assert!(markdown_to_html("# One").contains("<h2>One</h2>"));
    assert!(markdown_to_html("## Two").contains("<h3>Two</h3>"));
    assert!(markdown_to_html("### Three").contains("<h4>Three</h4>"));
    // h6 has nowhere left to go.
    assert!(markdown_to_html("###### Six").contains("<h6>Six</h6>"));
}

#[test]
fn a_post_has_exactly_one_h1() {
    let mut post = sample_post("The Title");
    post.body_md = "# Section\n\nText.\n".to_string();
    let html = render_post_html(&fixture_meta(), DEFAULT_STYLE, &post);
    assert_eq!(html.matches("<h1>").count(), 1, "{html}");
    assert!(html.contains("<h1>The Title</h1>"), "{html}");
    assert!(html.contains("<h2>Section</h2>"), "{html}");
}

#[test]
fn bold_and_italic() {
    assert!(markdown_to_html("**bold**").contains("<strong>bold</strong>"));
    assert!(markdown_to_html("*italic*").contains("<em>italic</em>"));
}

#[test]
fn inline_code_and_code_block() {
    assert!(markdown_to_html("`code`").contains("<code>code</code>"));
    let html = markdown_to_html("```\nlet x = 1;\n```");
    assert!(html.contains("<pre><code>"));
    assert!(html.contains("let x = 1;"));
}

#[test]
fn link() {
    let html = markdown_to_html("[text](https://example.com)");
    assert!(html.contains("<a href=\"https://example.com\">text</a>"));
}

#[test]
fn bullet_list() {
    let html = markdown_to_html("- one\n- two\n");
    assert!(html.contains("<ul>"));
    assert!(html.contains("<li>one</li>"));
    assert!(html.contains("<li>two</li>"));
}

#[test]
fn numbered_list() {
    let html = markdown_to_html("1. first\n2. second\n");
    assert!(html.contains("<ol>"));
    assert!(html.contains("<li>first</li>"));
    assert!(html.contains("<li>second</li>"));
}

#[test]
fn an_image_in_the_file_area_points_at_the_web_route() {
    // The author writes the name of a file that sits beside the posts; the
    // web side serves it from /files/, the mesh side as :/file/.
    for md in [
        "![Antenne](antenne.jpg)",
        "![Antenne](./antenne.jpg)",
        "![Antenne](files/antenne.jpg)",
        "![Antenne](/files/antenne.jpg)",
    ] {
        let html = markdown_to_html(md);
        assert!(
            html.contains("<img src=\"/files/antenne.jpg\" alt=\"Antenne\""),
            "input {md} produced {html}"
        );
    }
}

#[test]
fn an_external_image_is_left_exactly_as_written() {
    let html = markdown_to_html("![Antenne](https://example.com/antenne.jpg)");
    assert!(
        html.contains("<img src=\"https://example.com/antenne.jpg\""),
        "{html}"
    );
}

fn sample_post(title: &str) -> lblogd::post::Post {
    let src = format!("+++\ntitle = \"{title}\"\ndate = \"2026-07-12\"\n+++\n\nSome **body**.\n");
    parse_post(&src, &fixture_defaults()).unwrap()
}

#[test]
fn post_html_is_a_complete_document() {
    let html = render_post_html(&fixture_meta(), DEFAULT_STYLE, &sample_post("A Post"));
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<title>A Post</title>"));
    assert!(html.contains("<style>"));
    assert!(html.contains("<h1>A Post</h1>"));
    assert!(html.contains("2026-07-12"));
    assert!(html.contains("<strong>body</strong>"));
    assert!(html.ends_with("</html>\n"));
}

#[test]
fn post_html_escapes_the_title() {
    let html = render_post_html(
        &fixture_meta(),
        DEFAULT_STYLE,
        &sample_post("Tags <b> and &"),
    );
    assert!(html.contains("Tags &lt;b&gt; and &amp;"));
    assert!(!html.contains("<title>Tags <b>"));
}

#[test]
fn index_html_lists_posts_with_links() {
    let posts = vec![sample_post("First Post"), sample_post("Second Post")];
    let html = render_index_html(&fixture_meta(), DEFAULT_STYLE, &posts);
    assert!(html.starts_with("<!doctype html>"));
    // Headed by the blog, not by the word "Posts".
    assert!(html.contains("<h1>Test Blog</h1>"), "{html}");
    assert!(html.contains("<a href=\"/posts/first-post\">First Post</a>"));
    assert!(html.contains("<a href=\"/posts/second-post\">Second Post</a>"));
    assert!(html.contains("2026-07-12"));
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
