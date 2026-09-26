//! Renderers: Markdown to HTML and Markdown to Micron, plus the complete
//! index/post page templates for both output formats.
//!
//! The HTML side is a thin wrapper over pulldown-cmark's HTML renderer wrapped
//! in a minimal, theme-neutral document template with a tiny inline stylesheet.
//!
//! The Micron side walks the pulldown-cmark event stream and emits micron
//! markup as defined by the `leviculum-micron` parser (the authority on what
//! valid micron is). The mapping:
//!
//! | Markdown                | Micron                                        |
//! |-------------------------|-----------------------------------------------|
//! | heading level 1/2/3     | `>` / `>>` / `>>>` (deeper clamped to `>>>`)  |
//! | `**bold**`              | `` `! `` toggles                              |
//! | `*italic*`              | `` `* `` toggles                              |
//! | `` `inline code` ``     | `` `B333 `` background toggle (see below)     |
//! | fenced/indented code    | `` `= `` literal block                        |
//! | `[text](url)`           | `` `[text`url] ``                             |
//! | bullet list             | `\u{2022} item` lines, nested lists indented  |
//! | numbered list           | `1. item` lines, nested lists indented        |
//! | `---` rule              | `-` divider line                              |
//! | paragraph break         | blank line                                    |
//! | hard break              | line break                                    |
//!
//! Degradations (micron has no equivalent; never panics):
//!
//! - inline code: micron has no inline literal, so code is set off with a
//!   `` `B333 `` background colour toggle (a dark neutral that reads on the
//!   dark NomadNet default theme) and closed with `` `b ``
//! - images: an image naming a file in the file area becomes a link to it
//!   (`` `[alt`:/file/name] ``), which is the only form micron has; any other
//!   image (an external URL) stays `[image: alt]` plain text
//! - tables: emitted as a micron `` `t `` table — a header row, the alignment
//!   row micron reads as the table's second line, then the data rows, cells
//!   separated by `|` with any literal pipe escaped
//! - blockquotes: two-space indented text per nesting level
//! - raw HTML: emitted as escaped plain text
//! - strikethrough/task lists/definition lists: extensions not enabled, so
//!   their syntax passes through as plain text
//! - footnotes: the extension is not enabled, and the syntax does NOT
//!   reliably degrade to plain text. A reference `[^1]` stays literal, but a
//!   definition line whose body is a bare URL or a single word (`[^1]: Kurz`)
//!   parses as a CommonMark link-reference definition instead: the definition
//!   line disappears from the output entirely and the reference in the body
//!   turns into a link labelled `^1`. Until #197 closes the gap, footnotes are
//!   lossy on both sides.
//!
//! Plain text is escaped so it can never be misread as micron markup:
//! backslashes and backticks are `\`-escaped inline, and a text line that
//! would start with a line-level control character (`>`, `#`, `-`, `<`) gets
//! a leading `\` line escape.

use pulldown_cmark::{html, Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::files;
use crate::post::{slugify, Date, Post};

/// Micron heading depth is meaningful for 1-3 `>`; deeper Markdown headings
/// clamp here.
const MAX_MICRON_HEADING_DEPTH: usize = 3;

/// What a reader learns about the blog itself, independent of any one post.
///
/// Assembled once from the config and the resolved destination, then rendered
/// into every page on both sides. Optional fields are simply omitted when
/// absent rather than rendered empty, so a minimal configuration produces a
/// clean page rather than a page with blanks in it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlogMeta {
    /// The blog's name; the heading of every page.
    pub title: String,
    /// Who writes it, shown unless a post names its own author.
    pub author: Option<String>,
    /// One sentence on what this is.
    pub description: Option<String>,
    /// BCP 47 language tag for the HTML `lang` attribute.
    pub language: String,
    /// The blog's clearnet URL, shown on the NomadNet side so mesh readers
    /// can find the web version.
    pub web_url: Option<String>,
    /// The blog's NomadNet destination hash, shown on the web side so
    /// clearnet readers can find the mesh version.
    pub nomadnet_address: Option<String>,
    /// Contact address for the about page.
    pub email: Option<String>,
    /// LXMF destination hash for the about page, 32 hex characters.
    pub lxmf: Option<String>,
    /// Whether an about page exists, and therefore whether the author's name
    /// is a link.
    ///
    /// It exists as soon as there is anything to put on it: an address, a
    /// hash, or a text file. Without any of those a link would lead to an
    /// empty page, so the name stays plain text.
    pub has_about: bool,
}

impl BlogMeta {
    /// The author to credit for `post`: its own, else the blog's.
    fn author_of<'a>(&'a self, post: &'a Post) -> Option<&'a str> {
        post.author.as_deref().or(self.author.as_deref())
    }

