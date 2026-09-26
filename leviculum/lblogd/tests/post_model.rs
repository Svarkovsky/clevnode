//! Unit tests for the post/content model: frontmatter parsing, dates, slugs,
//! and directory loading.

use std::fs;

use lblogd::post::{load_posts_dir, parse_post, slugify, Date, PostDefaults, PostError};

/// Defaults a file would supply, for the cases that exercise the
/// frontmatter rather than the fallbacks.
fn defaults() -> PostDefaults {
    PostDefaults {
        title: "from-file-name".to_string(),
        date: "2000-01-01".parse().unwrap(),
    }
}

const VALID: &str = "+++\ntitle = \"Hello World\"\ndate = \"2026-07-12\"\n+++\n\nBody text.\n";

#[test]
fn valid_frontmatter_parses() {
    let post = parse_post(VALID, &defaults()).unwrap();
    assert_eq!(post.title, "Hello World");
    assert_eq!(post.date.to_string(), "2026-07-12");
    assert_eq!(post.slug, "hello-world");
    assert!(post.body_md.contains("Body text."));
}

#[test]
fn missing_title_falls_back_to_the_file_default() {
    let src = "+++\ndate = \"2026-07-12\"\n+++\nBody";
    let post = parse_post(src, &defaults()).unwrap();
    assert_eq!(post.title, "from-file-name");
    assert_eq!(post.slug, "from-file-name");
    assert_eq!(
        post.date.to_string(),
        "2026-07-12",
        "the set date still wins"
    );
}

#[test]
fn blank_title_falls_back_to_the_file_default() {
    let src = "+++\ntitle = \"  \"\ndate = \"2026-07-12\"\n+++\nBody";
    assert_eq!(
        parse_post(src, &defaults()).unwrap().title,
        "from-file-name"
    );
}

#[test]
fn missing_date_falls_back_to_the_file_default() {
    let src = "+++\ntitle = \"T\"\n+++\nBody";
    let post = parse_post(src, &defaults()).unwrap();
    assert_eq!(post.date.to_string(), "2000-01-01");
    assert_eq!(post.title, "T", "the set title still wins");
}

#[test]
fn invalid_date_is_an_error() {
    for bad in [
        "2026-13-01",
        "2026-02-30",
        "2026-7-12",
        "12.07.2026",
        "soon",
    ] {
        let src = format!("+++\ntitle = \"T\"\ndate = \"{bad}\"\n+++\nBody");
        assert!(
            matches!(
                parse_post(&src, &defaults()),
                Err(PostError::InvalidDate(_))
            ),
            "date {bad:?} should be rejected"
        );
    }
}

#[test]
fn leap_day_is_accepted() {
    let src = "+++\ntitle = \"T\"\ndate = \"2024-02-29\"\n+++\nBody";
    assert_eq!(
        parse_post(src, &defaults()).unwrap().date.to_string(),
        "2024-02-29"
    );
}

#[test]
fn system_time_maps_to_the_utc_calendar_day() {
    // Reference values from `date -u -d @<secs>`.
    for (secs, expected) in [
        (0_i64, "1970-01-01"),
        (86_399, "1970-01-01"),      // last second of the day
        (86_400, "1970-01-02"),      // first of the next
        (951_782_400, "2000-02-29"), // leap day of a century leap year
        (1_078_012_800, "2004-02-29"),
        (1_735_689_600, "2025-01-01"),
        (4_102_444_800, "2100-01-01"), // 2100 is not a leap year
    ] {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
        assert_eq!(
            Date::from_system_time(t).to_string(),
            expected,
            "epoch second {secs}"
        );
    }
}

#[test]
fn system_time_before_the_epoch_still_yields_a_date() {
    let t = std::time::UNIX_EPOCH - std::time::Duration::from_secs(86_400);
    assert_eq!(Date::from_system_time(t).to_string(), "1969-12-31");
}

#[test]
fn dates_order_chronologically() {
    let a: Date = "2025-12-31".parse().unwrap();
    let b: Date = "2026-01-02".parse().unwrap();
    let c: Date = "2026-01-10".parse().unwrap();
    assert!(a < b && b < c);
}

#[test]
fn explicit_slug_is_honored() {
    let src = "+++\ntitle = \"Hello World\"\ndate = \"2026-07-12\"\nslug = \"custom\"\n+++\nBody";
    assert_eq!(parse_post(src, &defaults()).unwrap().slug, "custom");
}

#[test]
fn slug_defaults_from_title() {
    let src = "+++\ntitle = \"A Post, With Punctuation!\"\ndate = \"2026-07-12\"\n+++\nBody";
    assert_eq!(
        parse_post(src, &defaults()).unwrap().slug,
        "a-post-with-punctuation"
    );
}

