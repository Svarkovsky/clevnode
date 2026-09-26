//! The post/content model: frontmatter parsing, slugs, and directory loading.
//!
//! A post is a Markdown file, optionally opening with a TOML frontmatter
//! block delimited by `+++` lines:
//!
//! ```text
//! +++
//! title = "Hello"       # optional, defaults to the file name without .md
//! date = "2026-07-12"   # optional, defaults to the file's mtime (UTC)
//! author = "Someone"    # optional, defaults to the blog's author
//! slug = "hello"        # optional, defaults to slugify(title)
//! +++
//!
//! Markdown body...
//! ```
//!
//! Every field is optional, and so is the block itself: a plain `.md` file
//! with nothing but prose is a complete post, titled after its file name and
//! dated by its mtime (see [`PostDefaults`]). Frontmatter is therefore a way
//! to override what the file already says, not a precondition. What remains
//! an error is a frontmatter block that opens and never closes, an invalid
//! date, and a title that slugifies to nothing.
//!
//! Dates are plain `YYYY-MM-DD` values ordered by (year, month, day); no
//! calendar library is involved.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use thiserror::Error;

/// Errors from parsing a post or loading a posts directory.
#[derive(Debug, Error)]
pub enum PostError {
    /// The opening `+++` has no matching closing `+++` line.
    #[error("unterminated frontmatter: no closing +++ line")]
    UnterminatedFrontmatter,
    /// The frontmatter block is not valid TOML.
    #[error("invalid frontmatter TOML: {0}")]
    Toml(#[from] toml::de::Error),
    /// The `date` field is not a valid `YYYY-MM-DD` calendar date.
    #[error("invalid date {0:?}: expected YYYY-MM-DD")]
    InvalidDate(String),
    /// Neither an explicit slug nor the title yields a non-empty slug.
    #[error("empty slug: title {0:?} slugifies to nothing and no explicit slug is set")]
    EmptySlug(String),
    /// A file in the posts directory failed to parse.
    #[error("{}: {source}", path.display())]
    File {
        /// The offending file.
        path: PathBuf,
        /// The underlying parse error.
        source: Box<PostError>,
    },
    /// A filesystem read failed.
    #[error("reading {}: {source}", path.display())]
    Io {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// A calendar date ordered by (year, month, day).
///
/// Parsed from `YYYY-MM-DD` with real month/day range checks (including leap
/// years); displayed back in the same form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    /// Four-digit year.
    pub year: i32,
    /// Month, 1-12.
    pub month: u8,
    /// Day of month, 1-31 (validated against the month).
    pub day: u8,
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl FromStr for Date {
    type Err = PostError;

    fn from_str(s: &str) -> Result<Date, PostError> {
        let invalid = || PostError::InvalidDate(s.to_string());
        let parts: Vec<&str> = s.split('-').collect();
        let [y, m, d] = parts.as_slice() else {
            return Err(invalid());
        };
        if y.len() != 4 || m.len() != 2 || d.len() != 2 {
            return Err(invalid());
        }
        let year: i32 = y.parse().map_err(|_| invalid())?;
        let month: u8 = m.parse().map_err(|_| invalid())?;
        let day: u8 = d.parse().map_err(|_| invalid())?;
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return Err(invalid());
        }
        Ok(Date { year, month, day })
    }
}

impl Date {
    /// The UTC calendar day of a [`SystemTime`], used for the mtime fallback.
    ///
    /// Times before the epoch are ordinary negative day counts, not an error:
    /// `civil_from_days` is defined over the whole range, so a file with a
    /// backdated mtime still yields the date it claims.
    pub fn from_system_time(time: SystemTime) -> Date {
        let secs = match time.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        };
        civil_from_days(secs.div_euclid(86_400))
    }
}

/// Convert a day count relative to 1970-01-01 into a Gregorian date.
///
/// Howard Hinnant's `civil_from_days`, the standard shift-the-epoch-to-March
/// method: with March as month 0 the leap day lands at the end of the year,
/// which makes the day-of-year arithmetic branch-free.
fn civil_from_days(days: i64) -> Date {
    // Shift the epoch from 1970-01-01 to 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of (March-based) year
    let mp = (5 * doy + 2) / 153; // March-based month, [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    Date {
        year: year as i32,
        month,
        day,
    }
}

/// The number of days in `month` of `year` (Gregorian, leap-year aware).
fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// A parsed blog post.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Post {
    /// The post title from the frontmatter.
    pub title: String,
    /// The publication date from the frontmatter.
    pub date: Date,
    /// Who wrote this one, when it is not the blog's author. `None` means
    /// the blog's author is credited.
    pub author: Option<String>,
    /// The URL slug: the explicit frontmatter `slug`, else `slugify(title)`.
    pub slug: String,
    /// The Markdown body (everything after the closing `+++` line).
    pub body_md: String,
}