    /// The blog author's name as HTML, linked to the about page when there is
    /// one.
    ///
    /// Only the blog's own author is linked. A guest author's name pointing
    /// at the blog author's about page would simply be wrong, and a page per
    /// author is more machinery than a blog with one writer needs.
    fn author_html(&self, name: &str) -> String {
        let escaped = escape_html(name);
        match self.has_about && Some(name) == self.author.as_deref() {
            true => format!("<a href=\"{ABOUT_HTML_PATH}\">{escaped}</a>"),
            false => escaped,
        }
    }

    /// The same for micron, linking to the local about page.
    fn author_micron(&self, name: &str) -> String {
        match self.has_about && Some(name) == self.author.as_deref() {
            true => format!("`[{}`{ABOUT_MICRON_PATH}]", sanitize_link_part(name)),
            false => escape_micron_text(name),
        }
    }
}

/// The HTTP path of the about page.
pub const ABOUT_HTML_PATH: &str = "/about";

/// The micron request path of the about page.
pub const ABOUT_MICRON_PATH: &str = ":/page/about.mu";

/// The micron background colour used to set off inline code (12-bit form).
const INLINE_CODE_BG: &str = "333";

/// The pulldown-cmark options used by both renderers. Tables are the only
/// extension: everything else degrades better as plain text.
fn markdown_options() -> Options {
    Options::ENABLE_TABLES
}

/// Render a Markdown fragment to an HTML fragment (no surrounding document).
pub fn markdown_to_html(md: &str) -> String {
    let parser = Parser::new_ext(md, markdown_options())
        .map(demote_heading)
        .map(resolve_file_ref);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// Point an image or link at the file area's web route when it names a file
/// there.
///
/// An author writes `![Antenne](antenne.jpg)` and means "the picture next to
/// my posts". On the web that is `/files/antenne.jpg`; on the mesh the same
/// reference becomes a `:/file/antenne.jpg` link (see
/// [`MicronWriter::end`]). Anything [`files::file_ref`] does not recognise —
/// an external URL above all — is left exactly as written.
fn resolve_file_ref(event: Event<'_>) -> Event<'_> {
    match event {
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: match files::file_ref(&dest_url) {
                Some(name) => files::web_path(&name).into(),
                None => dest_url,
            },
            title,
            id,
        }),
        other => other,
    }
}

/// Push a Markdown heading down one level.
///
/// The page template already gives the post its `<h1>`, so a `# Heading` in
/// the body would produce a second one and leave the document with two
/// competing top-level headings. Demoting means an author can write `#` for
/// their first section, as Markdown habit dictates, and still get a correctly
/// nested document.
///
/// Both the start and the end event carry the level, and moving only one of
/// them emits mismatched tags like `<h2>Text</h1>`.
fn demote_heading(event: Event<'_>) -> Event<'_> {
    match event {
        Event::Start(Tag::Heading {
            level,
            id,
            classes,
            attrs,
        }) => Event::Start(Tag::Heading {
            level: one_level_down(level),
            id,
            classes,
            attrs,
        }),
        Event::End(TagEnd::Heading(level)) => Event::End(TagEnd::Heading(one_level_down(level))),
        other => other,
    }
}

/// Render a post body to HTML with every link and image made absolute.
///
/// A feed entry is read somewhere else entirely, so a relative link in it
/// resolves against the reader's own address and lands nowhere. `base` is the
/// blog's root and `page` the post's own URL, which is what a document-
/// relative reference resolves against.
fn markdown_to_html_absolute(md: &str, base: &str, page: &str) -> String {
    let parser = Parser::new_ext(md, markdown_options())
        .map(demote_heading)
        // File references resolve to the web route BEFORE absolutising, so a
        // feed entry's picture points at `<base>/files/x.jpg` rather than at
        // a name resolved against the post's own URL, where nothing is.
        .map(resolve_file_ref)
        .map(|event| absolutize(event, base, page));
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// Rewrite the destination of a link or image event to an absolute URL.
fn absolutize<'a>(event: Event<'a>, base: &str, page: &str) -> Event<'a> {
    match event {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: absolute_url(&dest_url, base, page).into(),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: absolute_url(&dest_url, base, page).into(),
            title,
            id,
        }),
        other => other,
    }
}

