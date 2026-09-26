//! What the pages tell a reader about the blog, on both sides.
//!
//! The renderers are pure functions over `BlogMeta` and the posts, so this
//! asserts the rendered output directly rather than through a server. The
//! Micron assertions parse the result with `leviculum-micron` so a change
//! that produced plausible-looking but invalid markup would still fail.

use lblogd::post::{Date, Post};
use lblogd::render::{
    render_index_html, render_index_micron, render_post_html, render_post_micron, BlogMeta,
    DEFAULT_STYLE,
};
use leviculum_micron::parse;

fn date(s: &str) -> Date {
    s.parse().expect("parse fixture date")
}

fn post(title: &str, author: Option<&str>) -> Post {
    Post {
        title: title.to_string(),
        date: date("2026-07-12"),
        author: author.map(str::to_string),
        slug: lblogd::post::slugify(title),
        body_md: "Body text.\n".to_string(),
    }
}

/// A fully populated blog: every optional field present.
fn full_meta() -> BlogMeta {
    BlogMeta {
        title: "leviculum.network".to_string(),
        author: Some("Lew Palm".to_string()),
        description: Some("Notizen zur Reticulum-Implementierung".to_string()),
        language: "de".to_string(),
        web_url: Some("https://leviculum.network".to_string()),
        nomadnet_address: Some("0ec84236630cea839d80a71c39fb41ce".to_string()),
        email: None,
        lxmf: None,
        has_about: false,
    }
}

/// The other extreme: only a title, as a minimal config produces.
fn bare_meta() -> BlogMeta {
    BlogMeta {
        title: "Bare Blog".to_string(),
        language: "en".to_string(),
        ..BlogMeta::default()
    }
}

#[test]
fn html_index_introduces_the_blog() {
    let html = render_index_html(&full_meta(), DEFAULT_STYLE, &[post("First", None)]);

    assert!(html.contains("<h1>leviculum.network</h1>"), "{html}");
    assert!(html.contains("by Lew Palm"), "{html}");
    assert!(
        html.contains("Notizen zur Reticulum-Implementierung"),
        "{html}"
    );
    // The tagline doubles as the meta description, which is what a search
    // result or a chat preview shows.
    assert!(
        html.contains(
            "<meta name=\"description\" content=\"Notizen zur Reticulum-Implementierung\">"
        ),
        "{html}"
    );
    assert!(html.contains("<html lang=\"de\">"), "{html}");
}

#[test]
fn html_names_the_nomadnet_address() {
    // A clearnet reader has no other way to discover the mesh side.
    let html = render_index_html(&full_meta(), DEFAULT_STYLE, &[post("First", None)]);
    assert!(
        html.contains("0ec84236630cea839d80a71c39fb41ce"),
        "the index must name the NomadNet address: {html}"
    );

    let html = render_post_html(&full_meta(), DEFAULT_STYLE, &post("First", None));
    assert!(
        html.contains("0ec84236630cea839d80a71c39fb41ce"),
        "so must a post page: {html}"
    );
}

#[test]
fn micron_names_the_web_url() {
    // The mirror: a mesh reader who wants to share the blog off-mesh.
    let micron = render_index_micron(&full_meta(), &[post("First", None)]);
    assert!(
        micron.contains("https://leviculum.network"),
        "the index must name the web URL: {micron}"
    );

    let micron = render_post_micron(&full_meta(), &post("First", None));
    assert!(
        micron.contains("https://leviculum.network"),
        "so must a post page: {micron}"
    );
}

#[test]
fn a_post_page_links_back_to_the_index() {
    let html = render_post_html(&full_meta(), DEFAULT_STYLE, &post("First", None));
    assert!(
        html.contains("<a href=\"/\">"),
        "the HTML post needs a way back: {html}"
    );

    let micron = render_post_micron(&full_meta(), &post("First", None));
    assert!(
        micron.contains("`:/page/index.mu]"),
        "the micron post needs a way back: {micron}"
    );
}

#[test]
fn a_guest_author_is_named_and_the_usual_one_is_not_repeated() {
    let meta = full_meta();
    let posts = vec![post("Mine", None), post("Theirs", Some("Someone Else"))];

    let html = render_index_html(&meta, DEFAULT_STYLE, &posts);
    // "by Lew Palm" appears once, as the blog's byline, and not again on the
    // post he wrote: repeating the same name on every line is noise.
    assert_eq!(html.matches("by Lew Palm").count(), 1, "{html}");
    assert!(html.contains("by Someone Else"), "{html}");

    let micron = render_index_micron(&meta, &posts);
    assert_eq!(micron.matches("by Lew Palm").count(), 1, "{micron}");
    assert!(micron.contains("by Someone Else"), "{micron}");

    // On the post page itself the author is always named, since there is no
    // list to compare against.
    let own = render_post_html(&meta, DEFAULT_STYLE, &posts[0]);
    assert!(own.contains("Lew Palm"), "{own}");
    let guest = render_post_html(&meta, DEFAULT_STYLE, &posts[1]);
    assert!(guest.contains("Someone Else"), "{guest}");
    assert!(!guest.contains("Lew Palm"), "{guest}");
}

#[test]
fn absent_fields_are_omitted_rather_than_rendered_empty() {
    let meta = bare_meta();
    let html = render_index_html(&meta, DEFAULT_STYLE, &[post("First", None)]);
    assert!(html.contains("<h1>Bare Blog</h1>"), "{html}");
    assert!(!html.contains("class=\"byline\""), "no author, no byline");
    assert!(!html.contains("class=\"tagline\""), "no tagline element");
    assert!(!html.contains("<meta name=\"description\""), "{html}");
    assert!(!html.contains("<footer>"), "no address, no footer: {html}");
    assert!(
        !html.contains(" by "),
        "nothing dangling after 'by': {html}"
    );

    let micron = render_index_micron(&meta, &[post("First", None)]);
    assert!(!micron.contains("by "), "{micron}");
    assert!(!micron.contains("Also on the web"), "{micron}");
}

#[test]
fn the_custom_stylesheet_is_what_gets_inlined() {
    let css = "body{background:rebeccapurple}";
    let html = render_index_html(&bare_meta(), css, &[post("First", None)]);
    assert!(html.contains(css), "{html}");
    assert!(
        !html.contains("max-width:42rem"),
        "the built-in stylesheet must not be emitted alongside it: {html}"
    );
}

#[test]
fn micron_pages_stay_valid_micron_with_all_the_extras() {
    // The identity lines, bylines and footer are new markup on a format with
    // line-level control characters; parsing back is what proves they did not
    // turn into something the client would misread.
    let posts = vec![post("Erster", None), post("Zweiter", Some("Gast"))];

    let doc = parse(&render_index_micron(&full_meta(), &posts));
    assert!(
        matches!(
            doc.blocks.first(),
            Some(leviculum_micron::Block::Heading { depth: 1, .. })
        ),
        "index must open with the blog heading: {:?}",
        doc.blocks.first()
    );

    let doc = parse(&render_post_micron(&full_meta(), &posts[1]));
    assert!(
        matches!(
            doc.blocks.first(),
            Some(leviculum_micron::Block::Heading { depth: 1, .. })
        ),
        "a post must open with its title: {:?}",
        doc.blocks.first()
    );
}
