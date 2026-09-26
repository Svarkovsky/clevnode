//! The non-interactive sinks and the pieces the TUI shares with them: link
//! resolution, the fetch/parse step, and the one-shot print and download paths.
//!
//! Navigation semantics mirror NomadNet's `Browser.py` (`retrieve_url`,
//! `handle_link`, and `DEFAULT_PATH`): a link target is resolved against the
//! destination of the page currently in view and its preset query fields are
//! carried as `var_*` request variables. Wire-level URL parsing is delegated to
//! [`crate::url::parse_url`], the single source of truth for URL grammar. The
//! interactive browser itself lives in [`crate::tui`].

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use leviculum_micron::MicronDocument;

use crate::color::ColorDepth;
use crate::fetch::{FetchError, Session};
use crate::render::{render_with_options, RenderedLink, RenderedPage};
use crate::url::{parse_url, Target, UrlError};

/// Split a `#anchor` suffix off a link target, returning the base target and the
/// anchor name (if any, and non-empty).
fn split_anchor(target: &str) -> (&str, Option<String>) {
    match target.split_once('#') {
        Some((base, anchor)) if !anchor.is_empty() => (base, Some(anchor.to_string())),
        _ => (target, None),
    }
}

/// The preset (`key=value`) field components of a link, reconstructed into the
/// backtick blob [`parse_url`] understands. Valueless components are form-field
/// placeholders (interactive input, a v1 stub) and are dropped here.
fn preset_field_blob(link: &RenderedLink) -> String {
    link.fields
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("|")
}

/// Resolve a followed link into a fetch [`Target`] and any `#anchor`.
///
/// The link target is resolved against `current_dest` (for `:/page/x.mu`
/// same-destination links) and its preset fields are carried through
/// [`parse_url`], keeping URL grammar in one place.
pub fn resolve_link(
    link: &RenderedLink,
    current_dest: Option<[u8; 16]>,
) -> Result<(Target, Option<String>), UrlError> {
    let (base, anchor) = split_anchor(&link.target);
    let blob = preset_field_blob(link);
    let url = if blob.is_empty() {
        base.to_string()
    } else {
        format!("{base}`{blob}")
    };
    let target = parse_url(&url, current_dest)?;
    Ok((target, anchor))
}

/// Options controlling how pages are fetched and rendered.
#[derive(Debug, Clone, Copy)]
pub struct BrowserOptions {
    /// Render width in columns.
    pub width: usize,
    /// Strip ANSI colour from the rendered output.
    pub no_color: bool,
    /// The terminal colour depth: 24-bit true colour, or the downgraded
    /// xterm-256 palette for terminals without true-colour support.
    pub depth: ColorDepth,
    /// Per-request fetch timeout.
    pub timeout: Duration,
    /// Whether inline images are suppressed (`--images off`). Only the
    /// interactive TUI acts on it; the print sink never drew pictures.
    pub no_images: bool,
    /// How many bytes of fetched pictures to keep for revisits
    /// (`--image-cache`). Zero disables the cache.
    pub image_cache_bytes: u64,
}

/// Fetch a page, parse it, and return the parsed document. The raw bytes are
/// decoded as UTF-8 lossily so a page with stray bytes still renders.
///
/// Public so the TUI shell (`main`) can fetch a page once and lay it out into a
/// [`crate::tui::Model`], sharing the exact fetch/parse path the print sink uses.
pub async fn fetch_document(
    session: &mut Session,
    target: &Target,
    timeout: Duration,
) -> Result<MicronDocument, FetchError> {
    let bytes = session.fetch(target, timeout).await?;
    let source = String::from_utf8_lossy(&bytes);
    Ok(leviculum_micron::parse(&source))
}

/// A short, glanceable page title for the TUI frame: the node's discovered
/// display name when known, else the short dest hex. The page path is not part
/// of the title; it appears once, in the address shown beside it.
pub fn page_title(name: Option<&str>, dest_hash: &[u8; 16], _path: &str) -> String {
    match name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => short_dest_hex(dest_hash),
    }
}

/// A short, glanceable form of a destination hash: the first 8 hex characters
/// (4 bytes) followed by an ellipsis, e.g. `a8d24177…`.
fn short_dest_hex(dest_hash: &[u8; 16]) -> String {
    let mut s = String::with_capacity(9);
    for byte in &dest_hash[..4] {
        s.push_str(&format!("{byte:02x}"));
    }
    s.push('…');
    s
}

/// Write a rendered page's laid-out text to `out`.
///
/// Links carry no visible `[N]` marker and there is no trailing `Links:` legend:
/// a link is set apart in the page text by its underline + colour alone (the
/// interactive TUI adds focus, hints and mouse hit-testing on top of that).
fn write_page<W: Write>(out: &mut W, page: &RenderedPage) -> std::io::Result<()> {
    out.write_all(page.text.as_bytes())
}

/// A one-shot fetch/render/print, for `--print` mode and non-tty stdout.
///
/// Returns `Ok(())` on success; the caller maps a [`FetchError`] to an exit
/// code. The page is resolved against no current destination, so the URL must
/// name a destination.
pub async fn print_once<W: Write>(
    out: &mut W,
    session: &mut Session,
    target: &Target,
    opts: &BrowserOptions,
) -> Result<(), FetchError> {
    let doc = fetch_document(session, target, opts.timeout).await?;
    let page = render_with_options(&doc, opts.width, opts.no_color, opts.depth);
    let _ = write_page(out, &page);
    Ok(())
}