/// Resolve one reference against the blog root and the containing page.
///
/// Only the forms that occur in a post are handled, deliberately rather than
/// implementing RFC 3986: anything already carrying a scheme (`https:`,
/// `mailto:`) or a network path (`//host/x`) is left alone, a root-relative
/// path resolves against the blog root, a fragment against the page it sits
/// in, and anything else against the page's directory.
fn absolute_url(url: &str, base: &str, page: &str) -> String {
    if url.is_empty() || has_scheme(url) || url.starts_with("//") {
        return url.to_string();
    }
    if let Some(fragment) = url.strip_prefix('#') {
        // An in-page anchor would otherwise jump inside the reader's own page.
        return format!("{page}#{fragment}");
    }
    if let Some(path) = url.strip_prefix('/') {
        return format!("{base}/{path}");
    }
    // Document-relative: resolve against the directory the page sits in.
    let dir = page.rsplit_once('/').map(|(d, _)| d).unwrap_or(page);
    format!("{dir}/{url}")
}

/// Whether a reference starts with a URL scheme, e.g. `https:` or `mailto:`.
///
/// A scheme is a letter followed by letters, digits, `+`, `-` or `.`, then a
/// colon. Checking the shape rather than a list of known schemes avoids
/// mangling anything exotic an author writes on purpose.
fn has_scheme(url: &str) -> bool {
    let Some((prefix, _)) = url.split_once(':') else {
        return false;
    };
    let mut chars = prefix.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The next heading level down; `h6` has nowhere to go and stays put.
fn one_level_down(level: pulldown_cmark::HeadingLevel) -> pulldown_cmark::HeadingLevel {
    use pulldown_cmark::HeadingLevel::*;
    match level {
        H1 => H2,
        H2 => H3,
        H3 => H4,
        H4 => H5,
        H5 | H6 => H6,
    }
}

/// Escape text for inclusion in HTML element content or attribute values.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The built-in stylesheet, used when the operator configures none. Minimal,
/// theme-neutral and readable.
pub const DEFAULT_STYLE: &str = "\
body{margin:0 auto;max-width:42rem;padding:1rem;font-family:system-ui,sans-serif;\
line-height:1.6;color:#222;background:#fdfdfd}\
h1,h2,h3{line-height:1.25}\
code,pre{font-family:ui-monospace,monospace;background:#eee}\
pre{padding:.75rem;overflow-x:auto}\
a{color:#1a5fb4}\
.tagline{color:#444}\
.byline,.date{color:#666;font-size:.9rem}\
ul.posts{list-style:none;padding:0}\
ul.posts li{margin:.5rem 0}\
footer{margin-top:3rem;border-top:1px solid #ddd;padding-top:1rem;\
color:#666;font-size:.9rem}\
footer code{background:none}";

/// Wrap `body` in a complete HTML document.
///
/// `title` is the browser-tab title, which is the post title on a post page
/// and the blog title on the index; `css` is inlined rather than linked so a
/// page is always styled by the stylesheet it was rendered with.
fn html_document(meta: &BlogMeta, css: &str, title: &str, body: &str) -> String {
    let description = match &meta.description {
        Some(d) => format!(
            "<meta name=\"description\" content=\"{}\">\n",
            escape_html(d)
        ),
        None => String::new(),
    };
    // Feed autodiscovery, so a reader finds the feed from any page. Only
    // emitted when there is a feed to find, which mirrors the route.
    let feed = match meta.web_url {
        Some(_) => format!(
            "<link rel=\"alternate\" type=\"application/atom+xml\" \
             title=\"{}\" href=\"{FEED_PATH}\">\n",
            escape_html(&meta.title)
        ),
        None => String::new(),
    };
    format!(
        "<!doctype html>\n<html lang=\"{}\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         {}{}<title>{}</title>\n<style>{}</style>\n</head>\n<body>\n{}\n</body>\n</html>\n",
        escape_html(&meta.language),
        description,
        feed,
        escape_html(title),
        css,
        body
    )
}

/// The footer shown on every HTML page: where to find the blog on the mesh.
///
/// A reader on the clearnet side has no way to discover the NomadNet
/// destination otherwise, and it is the more interesting half of this blog.
fn html_footer(meta: &BlogMeta) -> String {
    match &meta.nomadnet_address {
        Some(address) => format!(
            "\n<footer>\nAlso on NomadNet over Reticulum: <code>{}</code>\n</footer>",
            escape_html(address)
        ),
        None => String::new(),
    }
}

/// Render the post index as a complete HTML document: who this is, what it
/// is about, and the posts. Posts link to `/posts/<slug>`.
pub fn render_index_html(meta: &BlogMeta, css: &str, posts: &[Post]) -> String {
    let mut body = format!("<h1>{}</h1>\n", escape_html(&meta.title));
    if let Some(author) = &meta.author {
        body.push_str(&format!(
            "<p class=\"byline\">by {}</p>\n",
            meta.author_html(author)
        ));
    }
    if let Some(description) = &meta.description {
        body.push_str(&format!(
            "<p class=\"tagline\">{}</p>\n",
            escape_html(description)
        ));
    }

    body.push_str("<ul class=\"posts\">\n");
    for post in posts {
        // Only name an author who differs from the blog's: repeating the same
        // name on every line is noise, a guest post is information.
        let byline = match &post.author {
            Some(author) if Some(author.as_str()) != meta.author.as_deref() => {
                format!(
                    " <span class=\"byline\">by {}</span>",
                    meta.author_html(author)
                )
            }
            _ => String::new(),
        };
        body.push_str(&format!(
            "<li><span class=\"date\">{}</span> <a href=\"/posts/{}\">{}</a>{}</li>\n",
            post.date,
            escape_html(&post.slug),
            escape_html(&post.title),
            byline
        ));
    }
    body.push_str("</ul>");
    body.push_str(&html_footer(meta));
    html_document(meta, css, &meta.title, &body)
}

/// Render one post as a complete HTML document, with a way back to the index.
pub fn render_post_html(meta: &BlogMeta, css: &str, post: &Post) -> String {
    let byline = match meta.author_of(post) {
        Some(author) => format!(" &middot; {}", meta.author_html(author)),
        None => String::new(),
    };
    let body = format!(
        "<article>\n<h1>{}</h1>\n<p class=\"date\">{}{}</p>\n{}</article>\n\
         <p><a href=\"/\">&larr; {}</a></p>{}",
        escape_html(&post.title),
        post.date,
        byline,
        markdown_to_html(&post.body_md),
        escape_html(&meta.title),
        html_footer(meta)
    );
    html_document(meta, css, &post.title, &body)
}

/// Render the about page as a complete HTML document.
///
/// `text` is the optional Markdown file, parsed exactly like a post. Its
/// date, slug and author are ignored: an about page is not a dated entry, so
/// showing a publication date and a byline on it would be misleading.
pub fn render_about_html(meta: &BlogMeta, css: &str, text: Option<&Post>) -> String {
    let heading = about_heading(meta, text);
    let mut body = format!("<h1>{}</h1>\n", escape_html(&heading));

    if let Some(email) = &meta.email {
        body.push_str(&format!(
            "<p class=\"contact\">Email: <a href=\"mailto:{0}\">{0}</a></p>\n",
            escape_html(email)
        ));
    }
    if let Some(lxmf) = &meta.lxmf {
        // No link: a browser has nothing to do with an LXMF address. The hash
        // is what a reader copies into their own client.
        body.push_str(&format!(
            "<p class=\"contact\">LXMF: <code>{}</code></p>\n",
            escape_html(lxmf)
        ));
    }
    if let Some(text) = text {
        body.push_str(&markdown_to_html(&text.body_md));
    }

    body.push_str(&format!(
        "<p><a href=\"/\">&larr; {}</a></p>",
        escape_html(&meta.title)
    ));
    body.push_str(&html_footer(meta));
    html_document(meta, css, &heading, &body)
}

/// Render the about page as a micron page.
///
/// The LXMF address becomes a `lxmf@<hash>` link, which NomadNet opens as a
/// conversation with that address.
pub fn render_about_micron(meta: &BlogMeta, text: Option<&Post>) -> String {
    let heading = about_heading(meta, text);
    let mut out = format!(">{}\n\n", escape_micron_text(&heading));

    if let Some(email) = &meta.email {
        out.push_str(&format!("Email: {}\n", escape_micron_text(email)));
    }
    if let Some(lxmf) = &meta.lxmf {
        out.push_str(&format!(
            "LXMF: `[{0}`lxmf@{0}]\n",
            sanitize_link_part(lxmf)
        ));
    }
    if meta.email.is_some() || meta.lxmf.is_some() {
        out.push_str("\n-\n\n");
    }
    if let Some(text) = text {
        out.push_str(&markdown_to_micron(&text.body_md));
        out.push('\n');
    }

    out.push_str(&format!(
        "\n`[\u{2190} {}`{ABOUT_BACK_PATH}]\n",
        sanitize_link_part(&meta.title)
    ));
    out.push_str(&micron_footer(meta));
    out
}

/// The micron request path of the index, used by the about page's back link.
const ABOUT_BACK_PATH: &str = ":/page/index.mu";

/// The about page's heading.
///
/// The text file's title when there is one, which the loader already defaults
/// to [`default_about_title`], so a file without frontmatter still lands on
/// the author's name rather than on its own file name.
fn about_heading(meta: &BlogMeta, text: Option<&Post>) -> String {
    match text {
        Some(text) => text.title.clone(),
        None => default_about_title(meta.author.as_deref()),
    }
}

/// The title an about page carries when nothing names one: the author, or a
/// plain "About".
///
/// A name that slugifies to nothing is skipped, because the post parser
/// requires a usable slug and would otherwise reject the file over a title it
/// never asked for.
pub fn default_about_title(author: Option<&str>) -> String {
    author
        .filter(|a| !slugify(a).is_empty())
        .unwrap_or("About")
        .to_string()
}

/// The path the Atom feed is served under.
pub const FEED_PATH: &str = "/feed.xml";

/// Render the Atom feed, or `None` when the blog has no public URL.
///
/// A feed is only meaningful with absolute links, and those need a domain.
/// A plaintext development run has none, so it serves no feed rather than a
/// feed full of links that resolve against whatever the reader happens to be
/// looking at.
///
/// Atom rather than RSS 2.0: entry identity is explicit rather than
/// conventional, and timestamps are RFC 3339 rather than RFC 822. Every
/// reader handles both.
pub fn render_feed_atom(meta: &BlogMeta, posts: &[Post]) -> Option<String> {
    let base = meta.web_url.as_deref()?;

    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\">\n");
    out.push_str(&format!("<title>{}</title>\n", escape_html(&meta.title)));
    if let Some(description) = &meta.description {
        out.push_str(&format!(
            "<subtitle>{}</subtitle>\n",
            escape_html(description)
        ));
    }
    out.push_str(&format!("<id>{}/</id>\n", escape_html(base)));
    out.push_str(&format!(
        "<link rel=\"alternate\" type=\"text/html\" href=\"{}/\"/>\n",
        escape_html(base)
    ));
    out.push_str(&format!(
        "<link rel=\"self\" type=\"application/atom+xml\" href=\"{}{}\"/>\n",
        escape_html(base),
        FEED_PATH
    ));
    // Posts are newest first, so the first one dates the feed. With no posts
    // there is no date to give and the epoch stands in; `updated` is
    // mandatory, and an empty feed is not worth a special case.
    let updated = posts
        .first()
        .map(|p| rfc3339(&p.date))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    out.push_str(&format!("<updated>{updated}</updated>\n"));
    if let Some(author) = &meta.author {
        out.push_str(&format!(
            "<author><name>{}</name></author>\n",
            escape_html(author)
        ));
    }

    for post in posts {
        out.push_str(&feed_entry(meta, base, post));
    }
    out.push_str("</feed>\n");
    Some(out)
}

