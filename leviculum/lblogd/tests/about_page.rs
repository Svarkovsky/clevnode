//! The about page: what it shows, when it exists, and how the author's name
//! links to it on both sides.

use lblogd::content::{load_snapshot, Sources, ABOUT_PATH};
use lblogd::post::{Date, Post};
use lblogd::render::{
    render_about_html, render_about_micron, render_index_html, render_index_micron,
    render_post_html, render_post_micron, BlogMeta, DEFAULT_STYLE,
};
use leviculum_micron::parse;

const LXMF: &str = "0ec84236630cea839d80a71c39fb41ce";

fn post(title: &str, author: Option<&str>) -> Post {
    Post {
        title: title.to_string(),
        date: "2026-07-12".parse::<Date>().expect("parse fixture date"),
        author: author.map(str::to_string),
        slug: lblogd::post::slugify(title),
        body_md: "Body text.\n".to_string(),
    }
}

/// A blog with an about page and full contact details.
fn meta_with_about() -> BlogMeta {
    BlogMeta {
        title: "leviculum.network".to_string(),
        author: Some("Lew Palm".to_string()),
        language: "en".to_string(),
        email: Some("lp@lew-palm.de".to_string()),
        lxmf: Some(LXMF.to_string()),
        has_about: true,
        ..BlogMeta::default()
    }
}

/// The same blog without one.
fn meta_without_about() -> BlogMeta {
    BlogMeta {
        title: "leviculum.network".to_string(),
        author: Some("Lew Palm".to_string()),
        language: "en".to_string(),
        ..BlogMeta::default()
    }
}

#[test]
fn the_about_page_shows_the_contact_details() {
    let html = render_about_html(&meta_with_about(), DEFAULT_STYLE, None);
    assert!(html.contains("<h1>Lew Palm</h1>"), "{html}");
    assert!(
        html.contains("<a href=\"mailto:lp@lew-palm.de\">lp@lew-palm.de</a>"),
        "the address must be actionable: {html}"
    );
    // The LXMF hash is not a link in a browser: nothing there can act on it.
    assert!(html.contains(&format!("<code>{LXMF}</code>")), "{html}");
    assert!(!html.contains(&format!("href=\"lxmf@{LXMF}\"")), "{html}");
    assert!(html.contains("<a href=\"/\">"), "a way back: {html}");
}

#[test]
fn the_micron_about_page_links_the_lxmf_address() {
    // NomadNet opens a conversation for a `lxmf@<hash>` target; the hash must
    // be exactly 32 hex characters or it refuses the link.
    let micron = render_about_micron(&meta_with_about(), None);
    assert!(micron.contains(&format!("`lxmf@{LXMF}]")), "{micron}");
    assert_eq!(LXMF.len(), 32);
    assert!(micron.contains("lp@lew-palm.de"), "{micron}");
    assert!(micron.contains("`:/page/index.mu]"), "a way back: {micron}");

    // The extra markup must still parse as micron.
    let doc = parse(&micron);
    assert!(
        matches!(
            doc.blocks.first(),
            Some(leviculum_micron::Block::Heading { depth: 1, .. })
        ),
        "{:?}",
        doc.blocks.first()
    );
}

#[test]
fn the_author_name_links_to_the_about_page_on_both_sides() {
    let meta = meta_with_about();
    let posts = [post("A Post", None)];

    let html = render_index_html(&meta, DEFAULT_STYLE, &posts);
    assert!(
        html.contains("<a href=\"/about\">Lew Palm</a>"),
        "index byline links: {html}"
    );
    let html = render_post_html(&meta, DEFAULT_STYLE, &posts[0]);
    assert!(
        html.contains("<a href=\"/about\">Lew Palm</a>"),
        "post byline links: {html}"
    );

    let micron = render_index_micron(&meta, &posts);
    assert!(
        micron.contains("`[Lew Palm`:/page/about.mu]"),
        "index byline links: {micron}"
    );
    let micron = render_post_micron(&meta, &posts[0]);
    assert!(
        micron.contains("`[Lew Palm`:/page/about.mu]"),
        "post byline links: {micron}"
    );
}

