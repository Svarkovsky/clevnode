//! The Atom feed: structure, absolute links, and the cases where there is
//! none to serve.

use lblogd::post::{Date, Post};
use lblogd::render::{render_feed_atom, render_index_html, BlogMeta, DEFAULT_STYLE};

fn post(title: &str, date: &str, body: &str, author: Option<&str>) -> Post {
    Post {
        title: title.to_string(),
        date: date.parse::<Date>().expect("parse fixture date"),
        author: author.map(str::to_string),
        slug: lblogd::post::slugify(title),
        body_md: body.to_string(),
    }
}

fn meta() -> BlogMeta {
    BlogMeta {
        title: "leviculum.network".to_string(),
        author: Some("Lew Palm".to_string()),
        description: Some("Notes on Reticulum".to_string()),
        language: "en".to_string(),
        web_url: Some("https://leviculum.network".to_string()),
        nomadnet_address: Some("0ec84236630cea839d80a71c39fb41ce".to_string()),
        email: None,
        lxmf: None,
        has_about: false,
    }
}

/// Every element between `<tag>` and `</tag>`, in order.
fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    xml.match_indices(&open)
        .filter_map(|(start, _)| {
            let rest = &xml[start + open.len()..];
            rest.find(&close).map(|end| &rest[..end])
        })
        .collect()
}

#[test]
fn the_feed_describes_the_blog_and_its_posts() {
    let posts = vec![
        post("Newer", "2026-07-12", "Body.\n", None),
        post("Older", "2026-07-01", "Body.\n", None),
    ];
    let xml = render_feed_atom(&meta(), &posts).expect("a feed with a web url");

    assert!(
        xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"),
        "{xml}"
    );
    assert!(
        xml.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"),
        "{xml}"
    );
    assert_eq!(
        elements(&xml, "title"),
        ["leviculum.network", "Newer", "Older"]
    );
    assert!(
        xml.contains("<subtitle>Notes on Reticulum</subtitle>"),
        "{xml}"
    );
    assert!(xml.contains("<name>Lew Palm</name>"), "{xml}");

    // The feed's own timestamp is the newest post's.
    assert!(
        xml.contains("<updated>2026-07-12T00:00:00Z</updated>"),
        "{xml}"
    );

    // Entry identity and link are the absolute post URL.
    assert!(
        xml.contains("<id>https://leviculum.network/posts/newer</id>"),
        "{xml}"
    );
    assert!(
        xml.contains("href=\"https://leviculum.network/posts/newer\""),
        "{xml}"
    );
    assert!(
        xml.contains("<published>2026-07-01T00:00:00Z</published>"),
        "{xml}"
    );
}

#[test]
fn entries_carry_the_full_text() {
    let posts = vec![post("Only", "2026-07-12", "A **bold** claim.\n", None)];
    let xml = render_feed_atom(&meta(), &posts).expect("feed");
    // type="html" means the markup is escaped inside the element.
    assert!(xml.contains("<content type=\"html\">"), "{xml}");
    assert!(
        xml.contains("&lt;strong&gt;bold&lt;/strong&gt;"),
        "the body must be carried, escaped: {xml}"
    );
}

#[test]
fn relative_links_are_made_absolute() {
    // In a reader, a relative link resolves against the reader's own address.
    let body = "\
[root](/about) and [sibling](other) and [anchor](#part) and \
[external](https://example.com/x) and [mail](mailto:a@b.example)\n";
    let posts = vec![post("Links", "2026-07-12", body, None)];
    let xml = render_feed_atom(&meta(), &posts).expect("feed");

    assert!(
        xml.contains("href=&quot;https://leviculum.network/about&quot;"),
        "root-relative: {xml}"
    );
    assert!(
        xml.contains("href=&quot;https://leviculum.network/posts/other&quot;"),
        "document-relative resolves against the post's directory: {xml}"
    );
    assert!(
        xml.contains("href=&quot;https://leviculum.network/posts/links#part&quot;"),
        "a fragment must point back into the post: {xml}"
    );
    assert!(
        xml.contains("href=&quot;https://example.com/x&quot;"),
        "an absolute URL is left alone: {xml}"
    );
    assert!(
        xml.contains("href=&quot;mailto:a@b.example&quot;"),
        "a non-http scheme is left alone: {xml}"
    );
}