/// One `<entry>`, carrying the post's full text.
///
/// Full text rather than a teaser: this is a text blog, and a feed a reader
/// can actually read in their reader is the point of having one.
fn feed_entry(meta: &BlogMeta, base: &str, post: &Post) -> String {
    let url = format!("{base}/posts/{}", post.slug);
    let mut entry = String::from("<entry>\n");
    entry.push_str(&format!("<title>{}</title>\n", escape_html(&post.title)));
    // The entry id has to stay stable, or readers show the post again as new.
    // It is the post URL, which means it moves when an untitled-slug post is
    // retitled; see the README note on pinning `slug` once published.
    entry.push_str(&format!("<id>{}</id>\n", escape_html(&url)));
    entry.push_str(&format!(
        "<link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>\n",
        escape_html(&url)
    ));
    let stamp = rfc3339(&post.date);
    entry.push_str(&format!("<published>{stamp}</published>\n"));
    entry.push_str(&format!("<updated>{stamp}</updated>\n"));
    // Atom lets entries inherit the feed's author, so only a differing one
    // needs saying. With no feed author, the post's own is all there is.
    if let Some(author) = meta.author_of(post) {
        if Some(author) != meta.author.as_deref() {
            entry.push_str(&format!(
                "<author><name>{}</name></author>\n",
                escape_html(author)
            ));
        }
    }
    entry.push_str(&format!(
        "<content type=\"html\">{}</content>\n",
        escape_html(&markdown_to_html_absolute(&post.body_md, base, &url))
    ));
    entry.push_str("</entry>\n");
    entry
}

