//! The file area: the one place that decides what a servable file is called
//! and where it appears on each side.
//!
//! Micron has no image construct — NomadNet's parser recognises formatting,
//! colour, sections, links, fields and tables, and nothing else — so a picture
//! reaches a mesh reader the only way NomadNet has ever offered: as a file,
//! linked from the page. The web side has no such limit and gets a real
//! `<img>`. Both sides are fed from this module so they can never disagree
//! about which files exist or what they are called:
//!
//! | side  | path                  |
//! |-------|-----------------------|
//! | mesh  | `/file/<name>`        |
//! | web   | `/files/<name>`       |
//! | post  | `![alt](<name>)`      |
//!
//! The area is deliberately flat: one directory, no subdirectories, no nested
//! request paths. The wire carries only a truncated hash of the request path,
//! so the node registers one handler per exact path anyway, and a flat
//! namespace makes [`sanitize_name`] a complete traversal guard rather than
//! one check among several.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// The mesh request path prefix, matching NomadNet's `serve_file` convention
/// (singular `file`, unlike the web side).
pub const NODE_PREFIX: &str = "/file/";

/// The web route prefix. Plural, so it cannot be confused with the mesh path
/// in a log line or a test.
pub const WEB_PREFIX: &str = "/files/";

/// Errors from reading the file area.
#[derive(Debug, Error)]
pub enum FilesError {
    /// The directory exists but could not be read.
    #[error("reading file area {path}: {source}")]
    Read {
        /// The directory that could not be read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// One servable file: its request name, where it lives, and how big it is.
///
/// The bytes are deliberately absent. A blog with twenty photographs should
/// not pin them all in memory for the lifetime of the process, so the node
/// and the web server read the file when a request asks for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// The sanitised name the file is served under, on both sides.
    pub name: String,
    /// The file's path on disk.
    pub path: PathBuf,
    /// Its size in bytes, as of the load that produced this entry.
    pub len: u64,
}

/// Where the file area lives and how large a single file may be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileArea {
    /// The directory holding the servable files.
    pub dir: PathBuf,
    /// The largest file that will be served. Anything above it is skipped
    /// with a warning: a blog is not a file server, and an unbounded transfer
    /// over LoRa denies service to every other reader of the same node.
    pub max_bytes: u64,
}

/// The default ceiling for a single file, 10 MiB.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

impl FileArea {
    /// A file area at `dir` with the default size ceiling.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FileArea {
            dir: dir.into(),
            max_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

/// The sanitised form of a requested file name, or `None` when the request
/// cannot name a file in the area.
///
/// This is the traversal guard for every entry point: the mesh handler, the
/// web route and the directory load all pass through it. A name may not be
/// empty, may not contain a path separator or a control character, may not
/// consist only of dots, and may not begin with a dot (which would hide it in
/// the directory listing the operator sees). Because the area is flat, a name
/// that survives this can only ever resolve inside it.
///
/// Note that the web route receives its name percent-decoded, so an attempt
/// like `%2e%2e%2f` arrives here as `../` and is rejected on the separator.
pub fn sanitize_name(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > 255 {
        return None;
    }
    if raw.starts_with('.') || raw.chars().all(|c| c == '.') {
        return None;
    }
    if raw.chars().any(|c| c == '/' || c == '\\' || c.is_control()) {
        return None;
    }
    Some(raw.to_string())
}

/// The mesh request path a file is served under.
pub fn node_path(name: &str) -> String {
    format!("{NODE_PREFIX}{name}")
}

/// The micron link target for a file on the same node (`:` meaning "here").
pub fn micron_target(name: &str) -> String {
    format!(":{}", node_path(name))
}

/// The web path a file is served under.
pub fn web_path(name: &str) -> String {
    format!("{WEB_PREFIX}{name}")
}

/// The file-area name a Markdown reference points at, or `None` when it
/// points somewhere else entirely.
///
/// Accepted, because these are the ways an author naturally writes a
/// reference to a file that sits beside the posts:
///
/// ```text
/// antenne.jpg          ./antenne.jpg
/// files/antenne.jpg    /files/antenne.jpg
/// ```
///
/// Everything else — a URL with a scheme, a network path (`//host/x`), a
/// deeper relative path, a fragment or query — yields `None` and is left
/// exactly as the author wrote it. An external image therefore keeps
/// travelling as an external reference and degrades on the mesh side as it
/// always has.
pub fn file_ref(dest_url: &str) -> Option<String> {
    if dest_url.is_empty() || dest_url.contains(['?', '#']) {
        return None;
    }
    // A scheme (`https:`, `mailto:`) or a network path is somebody else's.
    if dest_url.starts_with("//") || has_scheme(dest_url) {
        return None;
    }
    let rest = dest_url
        .strip_prefix(WEB_PREFIX)
        .or_else(|| dest_url.strip_prefix("files/"))
        .or_else(|| dest_url.strip_prefix("./"))
        .unwrap_or(dest_url);
    // What is left has to be a bare name: the area is flat.
    if rest.contains('/') {
        return None;
    }
    sanitize_name(rest)
}

/// Whether a reference carries a URL scheme (`scheme:` with a letter first).
fn has_scheme(dest_url: &str) -> bool {
    let Some(colon) = dest_url.find(':') else {
        return false;
    };
    let scheme = &dest_url[..colon];
    !scheme.is_empty()
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

/// The content type a file is served with on the web side.
///
/// A small, deliberate table: the formats a blog post actually references,
/// plus a safe default. Unknown extensions are served as an opaque download
/// rather than guessed at, and no sniffing happens anywhere.
pub fn mime_for(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Read the file area, keyed by served name.
///
/// A missing directory is not an error: a blog without pictures simply has no
/// file area, and demanding the directory exist would make the feature
/// mandatory. Subdirectories are skipped (the area is flat), as are names that
/// [`sanitize_name`] rejects and files above `area.max_bytes` — each with a
/// line on stderr, because silently not serving a file the author put there
/// is exactly the kind of thing that costs an afternoon.
///
/// Symlinks are followed and served. The area is operator-controlled, and
/// pointing it at photographs that live elsewhere is a reasonable thing to
/// want; the guard against a hostile *request* is [`sanitize_name`], which no
/// symlink can help a requester past.
pub fn load_files_dir(area: &FileArea) -> Result<BTreeMap<String, FileEntry>, FilesError> {
    let mut files = BTreeMap::new();
    let entries = match std::fs::read_dir(&area.dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(source) => {
            return Err(FilesError::Read {
                path: area.dir.display().to_string(),
                source,
            })
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("lblogd: file area: skipping unreadable entry: {e}");
                continue;
            }
        };
        let path = entry.path();
        // Follows symlinks, unlike `entry.file_type()`.
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                eprintln!("lblogd: file area: skipping {}: {e}", path.display());
                continue;
            }
        };
        if !meta.is_file() {
            continue;
        }
        let raw = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = sanitize_name(&raw) else {
            eprintln!("lblogd: file area: skipping {raw}: unusable file name");
            continue;
        };
        if meta.len() > area.max_bytes {
            eprintln!(
                "lblogd: file area: skipping {name}: {} bytes exceeds max_file_bytes ({})",
                meta.len(),
                area.max_bytes
            );
            continue;
        }
        files.insert(
            name.clone(),
            FileEntry {
                name,
                path,
                len: meta.len(),
            },
        );
    }
    Ok(files)
}