/// Download a `/file/` target, save it to disk, and print the save line
/// (`saved <bytes> bytes to <abspath>`).
///
/// The filename prefers the sanitised server-sent metadata name, falling back
/// to the URL basename (see [`crate::download::choose_name`]). `output` is the
/// CLI's `--output`: an existing directory (or a path spelled with a trailing
/// `/`) receives the file under that name, any other path names the exact file
/// to write (overwriting it), and `None` saves the name into the current
/// working directory. A computed (non-explicit)
/// path that already exists is de-duplicated with ` (1)`, ` (2)`, ... before
/// the extension, so nothing is overwritten silently.
pub async fn download_once<W: Write>(
    out: &mut W,
    session: &mut Session,
    target: &Target,
    output: Option<&Path>,
    timeout: Duration,
) -> Result<(), FetchError> {
    let (bytes, server_name) = session.download_file(target, timeout).await?;
    let name = crate::download::choose_name(server_name.as_deref(), &target.path);
    let cwd = std::env::current_dir().map_err(|e| FetchError::Save(e.to_string()))?;
    let (path, explicit) = crate::download::resolve_output(output, &name, &cwd);
    let path = if explicit {
        path
    } else {
        crate::download::dedup_path(&path)
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| FetchError::Save(e.to_string()))?;
        }
    }
    std::fs::write(&path, &bytes).map_err(|e| FetchError::Save(e.to_string()))?;
    let shown = std::path::absolute(&path).unwrap_or(path);
    let _ = writeln!(out, "saved {} bytes to {}", bytes.len(), shown.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_HEX: &str = "0123456789abcdef0123456789abcdef";
    const HASH_BYTES: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];
    const OTHER_HASH: [u8; 16] = [0xaa; 16];

    fn link(target: &str, fields: Vec<(&str, &str)>) -> RenderedLink {
        RenderedLink {
            index: 1,
            label: "L".to_string(),
            target: target.to_string(),
            fields: fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..RenderedLink::default()
        }
    }

    // --- link resolution ---

    #[test]
    fn resolve_absolute_link_ignores_current_dest() {
        let l = link(&format!("{HASH_HEX}:/page/next.mu"), vec![]);
        let (t, anchor) = resolve_link(&l, Some(OTHER_HASH)).unwrap();
        assert_eq!(t.dest_hash, HASH_BYTES);
        assert_eq!(t.path, "/page/next.mu");
        assert!(anchor.is_none());
    }

    #[test]
    fn resolve_relative_link_uses_current_dest() {
        // A same-destination link (leading `:`) resolves against current_dest.
        let l = link(":/page/rel.mu", vec![]);
        let (t, _) = resolve_link(&l, Some(OTHER_HASH)).unwrap();
        assert_eq!(t.dest_hash, OTHER_HASH);
        assert_eq!(t.path, "/page/rel.mu");
    }

    #[test]
    fn resolve_carries_preset_fields_and_drops_form_placeholders() {
        let l = link(
            &format!("{HASH_HEX}:/page/x.mu"),
            vec![("g", "reticulum"), ("ref", "")],
        );
        let (t, _) = resolve_link(&l, None).unwrap();
        // The preset field is carried with the var_ prefix; the valueless
        // form-field reference is dropped here (the TUI collects its current
        // value as a `field_` entry at submit time instead).
        assert_eq!(
            t.fields,
            vec![("var_g".to_string(), "reticulum".to_string())]
        );
    }

    #[test]
    fn resolve_splits_anchor_off_the_target() {
        let l = link(&format!("{HASH_HEX}:/page/x.mu#section2"), vec![]);
        let (t, anchor) = resolve_link(&l, None).unwrap();
        assert_eq!(t.path, "/page/x.mu");
        assert_eq!(anchor.as_deref(), Some("section2"));
    }

    #[test]
    fn resolve_anchor_with_preset_fields() {
        let l = link(&format!("{HASH_HEX}:/page/x.mu#top"), vec![("a", "1")]);
        let (t, anchor) = resolve_link(&l, None).unwrap();
        assert_eq!(t.path, "/page/x.mu");
        assert_eq!(t.fields, vec![("var_a".to_string(), "1".to_string())]);
        assert_eq!(anchor.as_deref(), Some("top"));
    }

    #[test]
    fn resolve_relative_without_current_is_malformed() {
        let l = link(":/page/x.mu", vec![]);
        assert!(resolve_link(&l, None).is_err());
    }

    // --- page output ---

    fn render_to_string<F>(f: F) -> String
    where
        F: FnOnce(&mut Vec<u8>) -> std::io::Result<()>,
    {
        let mut buf = Vec::new();
        f(&mut buf).expect("write");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn write_page_emits_text_with_no_legend() {
        let page = RenderedPage {
            text: "body text\n".to_string(),
            links: vec![RenderedLink {
                index: 1,
                label: "L1".to_string(),
                target: "/page/1.mu".to_string(),
                fields: Vec::new(),
                ..RenderedLink::default()
            }],
        };
        let out = render_to_string(|w| write_page(w, &page));
        assert_eq!(
            out, "body text\n",
            "write_page should emit only the page text"
        );
        // No trailing `Links:` legend and no legend entry.
        assert!(!out.contains("Links:"), "legend block leaked: {out:?}");
        assert!(
            !out.contains("-> /page/1.mu"),
            "legend entry leaked: {out:?}"
        );
    }
}