/// A post's date as an RFC 3339 timestamp.
///
/// Posts are dated to the day, so the time is always midnight UTC. Two posts
/// on one day therefore carry identical timestamps and a reader may order
/// them either way; our own index breaks that tie by title, a reader cannot.
fn rfc3339(date: &Date) -> String {
    format!("{date}T00:00:00Z")
}

/// Convert a Markdown fragment to valid micron markup. See the module docs
/// for the mapping and degradation table. Never panics.
pub fn markdown_to_micron(md: &str) -> String {
    let parser = Parser::new_ext(md, markdown_options());
    let mut writer = MicronWriter::default();
    for event in parser {
        writer.event(event);
    }
    writer.finish()
}

/// Render the post index as a micron page: the blog's identity, then one link
/// per post targeting the local page `:/page/<slug>.mu` (NomadNet's same-node
/// link form, as resolved by lnomad and NomadNet).
pub fn render_index_micron(meta: &BlogMeta, posts: &[Post]) -> String {
    let mut out = format!(">{}\n\n", escape_micron_text(&meta.title));
    if let Some(author) = &meta.author {
        out.push_str(&format!("by {}\n", meta.author_micron(author)));
    }
    if let Some(description) = &meta.description {
        out.push_str(&format!("{}\n", escape_micron_text(description)));
    }
    if meta.author.is_some() || meta.description.is_some() {
        out.push('\n');
    }

    for post in posts {
        // Same rule as HTML: name an author only where it differs.
        let byline = match &post.author {
            Some(author) if Some(author.as_str()) != meta.author.as_deref() => {
                format!(" by {author}")
            }
            _ => String::new(),
        };
        out.push_str(&format!(
            "`[{}`:/page/{}.mu]\n",
            sanitize_link_part(&format!("{} {}{}", post.date, post.title, byline)),
            sanitize_link_part(&post.slug)
        ));
    }
    out.push_str(&micron_footer(meta));
    out
}