#[test]
fn a_file_without_frontmatter_is_all_body() {
    // The cheapest way to publish: drop a plain .md file and write.
    let post = parse_post("# Just Markdown\n\nSome prose.\n", &defaults()).unwrap();
    assert_eq!(post.title, "from-file-name");
    assert_eq!(post.slug, "from-file-name");
    assert_eq!(post.date.to_string(), "2000-01-01");
    assert_eq!(
        post.body_md, "# Just Markdown\n\nSome prose.\n",
        "nothing may be swallowed as frontmatter"
    );
}

#[test]
fn a_leading_plus_line_that_is_not_a_delimiter_stays_body() {
    let post = parse_post("++++\nstill body\n", &defaults()).unwrap();
    assert_eq!(post.body_md, "++++\nstill body\n");
}

#[test]
fn unterminated_frontmatter_is_an_error() {
    let src = "+++\ntitle = \"T\"\ndate = \"2026-07-12\"\nBody without closing";
    assert!(matches!(
        parse_post(src, &defaults()),
        Err(PostError::UnterminatedFrontmatter)
    ));
}

#[test]
fn invalid_toml_is_an_error() {
    let src = "+++\ntitle = unquoted\n+++\nBody";
    assert!(matches!(
        parse_post(src, &defaults()),
        Err(PostError::Toml(_))
    ));
}

#[test]
fn slugify_cases() {
    assert_eq!(slugify("Hello World"), "hello-world");
    assert_eq!(slugify("  Hello,   World!  "), "hello-world");
    assert_eq!(slugify("CamelCase and 123"), "camelcase-and-123");
    assert_eq!(slugify("--a--b--"), "a-b");
    assert_eq!(slugify("trailing---"), "trailing");
    // Non-ASCII characters are separators, keeping slugs plain ASCII.
    assert_eq!(slugify("Grün und Über"), "gr-n-und-ber");
    assert_eq!(slugify(""), "");
    assert_eq!(slugify("!!!"), "");
}

#[test]
fn all_punctuation_title_without_slug_is_an_error() {
    let src = "+++\ntitle = \"!!!\"\ndate = \"2026-07-12\"\n+++\nBody";
    assert!(matches!(
        parse_post(src, &defaults()),
        Err(PostError::EmptySlug(_))
    ));
}

fn write_post(dir: &std::path::Path, name: &str, title: &str, date: &str) {
    let src = format!("+++\ntitle = \"{title}\"\ndate = \"{date}\"\n+++\nBody of {title}.\n");
    fs::write(dir.join(name), src).unwrap();
}

#[test]
fn load_posts_dir_sorts_newest_first_with_title_tiebreak() {
    let dir = tempfile::tempdir().unwrap();
    write_post(dir.path(), "old.md", "Oldest", "2025-01-01");
    write_post(dir.path(), "new.md", "Newest", "2026-07-01");
    write_post(dir.path(), "tie-b.md", "Beta", "2026-03-15");
    write_post(dir.path(), "tie-a.md", "Alpha", "2026-03-15");
    fs::write(dir.path().join("notes.txt"), "not a post").unwrap();

    let posts = load_posts_dir(dir.path()).unwrap();
    let titles: Vec<&str> = posts.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, ["Newest", "Alpha", "Beta", "Oldest"]);
}

#[test]
fn load_posts_dir_surfaces_malformed_file_with_path() {
    let dir = tempfile::tempdir().unwrap();
    write_post(dir.path(), "good.md", "Good", "2026-01-01");
    fs::write(
        dir.path().join("broken.md"),
        "+++\ntitle = \"X\"\ndate = \"2026-13-45\"\n+++\nBody",
    )
    .unwrap();

    let err = load_posts_dir(dir.path()).unwrap_err();
    let PostError::File { path, source } = err else {
        panic!("expected PostError::File, got {err:?}");
    };
    assert!(path.ends_with("broken.md"));
    assert!(matches!(*source, PostError::InvalidDate(_)));
}

#[test]
fn load_posts_dir_titles_a_bare_file_after_its_name_and_dates_it_by_mtime() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("ein-nackter-post.md"), "Nur Prosa.\n").unwrap();

    let posts = load_posts_dir(dir.path()).unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].title, "ein-nackter-post");
    assert_eq!(posts[0].slug, "ein-nackter-post");
    assert_eq!(posts[0].body_md, "Nur Prosa.\n");
    // The mtime is "just now", so the date must be today's UTC day.
    let today = Date::from_system_time(std::time::SystemTime::now());
    assert_eq!(posts[0].date, today);
}

#[test]
fn load_posts_dir_uses_the_real_mtime_not_the_current_time() {
    // Backdate the file and check the date follows it, so the fallback is
    // genuinely the mtime rather than "whenever the server started".
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alt.md");
    fs::write(&path, "Alter Text.\n").unwrap();
    let backdated = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    fs::File::open(&path)
        .unwrap()
        .set_modified(backdated)
        .unwrap();

    let posts = load_posts_dir(dir.path()).unwrap();
    assert_eq!(posts[0].date.to_string(), "2001-09-09");
}

#[test]
fn load_posts_dir_missing_dir_is_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    assert!(matches!(
        load_posts_dir(&missing),
        Err(PostError::Io { .. })
    ));
}