#[test]
fn without_an_about_page_the_name_stays_plain() {
    let meta = meta_without_about();
    let posts = [post("A Post", None)];

    let html = render_index_html(&meta, DEFAULT_STYLE, &posts);
    assert!(html.contains("by Lew Palm"), "{html}");
    assert!(
        !html.contains("/about"),
        "a link to a page with nothing on it is worse than none: {html}"
    );

    let micron = render_index_micron(&meta, &posts);
    assert!(!micron.contains("about.mu"), "{micron}");
}

#[test]
fn a_guest_author_is_never_linked_to_the_blog_authors_page() {
    // The about page belongs to the blog's author; pointing a guest's name at
    // it would credit the wrong person.
    let meta = meta_with_about();
    let posts = [post("Theirs", Some("Someone Else"))];

    let html = render_index_html(&meta, DEFAULT_STYLE, &posts);
    assert!(html.contains("by Someone Else"), "{html}");
    assert!(
        !html.contains("<a href=\"/about\">Someone Else</a>"),
        "{html}"
    );

    let html = render_post_html(&meta, DEFAULT_STYLE, &posts[0]);
    assert!(
        !html.contains("<a href=\"/about\">Someone Else</a>"),
        "{html}"
    );

    let micron = render_post_micron(&meta, &posts[0]);
    assert!(!micron.contains("`[Someone Else`"), "{micron}");
}

#[test]
fn the_text_file_is_shown_and_titles_the_page() {
    let text = Post {
        title: "Über mich".to_string(),
        date: "2026-01-01".parse().expect("date"),
        author: None,
        slug: "ueber-mich".to_string(),
        body_md: "Ich baue **Netze**.\n".to_string(),
    };
    let html = render_about_html(&meta_with_about(), DEFAULT_STYLE, Some(&text));
    assert!(html.contains("<h1>Über mich</h1>"), "{html}");
    assert!(html.contains("<strong>Netze</strong>"), "{html}");
    // An about page is not a dated entry.
    assert!(!html.contains("2026-01-01"), "no publication date: {html}");
    assert!(!html.contains("class=\"date\""), "{html}");

    let micron = render_about_micron(&meta_with_about(), Some(&text));
    assert!(micron.contains("Ich baue"), "{micron}");
    assert!(!micron.contains("2026-01-01"), "{micron}");
}

#[test]
fn the_about_page_is_served_over_micron_but_is_not_a_post() {
    let dir = tempfile::tempdir().expect("posts dir");
    std::fs::write(dir.path().join("erster.md"), "Erster Text.\n").expect("write post");
    let about = dir.path().join("..").join("about.md");
    std::fs::write(&about, "+++\ntitle = \"Über mich\"\n+++\n\nText.\n").expect("write about");

    let snapshot = load_snapshot(
        &meta_with_about(),
        &Sources::new(dir.path()).with_about(Some(&about)),
    )
    .expect("load snapshot");

    assert!(
        snapshot.served_paths().contains(&ABOUT_PATH.to_string()),
        "{:?}",
        snapshot.served_paths()
    );
    // It is not a post: not in the list, and so not in the index or the feed.
    assert_eq!(snapshot.posts.len(), 1, "the about text is not a post");
    assert_eq!(snapshot.posts[0].title, "erster");
    assert!(snapshot.about.is_some());

    let index = render_index_micron(&snapshot.meta, &snapshot.posts);
    assert!(!index.contains("Über mich"), "not listed: {index}");
}

#[test]
fn contact_details_alone_are_enough_for_an_about_page() {
    // No text file: the page still exists and carries the addresses.
    let dir = tempfile::tempdir().expect("posts dir");
    std::fs::write(dir.path().join("erster.md"), "Text.\n").expect("write post");
    let snapshot = load_snapshot(&meta_with_about(), &Sources::new(dir.path())).expect("load");

    assert!(snapshot.about.is_none());
    assert!(snapshot.served_paths().contains(&ABOUT_PATH.to_string()));
}

#[test]
fn no_about_page_means_no_micron_page_for_it() {
    let dir = tempfile::tempdir().expect("posts dir");
    std::fs::write(dir.path().join("erster.md"), "Text.\n").expect("write post");
    let snapshot = load_snapshot(&meta_without_about(), &Sources::new(dir.path())).expect("load");
    assert!(!snapshot.served_paths().contains(&ABOUT_PATH.to_string()));
}