/// Render one post as a micron page: title heading, date and author line,
/// divider, body, and a link back to the index.
pub fn render_post_micron(meta: &BlogMeta, post: &Post) -> String {
    let byline = match meta.author_of(post) {
        Some(author) => format!(" \u{b7} {}", meta.author_micron(author)),
        None => String::new(),
    };
    format!(
        ">{}\n\n{}{}\n-\n\n{}\n\n`[\u{2190} {}`:/page/index.mu]\n{}",
        escape_micron_text(&post.title),
        post.date,
        byline,
        markdown_to_micron(&post.body_md),
        sanitize_link_part(&meta.title),
        micron_footer(meta)
    )
}

/// The footer shown on every micron page: where to find the blog on the web.
///
/// The mirror of [`html_footer`]; a mesh reader who wants to share the blog
/// with someone off-mesh needs the clearnet URL.
fn micron_footer(meta: &BlogMeta) -> String {
    match &meta.web_url {
        Some(url) => format!("\n-\n\nAlso on the web: {}\n", escape_micron_text(url)),
        None => String::new(),
    }
}

/// Escape plain text so micron's inline parser reads it verbatim: `\` and
/// `` ` `` are `\`-escaped.
fn escape_micron_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || c == '`' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Sanitize text for use inside a micron `` `[label`target] `` link, whose
/// contents run raw to the closing bracket: backticks and `]` would end the
/// label/target early, so they degrade to close lookalikes.
fn sanitize_link_part(s: &str) -> String {
    s.replace('`', "'").replace(']', ")")
}

/// Line-level micron control characters: a plain-text line must not start
/// with one of these, or it would parse as a heading (`>`), comment (`#`),
/// divider (`-`) or depth reset (`<`). `` ` `` needs no entry because inline
/// escaping already turns it into `` \` ``.
const LINE_CONTROL_CHARS: [char; 4] = ['>', '#', '-', '<'];

/// The table currently being emitted.
#[derive(Clone, Debug, Default)]
struct OpenTable {
    /// The column alignments, for the separator row micron wants as the
    /// table's second line.
    alignments: Vec<Alignment>,
    /// Cells emitted so far in the current row.
    cells: usize,
}

/// The image currently being collected: its alt text, and where it points.
#[derive(Clone, Debug, Default)]
struct OpenImage {
    /// Alt text buffered until the closing event.
    alt: String,
    /// The destination as the author wrote it.
    dest: String,
}

/// The streaming Markdown-event-to-micron writer.
#[derive(Default)]
struct MicronWriter {
    /// Finished output lines.
    out: Vec<String>,
    /// The line being built.
    line: String,
    /// Whether `line` began with plain text (needs the line-start escape
    /// check at flush) rather than with markup we emitted deliberately.
    line_is_text: bool,
    /// Blockquote nesting depth; each level indents flushed lines two spaces.
    quote_depth: usize,
    /// Open lists: `None` for a bullet list, `Some(next_index)` for numbered.
    list_stack: Vec<Option<u64>>,
    /// Inside a `` `= `` literal block: lines pass through verbatim.
    in_code_block: bool,
    /// Target URL of the link currently open, if any.
    link_url: Option<String>,
    /// Label text buffered while a link is open.
    link_label: String,
    /// Alt text buffered while an image is open, with the image's target.
    image: Option<OpenImage>,
    /// The table currently being emitted, if any.
    table: Option<OpenTable>,
    /// The cell currently being collected. Holds finished output fragments,
    /// not source text, so a link or a style toggle inside a cell survives;
    /// only plain text picks up the extra `|` escaping on the way in.
    cell: Option<String>,
}

