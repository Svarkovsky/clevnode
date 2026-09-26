//! `lnomad`: a terminal browser for NomadNet micron pages.
//!
//! Phase 3 provides the [`render`] module: it turns a parsed
//! [`leviculum_micron::MicronDocument`] into ANSI terminal output for a given
//! width and collects the page's links for navigation. It is pure and testable,
//! with no networking. Phase 4 adds the fetch layer and the interactive CLI to
//! this same crate.
//!
//! ```
//! use leviculum_micron::parse;
//! use lnomad::render::render;
//!
//! let doc = parse(">> Title\n\nSome `!bold`! text and a `[link`:/page/x].");
//! let page = render(&doc, 80);
//! assert!(page.text.contains("Title"));
//! assert_eq!(page.links.len(), 1);
//! ```

//! Phase 4a adds the network backend: [`url`] parses NomadNet page URLs and
//! [`fetch`] connects to a running `lnsd`/`rnsd` shared instance and retrieves a
//! page's bytes over the RNS request/response path.

//! Phase 4b adds the [`browser`] module: link resolution, the fetch/parse step
//! and the one-shot print and download sinks the `lnomad` binary drives when it
//! is not running the interactive [`tui`].

pub mod bookmarks;
pub mod browser;
pub mod cli;
pub mod color;
pub mod discovery;
pub mod download;
pub mod fetch;
pub mod identify;
pub mod identity;
pub mod image;
pub mod image_cache;
pub mod page_cache;
pub mod render;
pub mod theme;
pub mod tui;
pub mod url;

pub use bookmarks::{Bookmark, Bookmarks};
pub use browser::{resolve_link, BrowserOptions};
pub use cli::{resolve_args, Mode};
pub use color::{resolve_depth, rgb_to_ansi256, ColorDepth, ColorFlag};
pub use discovery::{
    is_nomad_node_announce, name_hash_is_nomad_node, nomad_node_name_hash, DiscoveredNode,
    NomadNodeRegistry,
};
pub use fetch::{FetchError, Session};
pub use identify::IdentifyStore;
pub use page_cache::{CacheEntry, PageCache};
pub use render::{
    layout, render_with_options, RLine, RStyle, RenderedLink, RenderedPage, StyledChar,
};
pub use theme::{resolve_theme, Bg, Theme, ThemeFlag};
pub use tui::{
    run_tui, to_ratatui_text, update, view, AppEvent, Model, TerminalGuard, TerminalOps,
};
pub use url::{classify_link, parse_url, LinkKind, Target, UrlError};