/// Read one entry's bytes, checking the size ceiling again.
///
/// The re-check is not paranoia: the entry was measured when the snapshot was
/// built, and the file may have been rewritten since. Serving from disk is
/// what keeps large files out of memory, and this is the price.
pub fn read_entry(entry: &FileEntry, max_bytes: u64) -> Result<Vec<u8>, std::io::Error> {
    let meta = std::fs::metadata(&entry.path)?;
    if meta.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} grew to {} bytes, above max_file_bytes ({max_bytes})",
                entry.name,
                meta.len()
            ),
        ));
    }
    std::fs::read(&entry.path)
}

/// The directory a file area defaults to for a given posts directory: a
/// sibling `files/`, so `posts/` and `files/` sit next to each other under the
/// data directory.
pub fn default_files_dir(posts_dir: &Path) -> PathBuf {
    match posts_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("files"),
        _ => PathBuf::from("files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_everything_that_could_leave_the_directory() {
        assert_eq!(sanitize_name("antenne.jpg").as_deref(), Some("antenne.jpg"));
        assert_eq!(sanitize_name("a b.png").as_deref(), Some("a b.png"));
        assert_eq!(sanitize_name(".."), None);
        assert_eq!(sanitize_name("../../etc/passwd"), None);
        assert_eq!(sanitize_name("a/b"), None);
        assert_eq!(sanitize_name("a\\b"), None);
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name("."), None);
        assert_eq!(sanitize_name(".hidden"), None);
        assert_eq!(sanitize_name("a\nb"), None);
        assert_eq!(sanitize_name("a\0b"), None);
    }

    #[test]
    fn paths_have_the_shape_each_side_expects() {
        assert_eq!(node_path("a.png"), "/file/a.png");
        assert_eq!(micron_target("a.png"), ":/file/a.png");
        assert_eq!(web_path("a.png"), "/files/a.png");
    }

    #[test]
    fn file_ref_accepts_the_ways_an_author_writes_a_local_file() {
        for input in [
            "antenne.jpg",
            "./antenne.jpg",
            "files/antenne.jpg",
            "/files/antenne.jpg",
        ] {
            assert_eq!(
                file_ref(input).as_deref(),
                Some("antenne.jpg"),
                "input {input}"
            );
        }
    }

    #[test]
    fn file_ref_leaves_everything_else_alone() {
        for input in [
            "https://example.com/a.png",
            "//example.com/a.png",
            "mailto:a@b.c",
            "data:image/png;base64,AAAA",
            "sub/dir/a.png",
            "/other/a.png",
            "a.png?v=2",
            "a.png#frag",
            "",
            "../a.png",
        ] {
            assert_eq!(file_ref(input), None, "input {input}");
        }
    }

    #[test]
    fn mime_covers_the_image_formats_and_defaults_to_octet_stream() {
        assert_eq!(mime_for("a.png"), "image/png");
        assert_eq!(mime_for("a.JPG"), "image/jpeg");
        assert_eq!(mime_for("a.jpeg"), "image/jpeg");
        assert_eq!(mime_for("a.gif"), "image/gif");
        assert_eq!(mime_for("a.svg"), "image/svg+xml");
        assert_eq!(mime_for("a.bin"), "application/octet-stream");
        assert_eq!(mime_for("noext"), "application/octet-stream");
    }

    #[test]
    fn default_dir_is_a_sibling_of_the_posts_directory() {
        assert_eq!(
            default_files_dir(Path::new("/var/lib/lblogd/posts")),
            PathBuf::from("/var/lib/lblogd/files")
        );
        assert_eq!(
            default_files_dir(Path::new("posts")),
            PathBuf::from("files")
        );
    }
}
