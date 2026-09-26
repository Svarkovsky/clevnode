//! Saving fetched `/file/` payloads to disk.
//!
//! A NomadNet file download arrives as raw bytes (see
//! [`crate::fetch::Session::download_file`]); this module owns everything that
//! happens after: deriving a safe filename from the URL path, resolving where
//! the file lands (the CLI's `--output`, the TUI's download directory, or the
//! current working directory), and de-duplicating against an existing file so
//! nothing is overwritten silently.
//!
//! The filename prefers the server's Resource metadata name (NomadNet's
//! `serve_file` sends `{"name": ...}`), falling back to the URL path basename.
//! Either source is untrusted and goes through [`basename`]: path separators
//! are stripped and a dot-only name is replaced, so a hostile name can never
//! escape the target directory.

use std::io;
use std::path::{Path, PathBuf};

/// The fallback filename when the URL path yields nothing usable (an empty
/// basename, or a name that is only dots).
pub(crate) const FALLBACK_NAME: &str = "download";

/// The filename implied by a request path: everything after the last `/`,
/// sanitised so it cannot escape the directory it is saved into. Any remaining
/// path separator (`\` survives the split) is stripped, and an empty or
/// dot-only result (``, `.`, `..`) falls back to `FALLBACK_NAME`.
pub fn basename(path: &str) -> String {
    let raw = path.rsplit('/').next().unwrap_or("");
    let name: String = raw.chars().filter(|&c| c != '/' && c != '\\').collect();
    if name.is_empty() || name.chars().all(|c| c == '.') {
        FALLBACK_NAME.to_string()
    } else {
        name
    }
}