#[test]
fn images_are_absolutised_too() {
    let posts = vec![post("Pic", "2026-07-12", "![alt](/img/rig.png)\n", None)];
    let xml = render_feed_atom(&meta(), &posts).expect("feed");
    assert!(
        xml.contains("src=&quot;https://leviculum.network/img/rig.png&quot;"),
        "{xml}"
    );
}

#[test]
fn a_file_area_image_resolves_to_the_web_route_before_being_absolutised() {
    // Resolving first is what keeps the feed honest: a bare `antenne.jpg`
    // absolutised on its own would point at /posts/pic/antenne.jpg, where
    // nothing is served.
    let posts = vec![post("Pic", "2026-07-12", "![alt](antenne.jpg)\n", None)];
    let xml = render_feed_atom(&meta(), &posts).expect("feed");
    assert!(
        xml.contains("src=&quot;https://leviculum.network/files/antenne.jpg&quot;"),
        "{xml}"
    );
}

#[test]
fn a_guest_author_overrides_the_feed_author() {
    let posts = vec![
        post("Mine", "2026-07-12", "Body.\n", None),
        post("Theirs", "2026-07-11", "Body.\n", Some("Someone Else")),
    ];
    let xml = render_feed_atom(&meta(), &posts).expect("feed");
    // Atom entries inherit the feed author, so only the guest needs naming.
    assert_eq!(xml.matches("<name>Lew Palm</name>").count(), 1, "{xml}");
    assert_eq!(xml.matches("<name>Someone Else</name>").count(), 1, "{xml}");
}

#[test]
fn markup_in_titles_is_escaped() {
    let mut meta = meta();
    meta.title = "Tom & Jerry <live>".to_string();
    let posts = vec![post("A <b>title</b>", "2026-07-12", "Body.\n", None)];
    let xml = render_feed_atom(&meta, &posts).expect("feed");
    assert!(xml.contains("Tom &amp; Jerry &lt;live&gt;"), "{xml}");
    assert!(xml.contains("A &lt;b&gt;title&lt;/b&gt;"), "{xml}");
    // Nothing may have escaped as live markup.
    assert!(!xml.contains("<b>"), "{xml}");
}

#[test]
fn an_empty_blog_still_produces_a_valid_feed() {
    let xml = render_feed_atom(&meta(), &[]).expect("feed");
    assert!(xml.contains("<updated>"), "updated is mandatory: {xml}");
    assert!(!xml.contains("<entry>"), "{xml}");
    assert!(xml.ends_with("</feed>\n"), "{xml}");
}

#[test]
fn without_a_web_url_there_is_no_feed() {
    // A plaintext development run: no domain, so no absolute links, so
    // nothing worth serving.
    let meta = BlogMeta {
        web_url: None,
        ..meta()
    };
    assert!(render_feed_atom(&meta, &[post("Only", "2026-07-12", "B.\n", None)]).is_none());
}

#[test]
fn pages_advertise_the_feed_only_when_there_is_one() {
    let posts = [post("Only", "2026-07-12", "Body.\n", None)];

    let html = render_index_html(&meta(), DEFAULT_STYLE, &posts);
    assert!(
        html.contains(
            "<link rel=\"alternate\" type=\"application/atom+xml\" \
             title=\"leviculum.network\" href=\"/feed.xml\">"
        ),
        "autodiscovery is how a reader finds the feed: {html}"
    );

    let meta = BlogMeta {
        web_url: None,
        ..meta()
    };
    let html = render_index_html(&meta, DEFAULT_STYLE, &posts);
    assert!(
        !html.contains("atom+xml"),
        "no feed, no advertisement: {html}"
    );
}