/// The raw TOML frontmatter shape. Every field is optional; unknown fields
/// are tolerated. The default value stands in for a file with no frontmatter
/// block at all.
#[derive(Default, Deserialize)]
struct RawFrontmatter {
    title: Option<String>,
    date: Option<String>,
    author: Option<String>,
    slug: Option<String>,
}

/// What a post inherits from its file when the frontmatter stays silent.
///
/// Every field of the frontmatter is optional, and so is the frontmatter
/// block itself: the cheapest way to publish is to drop a plain `.md` file
/// into the posts directory and write. [`PostDefaults::for_file`] derives
/// both values from the file itself, so that file is already a complete post.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostDefaults {
    /// Title to use when the frontmatter sets none: the file name without its
    /// extension, verbatim.
    pub title: String,
    /// Date to use when the frontmatter sets none: the file's modification
    /// time.
    pub date: Date,
}

impl PostDefaults {
    /// Derive the defaults from a file: its stem as the title, its
    /// modification time as the date.
    ///
    /// The date is the mtime's **UTC** calendar day. Without a timezone
    /// database there is no honest way to render a local one, and guessing an
    /// offset would silently shift dates. A file saved shortly after local
    /// midnight therefore dates to the previous day; setting `date` in the
    /// frontmatter is the fix, and is what any post that cares about its date
    /// should do anyway.
    pub fn for_file(path: &Path) -> Result<PostDefaults, PostError> {
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let modified = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|source| PostError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(PostDefaults {
            title,
            date: Date::from_system_time(modified),
        })
    }
}

/// Parse a post source (optional frontmatter plus Markdown body) into a
/// [`Post`], filling anything the frontmatter omits from `defaults`.
///
/// A source that does not open with `+++` is taken as body only. That makes
/// the frontmatter a way to override the file-derived title, date and slug
/// rather than a precondition for being a post at all.
pub fn parse_post(source: &str, defaults: &PostDefaults) -> Result<Post, PostError> {
    let (raw, body_md) = split_frontmatter(source)?;

    let title = match raw.title {
        Some(t) if !t.trim().is_empty() => t,
        _ => defaults.title.clone(),
    };
    let date = match raw.date {
        Some(d) => d.parse()?,
        None => defaults.date,
    };
    let slug = match raw.slug {
        Some(s) if !s.is_empty() => s,
        _ => {
            let s = slugify(&title);
            if s.is_empty() {
                return Err(PostError::EmptySlug(title));
            }
            s
        }
    };

    Ok(Post {
        title,
        date,
        author: raw.author.filter(|a| !a.trim().is_empty()),
        slug,
        body_md,
    })
}

/// Split a source into its frontmatter fields and its body.
///
/// A source not opening with `+++` has no frontmatter and is all body. One
/// that opens with `+++` and never closes it is an error rather than body:
/// the author clearly meant to write frontmatter, and silently serving the
/// TOML as prose would hide the typo.
fn split_frontmatter(source: &str) -> Result<(RawFrontmatter, String), PostError> {
    let mut lines = source.split('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_end_matches('\r') != "+++" {
        return Ok((RawFrontmatter::default(), source.to_string()));
    }

    let mut frontmatter = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim_end_matches('\r') == "+++" {
            closed = true;
            break;
        }
        frontmatter.push_str(line);
        frontmatter.push('\n');
    }
    if !closed {
        return Err(PostError::UnterminatedFrontmatter);
    }
    let body_md: String = lines.collect::<Vec<&str>>().join("\n");
    Ok((toml::from_str(&frontmatter)?, body_md))
}

/// Slugify a string for use in URLs and page paths.
///
/// Keeps ASCII alphanumerics (lowercased); every other run of characters
/// becomes a single hyphen; leading/trailing hyphens are trimmed. Non-ASCII
/// characters are treated as separators, matching the micron heading-anchor
/// slug rules, so slugs are always plain lowercase ASCII.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_hyphen = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !out.is_empty() && !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Load every `*.md` file in `dir` as a post, sorted by date descending
/// (newest first) with a stable title tie-break. A malformed file surfaces as
/// [`PostError::File`] naming the file.
pub fn load_posts_dir(dir: &Path) -> Result<Vec<Post>, PostError> {
    let read_err = |source| PostError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut posts = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(read_err)? {
        let path = entry.map_err(read_err)?.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let source = std::fs::read_to_string(&path).map_err(|source| PostError::Io {
            path: path.clone(),
            source,
        })?;
        let defaults = PostDefaults::for_file(&path)?;
        let post = parse_post(&source, &defaults).map_err(|e| PostError::File {
            path: path.clone(),
            source: Box::new(e),
        })?;
        posts.push(post);
    }
    posts.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.title.cmp(&b.title)));
    Ok(posts)
}