/// The filename a file response's Resource metadata carries: the msgpack map
/// `{"name": <str or bin>}` NomadNet's `serve_file` sends. Returns the raw,
/// UNSANITISED name (a bin name is decoded as lossy UTF-8), or `None` when the
/// blob is not a map, has no `name` key, or the name is empty. Sanitisation is
/// deliberately left to the caller (see [`choose_name`]) so this parse is
/// testable in isolation.
pub fn parse_metadata_name(metadata: &[u8]) -> Option<String> {
    let mut cursor = std::io::Cursor::new(metadata);
    let value = rmpv::decode::read_value(&mut cursor).ok()?;
    let rmpv::Value::Map(entries) = value else {
        return None;
    };
    let name = entries.iter().find_map(|(key, value)| {
        let is_name = match key {
            rmpv::Value::String(s) => s.as_bytes() == b"name",
            rmpv::Value::Binary(b) => b.as_slice() == b"name",
            _ => false,
        };
        if !is_name {
            return None;
        }
        match value {
            rmpv::Value::String(s) => Some(String::from_utf8_lossy(s.as_bytes()).into_owned()),
            rmpv::Value::Binary(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    })?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// The filename a download is saved under: the server-sent name when it
/// survives sanitisation, else the URL path basename.
///
/// The server name is untrusted, so it is run through [`basename`], which
/// strips path separators and maps `..`/`.`/empty to `FALLBACK_NAME`; a name
/// like `../../etc/passwd` or `a/b/c` can therefore never write outside the
/// download directory. A server name that sanitises to the bare fallback is
/// discarded in favour of the URL basename.
pub fn choose_name(server_name: Option<&str>, url_path: &str) -> String {
    server_name
        .map(basename)
        .filter(|name| name != FALLBACK_NAME)
        .unwrap_or_else(|| basename(url_path))
}

/// Resolve where a download lands for the CLI's `--output`.
///
/// Returns `(path, explicit)`: an `--output` that is an existing directory or
/// is spelled with a trailing `/` receives `name` inside it (`explicit` =
/// `false`); any other `--output` names the exact file (`explicit` = `true`);
/// no `--output` puts `name` into `cwd`. Only a non-explicit path is
/// de-duplicated by the caller — naming the exact file opts into overwriting.
pub fn resolve_output(output: Option<&Path>, name: &str, cwd: &Path) -> (PathBuf, bool) {
    match output {
        None => (cwd.join(name), false),
        Some(out) => {
            let trailing_slash = out.as_os_str().to_string_lossy().ends_with('/');
            if trailing_slash || out.is_dir() {
                (out.join(name), false)
            } else {
                (out.to_path_buf(), true)
            }
        }
    }
}

/// The first free variant of `path`: the path itself when nothing exists
/// there, else ` (1)`, ` (2)`, ... inserted before the extension.
pub fn dedup_path(path: &Path) -> PathBuf {
    dedup_path_with(path, |p| p.exists())
}

/// [`dedup_path`] over an injectable existence check, so the numbering is a
/// pure function unit tests can drive without touching the filesystem.
fn dedup_path_with(path: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    if !exists(path) {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| FALLBACK_NAME.to_string());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    let mut n: u64 = 1;
    loop {
        let candidate = match &ext {
            Some(e) => parent.join(format!("{stem} ({n}).{e}")),
            None => parent.join(format!("{stem} ({n})")),
        };
        if !exists(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Save `bytes` into `dir` (created if missing) under `name`, de-duplicating
/// against existing files. Returns the path actually written.
pub fn save_to_dir(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dedup_path(&dir.join(name));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// The directory the TUI saves downloads into: `$XDG_DOWNLOAD_DIR`, falling
/// back to `$HOME/Downloads`. `None` only when neither variable is set.
pub fn download_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_takes_the_last_path_component() {
        assert_eq!(basename("/file/hello.bin"), "hello.bin");
        assert_eq!(basename("/file/docs/manual.pdf"), "manual.pdf");
        assert_eq!(basename("/file/a/b/c.pdf"), "c.pdf");
    }

    #[test]
    fn basename_cannot_escape_the_target_dir() {
        // Traversal components and separators are neutralised.
        assert_eq!(basename("/file/.."), FALLBACK_NAME);
        assert_eq!(basename("/file/../../etc/passwd"), "passwd");
        assert_eq!(basename("/file/evil\\..\\name.bin"), "evil..name.bin");
        assert_eq!(basename("/file/"), FALLBACK_NAME);
        assert_eq!(basename(""), FALLBACK_NAME);
        assert_eq!(basename("/file/."), FALLBACK_NAME);
    }

    /// Msgpack-encode a single-entry map `{key: value}`.
    fn msgpack_map(key: rmpv::Value, value: rmpv::Value) -> Vec<u8> {
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &rmpv::Value::Map(vec![(key, value)]))
            .expect("encode metadata map");
        buf
    }

    #[test]
    fn parse_metadata_name_reads_a_str_name() {
        let blob = msgpack_map("name".into(), "report.pdf".into());
        assert_eq!(parse_metadata_name(&blob).as_deref(), Some("report.pdf"));
    }

    #[test]
    fn parse_metadata_name_reads_a_bin_name() {
        // NomadNet may pack the name as bytes; it decodes as UTF-8.
        let blob = msgpack_map("name".into(), rmpv::Value::Binary(b"report.pdf".to_vec()));
        assert_eq!(parse_metadata_name(&blob).as_deref(), Some("report.pdf"));
    }

    #[test]
    fn parse_metadata_name_rejects_a_map_without_name() {
        let blob = msgpack_map("size".into(), rmpv::Value::from(42));
        assert_eq!(parse_metadata_name(&blob), None);
    }

    #[test]
    fn parse_metadata_name_rejects_a_non_map() {
        let mut blob = Vec::new();
        rmpv::encode::write_value(&mut blob, &rmpv::Value::from(42)).expect("encode int");
        assert_eq!(parse_metadata_name(&blob), None);
    }

    #[test]
    fn parse_metadata_name_rejects_an_empty_name() {
        let blob = msgpack_map("name".into(), "".into());
        assert_eq!(parse_metadata_name(&blob), None);
    }

    #[test]
    fn parse_metadata_name_does_not_sanitise() {
        // Sanitisation is choose_name's job; the parse returns the raw name.
        let blob = msgpack_map("name".into(), "../../etc/passwd".into());
        assert_eq!(
            parse_metadata_name(&blob).as_deref(),
            Some("../../etc/passwd")
        );
    }

    #[test]
    fn choose_name_prefers_the_server_name() {
        assert_eq!(choose_name(Some("report.pdf"), "/file/x.bin"), "report.pdf");
    }

    #[test]
    fn choose_name_sanitises_the_server_name() {
        // A hostile server name is reduced to its basename, so it can never
        // write outside the download directory.
        assert_eq!(choose_name(Some("a/b/evil.sh"), "/file/x.bin"), "evil.sh");
        assert_eq!(
            choose_name(Some("../../etc/passwd"), "/file/x.bin"),
            "passwd"
        );
    }

    #[test]
    fn choose_name_falls_back_to_the_url_basename() {
        assert_eq!(choose_name(None, "/file/x.bin"), "x.bin");
        // A server name that sanitises to the bare fallback is discarded.
        assert_eq!(choose_name(Some(".."), "/file/x.bin"), "x.bin");
    }

    #[test]
    fn resolve_omitted_output_uses_cwd() {
        let (path, explicit) = resolve_output(None, "a.bin", Path::new("/work"));
        assert_eq!(path, Path::new("/work/a.bin"));
        assert!(!explicit);
    }

    #[test]
    fn resolve_trailing_slash_output_is_a_directory() {
        // The directory need not exist: the trailing slash alone marks it.
        let (path, explicit) = resolve_output(
            Some(Path::new("/nonexistent/dl/")),
            "a.bin",
            Path::new("/work"),
        );
        assert_eq!(path, Path::new("/nonexistent/dl/a.bin"));
        assert!(!explicit);
    }

    #[test]
    fn resolve_existing_directory_output_receives_the_basename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, explicit) = resolve_output(Some(dir.path()), "a.bin", Path::new("/work"));
        assert_eq!(path, dir.path().join("a.bin"));
        assert!(!explicit);
    }

    #[test]
    fn resolve_file_output_is_explicit_and_verbatim() {
        let (path, explicit) = resolve_output(
            Some(Path::new("/tmp/renamed.bin")),
            "a.bin",
            Path::new("/work"),
        );
        assert_eq!(path, Path::new("/tmp/renamed.bin"));
        assert!(explicit);
    }

    #[test]
    fn dedup_leaves_a_free_path_alone() {
        let path = Path::new("/dl/a.bin");
        assert_eq!(dedup_path_with(path, |_| false), path);
    }

    #[test]
    fn dedup_numbers_before_the_extension() {
        let taken = [PathBuf::from("/dl/a.bin"), PathBuf::from("/dl/a (1).bin")];
        let free = dedup_path_with(Path::new("/dl/a.bin"), |p| taken.contains(&p.to_path_buf()));
        assert_eq!(free, Path::new("/dl/a (2).bin"));
    }

    #[test]
    fn dedup_numbers_an_extensionless_name_at_the_end() {
        let taken = [PathBuf::from("/dl/README")];
        let free = dedup_path_with(Path::new("/dl/README"), |p| {
            taken.contains(&p.to_path_buf())
        });
        assert_eq!(free, Path::new("/dl/README (1)"));
    }

    #[test]
    fn save_to_dir_creates_writes_and_dedups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("sub");
        let first = save_to_dir(&target, "a.bin", b"one").expect("first save");
        assert_eq!(first, target.join("a.bin"));
        assert_eq!(std::fs::read(&first).expect("read first"), b"one");
        // A second download of the same name lands beside it, not over it.
        let second = save_to_dir(&target, "a.bin", b"two").expect("second save");
        assert_eq!(second, target.join("a (1).bin"));
        assert_eq!(std::fs::read(&first).expect("first intact"), b"one");
        assert_eq!(std::fs::read(&second).expect("read second"), b"two");
    }
}