impl MicronWriter {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => self.inline_code(&t),
            Event::Html(t) | Event::InlineHtml(t) => self.text(&t),
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.block_sep();
                self.push_raw("-");
                self.flush_line();
            }
            // Extensions we do not enable; listed for totality, never emitted.
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            // A paragraph opening right after a list-item marker (or a table
            // cell) continues that line; otherwise it starts a fresh,
            // blank-separated block.
            Tag::Paragraph if self.line.is_empty() => self.block_sep(),
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.block_sep();
                // Demoted for the same reason as on the HTML side: the page
                // template already used `>` for the post title, so a body
                // heading starts one level in. Micron has only three levels,
                // so this also means `>` is reserved for the title alone.
                let depth = (level as usize + 1).min(MAX_MICRON_HEADING_DEPTH);
                self.push_raw(&">".repeat(depth));
            }
            Tag::BlockQuote(_) => {
                self.block_sep();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(CodeBlockKind::Fenced(_) | CodeBlockKind::Indented) => {
                self.block_sep();
                self.out.push("`=".to_string());
                self.in_code_block = true;
            }
            Tag::List(start) => {
                if self.list_stack.is_empty() {
                    self.block_sep();
                } else {
                    // A nested list starts inside its parent item's line.
                    self.flush_line();
                }
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush_line();
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "\u{2022} ".to_string(),
                };
                self.push_raw(&format!("{indent}{marker}"));
            }
            Tag::Table(alignments) => {
                self.block_sep();
                self.out.push("`t".to_string());
                self.table = Some(OpenTable {
                    alignments,
                    cells: 0,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                self.flush_line();
                if let Some(table) = self.table.as_mut() {
                    table.cells = 0;
                }
            }
            Tag::TableCell => {
                let first = self.table.as_ref().is_none_or(|t| t.cells == 0);
                if !first {
                    self.push_raw(" | ");
                }
                if let Some(table) = self.table.as_mut() {
                    table.cells += 1;
                }
                // Collect the cell rather than writing straight to the line:
                // a literal `|` in its text has to be escaped, and that can
                // only be decided per cell.
                self.cell = Some(String::new());
            }
            Tag::Emphasis => self.style_toggle("`*"),
            Tag::Strong => self.style_toggle("`!"),
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.to_string());
                self.link_label.clear();
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some(OpenImage {
                    alt: String::new(),
                    dest: dest_url.to_string(),
                })
            }
            Tag::HtmlBlock => self.block_sep(),
            // Extensions we do not enable (footnotes, definition lists,
            // strikethrough, sub/superscript, metadata): their tags are never
            // constructed, so what reaches us is whatever pulldown-cmark makes
            // of the raw syntax. For most of them that is plain text; footnote
            // definitions are the exception and can be swallowed as link
            // reference definitions (see the module docs).
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::HtmlBlock => {
                self.flush_line();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if !self.line.is_empty() {
                    self.flush_code_line();
                }
                self.in_code_block = false;
                self.out.push("`=".to_string());
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
            }
            TagEnd::TableCell => {
                let cell = self.cell.take().unwrap_or_default();
                self.push_raw(&cell);
            }
            TagEnd::TableHead => {
                self.flush_line();
                // Micron takes the table's second line as the alignment row,
                // exactly as Markdown does (RNS rngit util.py:534-535), so it
                // is emitted rather than inferred.
                let alignments = self
                    .table
                    .as_ref()
                    .map(|t| t.alignments.clone())
                    .unwrap_or_default();
                let cols = alignments.len().max(1);
                let row: Vec<&str> = (0..cols)
                    .map(|i| match alignments.get(i) {
                        Some(Alignment::Center) => ":---:",
                        Some(Alignment::Right) => "---:",
                        _ => "---",
                    })
                    .collect();
                self.out.push(row.join(" | "));
            }
            TagEnd::TableRow => self.flush_line(),
            TagEnd::Table => {
                self.flush_line();
                self.out.push("`t".to_string());
                self.table = None;
            }
            TagEnd::Emphasis => self.style_toggle("`*"),
            TagEnd::Strong => self.style_toggle("`!"),
            TagEnd::Link => {
                let url = sanitize_link_part(&self.link_url.take().unwrap_or_default());
                let label = sanitize_link_part(self.link_label.trim());
                let label = if label.is_empty() { url.clone() } else { label };
                self.push_raw(&format!("`[{label}`{url}]"));
            }
            TagEnd::Image => {
                let image = self.image.take().unwrap_or_default();
                let alt = image.alt.trim();
                // A picture in the file area becomes an ordinary micron link
                // to it. NomadNet shows a link that saves the file to the
                // reader's download directory; lnomad draws it in the page.
                // Micron has no image construct, so this is the whole of what
                // is available, and inventing one would render as raw markup
                // in every other browser.
                if let Some(name) = files::file_ref(&image.dest) {
                    let label = match alt.is_empty() {
                        true => name.clone(),
                        false => alt.to_string(),
                    };
                    let label = sanitize_link_part(&label);
                    let target = sanitize_link_part(&files::micron_target(&name));
                    self.push_raw(&format!("`[{label}`{target}]"));
                } else if alt.is_empty() {
                    self.push_text("[image]");
                } else {
                    self.push_text(&format!("[image: {alt}]"));
                }
            }
            _ => {}
        }
    }

    /// Route text to whatever is currently collecting it: image alt, link
    /// label, literal block, or the current line (escaped). Embedded
    /// newlines (raw HTML, code text) split lines.
    fn text(&mut self, t: &str) {
        if let Some(image) = self.image.as_mut() {
            image.alt.push_str(t);
            return;
        }
        if self.link_url.is_some() {
            self.link_label.push_str(t);
            return;
        }
        for (i, segment) in t.split('\n').enumerate() {
            if i > 0 {
                if self.in_code_block {
                    self.flush_code_line();
                } else {
                    self.flush_line();
                }
            }
            if self.in_code_block {
                self.line.push_str(segment);
            } else if !segment.is_empty() {
                self.push_text(segment);
            }
        }
    }

    /// Inline code: no micron inline literal exists, so set it off with a
    /// background colour toggle (degradation documented in the module docs).
    fn inline_code(&mut self, code: &str) {
        if let Some(image) = self.image.as_mut() {
            image.alt.push_str(code);
            return;
        }
        if self.link_url.is_some() {
            self.link_label.push_str(code);
            return;
        }
        self.push_raw(&format!("`B{INLINE_CODE_BG}"));
        self.push_text(code);
        self.push_raw("`b");
    }

    /// Emit a style toggle unless a link/image is collecting text (labels run
    /// raw to the closing bracket, so styles inside them are dropped).
    fn style_toggle(&mut self, toggle: &str) {
        if self.image.is_none() && self.link_url.is_none() {
            self.push_raw(toggle);
        }
    }

    /// Append micron markup we emit deliberately (never escaped).
    fn push_raw(&mut self, s: &str) {
        if let Some(cell) = self.cell.as_mut() {
            cell.push_str(s);
            return;
        }
        if self.line.is_empty() {
            self.line_is_text = false;
        }
        self.line.push_str(s);
    }

    /// Append plain text, escaped so micron reads it verbatim.
    fn push_text(&mut self, s: &str) {
        let escaped = escape_micron_text(s);
        if let Some(cell) = self.cell.as_mut() {
            // Inside a table cell an unescaped `|` would start the next
            // column. The reference honours `\|` (rngit util.py:641-647).
            cell.push_str(&escaped.replace('|', "\\|"));
            return;
        }
        if self.line.is_empty() {
            self.line_is_text = true;
        }
        self.line.push_str(&escaped);
    }

    /// Finish the current line: line-escape a leading control character on
    /// plain-text lines, apply blockquote indentation, and emit.
    fn flush_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let mut line = std::mem::take(&mut self.line);
        if self.line_is_text && line.starts_with(LINE_CONTROL_CHARS) {
            line.insert(0, '\\');
        }
        if self.quote_depth > 0 {
            line = format!("{}{line}", "  ".repeat(self.quote_depth));
        }
        self.out.push(line);
    }

    /// Finish one verbatim literal-block line. Only the block toggle itself
    /// needs care: a content line reading `` `= `` must be emitted as
    /// `` \`= `` (the parser unescapes it inside literal blocks).
    fn flush_code_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        if line == "`=" {
            self.out.push("\\`=".to_string());
        } else {
            self.out.push(line);
        }
    }

    /// Separate blocks with one blank line (never at the start of output).
    fn block_sep(&mut self) {
        self.flush_line();
        if self.out.last().is_some_and(|l| !l.is_empty()) {
            self.out.push(String::new());
        }
    }

    /// Flush pending state and return the final micron source.
    fn finish(mut self) -> String {
        self.flush_line();
        let mut out = self.out.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }
}
