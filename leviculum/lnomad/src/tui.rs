//! The ratatui/crossterm terminal UI: the testable core and the thin IO shell.
//!
//! The design is TEA (The Elm Architecture) so the interesting logic is pure
//! and driven from headless tests:
//!
//! - [`Model`] is the whole UI state (the laid-out page, its links, the title,
//!   the browsing history, the address editor, the terminal size, a quit flag).
//! - [`update`] folds an [`AppEvent`] into the model and returns a list of
//!   [`Effect`]s for the IO shell to run. It does no IO, so a test feeds it
//!   events and inspects the model and the returned effects directly. Navigation
//!   is expressed purely: an address entry or a history motion yields
//!   [`Effect::Navigate`]; a cancel yields [`Effect::Cancel`]. A completed fetch
//!   arrives back as [`AppEvent::PageLoaded`] / [`AppEvent::LoadFailed`], also
//!   folded by `update`.
//! - [`view`] draws the model into a ratatui [`Frame`]: a one-row top-bar (page
//!   title, address, and a right-aligned status cluster), the scrollable
//!   content, and a one-row status bar. A test renders it into a
//!   [`ratatui::backend::TestBackend`] and asserts on the resulting buffer.
//! - [`run_tui`] is the only part that touches a real terminal and the network.
//!   It owns the [`Session`], runs the initial fetch and every subsequent
//!   navigation as a background task whose result rejoins the `tokio::select!`
//!   loop, so the UI stays live (spinner animates, keys and the address input
//!   work) while a page loads. A slow or failed fetch never blocks the UI; a
//!   cancel drops the in-flight task and returns to the current page.
//!
//! Phase 3 adds the top-bar, the history stack, async navigation with a loading
//! spinner and `Esc`/`Ctrl-g` cancel, and a `tui-input` address editor.
//!
//! Phase 4 adds the navigation layer over the rendered page: [`visible_links`]
//! maps each on-screen link to a screen rectangle, reused by Tab focus (with
//! auto-scroll), `f` hint mode (vimium-style labels over links and controls),
//! left-click hit-testing, and mouse-hover. The focused or hovered link's target
//! shows in the status bar. Links no longer carry a visible `[N]` marker.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Margin, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RtLine, Span as RtSpan, Text};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::{Frame, Terminal};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};
use ratatui_image::{FilterType, Resize};
use tokio::sync::{mpsc, Mutex};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use leviculum_micron::MicronDocument;

use crate::bookmarks::{Bookmark, Bookmarks};
use crate::browser::{self, BrowserOptions};
use crate::color::{rgb_to_ansi256, ColorDepth};
use crate::discovery::{DiscoveredNode, NomadNodeRegistry};
use crate::fetch::Session;
use crate::image::{Backend, ImageSlot, Probe, SlotState, MAX_INLINE_IMAGES};
use crate::image_cache::ImageCache;
use crate::page_cache::{CacheEntry, PageCache};
use crate::render::{
    insert_image_rows, layout_blocks, standalone_links, FieldValue, RLine, RStyle, RenderedField,
    RenderedLink,
};
use crate::theme::{resolve_theme, Bg, Theme, ThemeFlag};
use crate::url::{classify_link, parse_url, LinkKind, Target, UrlError, DEFAULT_PATH};
use leviculum_micron::FieldKind;

/// The number of columns reserved on the right for the scrollbar.
const SCROLLBAR_COLS: u16 = 1;
/// How many lines one mouse-wheel notch scrolls.
const WHEEL_STEP: usize = 3;
/// Rows in the fixed top-bar. A single identity row: the page title, the
/// address, and a right-aligned status cluster.
const TOPBAR_ROWS: u16 = 1;
/// Rows in the fixed status bar.
const STATUS_ROWS: u16 = 1;
/// Total chrome rows: top-bar plus status bar. The content viewport is the
/// terminal height minus this.
const CHROME_ROWS: u16 = TOPBAR_ROWS + STATUS_ROWS;
/// Columns of blank gap between the top-bar's left identity (title / address)
/// and its right-aligned status cluster.
const CTRL_GAP: u16 = 2;
/// Spinner animation cadence while a fetch is in flight, in milliseconds. Also
/// the cadence at which an idle toast is aged towards its expiry.
const SPINNER_TICK_MS: u64 = 120;
/// How long a toast stays up before auto-dismissing, in milliseconds.
const TOAST_LIFETIME_MS: u64 = 4000;
/// Toast lifetime expressed in animation ticks (`TOAST_LIFETIME_MS` over the
/// tick cadence), so expiry is a pure count of ticks.
const TOAST_TICKS: u64 = TOAST_LIFETIME_MS / SPINNER_TICK_MS;

/// The status-cluster glyph shown when the current page is bookmarked, and when
/// it is not. The star is a click target that toggles the bookmark.
const STAR_ON: &str = "★";
const STAR_OFF: &str = "☆";

/// The subtle right-aligned status-cluster glyph shown when the current page was
/// served from the in-RAM page cache rather than a fresh fetch.
const CACHED_GLYPH: &str = "⚡";

/// The status-cluster glyph shown (with the fingerprint prefix) while the
/// session identifies to the current page's node. An ordinary wide emoji (no
/// VS16), so `UnicodeWidthStr` scores it 2 columns like the terminal draws it.
const IDENTIFY_GLYPH: &str = "\u{1F511}";

/// The instructional sentence shown in the footer while hint mode is active.
const HINT_SENTENCE: &str = "type a hint label or link text";

/// The home-row alphabet hint labels are drawn from (vimium-style).
const HINT_ALPHABET: [char; 9] = ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l'];

/// A ratatui [`Color`] from a theme RGB triple.
fn rgb(c: (u8, u8, u8)) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// The style the fixed top-bar and status bar are filled with, full width, so
/// they stand out from the content. With colour, the theme's chrome background
/// and foreground (a muted slate on dark, a light bar on light); under
/// `no_color`, the REVERSE modifier so the bars still delineate without any
/// colour. Shared by both bars for a consistent look.
fn chrome_style(no_color: bool, theme: Theme) -> Style {
    if no_color {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
            .fg(rgb(theme.chrome_fg()))
            .bg(rgb(theme.chrome_bg()))
    }
}

/// The foreground for the bright chrome text drawn over the bars: the status-bar
/// key-hints and the AVAILABLE top-bar controls. Uses the theme's bright chrome
/// foreground (~4.5:1 on the chrome background) instead of `Modifier::DIM`,
/// which halved the foreground and read as too low contrast. Under `no_color`
/// it returns the plain style so the reverse-video chrome fill shows through.
fn chrome_text_style(no_color: bool, theme: Theme) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(rgb(theme.chrome_fg()))
    }
}

/// The foreground for UNAVAILABLE top-bar controls: the theme's muted chrome
/// foreground, a distinct-but-readable grey (~3:1 on the chrome background) that
/// reads as "disabled" without the illegibility of `Modifier::DIM`. Under
/// `no_color` it returns the plain style so the reverse-video fill shows
/// through (no DIM under reverse).
fn chrome_muted_style(no_color: bool, theme: Theme) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(rgb(theme.chrome_muted_fg()))
    }
}

/// The content layout width for a terminal `cols` wide: full width minus the
/// scrollbar column, never below 1 so wrapping stays well defined.
fn content_width(cols: u16) -> usize {
    (cols.saturating_sub(SCROLLBAR_COLS) as usize).max(1)
}

/// The content viewport height for a terminal `rows` tall: the height left after
/// the fixed top-bar and status bar.
fn content_height(rows: u16) -> usize {
    rows.saturating_sub(CHROME_ROWS) as usize
}

/// The column width a terminal actually PAINTS for `s`, used to fit text into a
/// box. This is `unicode-width` PLUS one column for every `U+FE0F` VARIATION
/// SELECTOR-16: `unicode-width` scores VS16 as 0 and a text-default base (e.g.
/// `U+262F ☯`) as 1, but a terminal draws the resulting emoji-presentation
/// sequence 2 columns wide. Over-counting is safe when fitting — we truncate a
/// column early; under-counting overflows a border. ZWJ sequences are already
/// over-counted by `unicode-width`, which is likewise safe, so they are left be.
///
/// This is deliberately a DIFFERENT notion of width from the plain
/// `unicode-width` used for cell POSITIONS (`visible_links`, `visible_fields`,
/// the search-match columns): those must keep agreeing with ratatui's own
/// `unicode-width` cell layout for hit-testing. `fit_width` measures what the
/// TERMINAL paints; the plain width measures what ratatui ADDRESSES.
fn fit_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0) + usize::from(c == '\u{FE0F}'))
        .sum()
}

/// Truncate `s` to at most `cols` display columns, ending with a `…` when
/// anything is cut. Uses [`fit_width`] so an emoji-presentation sequence counts
/// as the two columns a terminal paints, so the returned string never spills
/// over a `cols`-wide overlay border. A zero budget yields an empty string.
fn truncate_to_cols(s: &str, cols: usize) -> String {
    if cols == 0 {
        return String::new();
    }
    if fit_width(s) <= cols {
        return s.to_string();
    }
    // Reserve one column for the ellipsis, then take as many full characters as
    // fit. A wide char that would straddle the budget is dropped whole, so the
    // result never exceeds `cols`.
    let budget = cols - 1;
    let mut out = String::new();
    let mut width = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0) + usize::from(c == '\u{FE0F}');
        if width + w > budget {
            break;
        }
        out.push(c);
        width += w;
    }
    out.push('…');
    out
}

/// Split the frame into the three fixed regions: top-bar, content, status.
fn regions(area: Rect) -> [Rect; 3] {
    let parts = Layout::vertical([
        Constraint::Length(TOPBAR_ROWS),
        Constraint::Min(0),
        Constraint::Length(STATUS_ROWS),
    ])
    .split(area);
    [parts[0], parts[1], parts[2]]
}

/// A vertical scroll motion, resolved against the current viewport height in
/// [`Model::apply_scroll`]. Bound to both vi and emacs keys plus the wheel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollCmd {
    /// Up one line.
    LineUp,
    /// Down one line.
    LineDown,
    /// Up half a viewport.
    HalfPageUp,
    /// Down half a viewport.
    HalfPageDown,
    /// Up one full viewport.
    PageUp,
    /// Down one full viewport.
    PageDown,
    /// To the very top.
    Top,
    /// To the very bottom.
    Bottom,
}

/// The single scroll rule shared by every scrollable window — the content body
/// and both overlays (help, places), and any future overlay. Resolve `cmd`
/// against the current offset `cur`, given the `viewport` height in rows and
/// `max` (the largest valid offset, i.e. `content.saturating_sub(viewport)`),
/// clamped into `0..=max`. A half page is `viewport / 2`. Keeping one pure
/// function means all windows scroll with identical semantics; a new overlay
/// gets correct scrolling by calling this with its own viewport and max.
pub fn scrolled(cur: usize, cmd: ScrollCmd, viewport: usize, max: usize) -> usize {
    let vp = viewport.max(1);
    let half = (vp / 2).max(1);
    let next = match cmd {
        ScrollCmd::LineUp => cur.saturating_sub(1),
        ScrollCmd::LineDown => cur.saturating_add(1),
        ScrollCmd::HalfPageUp => cur.saturating_sub(half),
        ScrollCmd::HalfPageDown => cur.saturating_add(half),
        ScrollCmd::PageUp => cur.saturating_sub(vp),
        ScrollCmd::PageDown => cur.saturating_add(vp),
        ScrollCmd::Top => 0,
        ScrollCmd::Bottom => max,
    };
    next.min(max)
}

/// The interactive UI mode: normal browsing or entering an address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Scrolling and navigation keys are live.
    #[default]
    Browse,
    /// The top-bar address editor has focus.
    Address,
    /// Hint mode (`f`): each visible link and top-bar control wears a label the
    /// reader types to jump to it.
    Hint,
    /// In-page search (`/`): a one-line query editor; committing highlights all
    /// matches and jumps to the first.
    Search,
    /// A form field has focus and is being edited: typing edits a text field,
    /// Space toggles a checkbox/radio, Tab/Shift-Tab move to the next/previous
    /// interactive element, Esc returns to browsing.
    Field,
}

/// A focusable interactive element on the page: a link or a form field, keyed by
/// its 1-based index in its own kind's space. The Tab focus cycle walks links and
/// fields together in document order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The page link with this 1-based index.
    Link(usize),
    /// The form field with this 1-based index.
    Field(usize),
}

/// A form field currently on screen, with its screen rectangle. Produced by
/// [`visible_fields`] and consumed by hit-testing, focus and the input overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleField {
    /// The field's 1-based [`RenderedField::index`].
    pub index: usize,
    /// The field name (form key).
    pub name: String,
    /// The field kind (text, checkbox, radio).
    pub kind: FieldKind,
    /// The screen row (absolute, below the top-bar) the field sits on.
    pub row: u16,
    /// The first screen column of the field widget.
    pub col_start: u16,
    /// One past the last screen column of the field widget.
    pub col_end: u16,
}

/// The editable state of one form field: the source of truth for what the reader
/// has typed or toggled. Persists across a re-layout (resize / theme toggle);
/// rebuilt only when a new page loads.
#[derive(Clone, Debug)]
pub struct FieldEdit {
    /// The field name (submitted as `field_<name>`).
    pub name: String,
    /// The field kind.
    pub kind: FieldKind,
    /// The text editor for a text field (unused for checkbox/radio).
    pub input: Input,
    /// The current checked state of a checkbox/radio (unused for text).
    pub checked: bool,
    /// The submit value of a checkbox/radio: its explicit value, or its label
    /// when none was given (unused for text; a text field submits its editor
    /// contents).
    pub value: String,
}

/// One in-page search hit: a column span on a single laid-out page line. Column
/// indices are 0-based DISPLAY columns (the screen column the renderer draws the
/// cell at), not character indices: a wide char (emoji, CJK) advances two
/// columns, so `col_start`/`col_end` line up with the cells ratatui actually
/// paints in the content body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    /// 0-based index of the laid-out [`RLine`] the match sits on.
    pub line_idx: usize,
    /// 0-based first matched display column on that line.
    pub col_start: usize,
    /// 0-based display column one past the last matched column
    /// (`col_start..col_end`).
    pub col_end: usize,
}

/// Every case-insensitive substring match of `query` over the rendered page
/// text (the [`RLine`] cells, i.e. exactly what the reader sees, post-wrap).
///
/// Matching is per line and non-overlapping: after a hit the scan resumes at
/// its end, so adjacent hits (`aa` in `aaaa` -> two matches) are found but
/// overlapping ones (`aa` starting one column apart) are not. An empty query
/// yields no matches. Comparison is case-insensitive per character while keeping
/// a strict one-cell-per-column mapping, so the returned rects line up with the
/// laid-out cells regardless of case.
pub fn find_matches(page: &[RLine], query: &str) -> Vec<Match> {
    let needle: Vec<char> = query.chars().collect();
    let n = needle.len();
    if n == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (line_idx, line) in page.iter().enumerate() {
        let hay: Vec<char> = line.cells.iter().map(|c| c.ch).collect();
        if hay.len() < n {
            continue;
        }
        // `col_at[i]` is the DISPLAY column where char `i` starts; `col_at[len]`
        // is the column one past the line. Matching stays char-based, but the
        // emitted spans are display columns so they align with the renderer.
        let mut col_at = Vec::with_capacity(hay.len() + 1);
        let mut col = 0usize;
        for ch in &hay {
            col_at.push(col);
            col += UnicodeWidthChar::width(*ch).unwrap_or(0);
        }
        col_at.push(col);
        let mut i = 0;
        while i + n <= hay.len() {
            if (0..n).all(|k| chars_eq_ci(hay[i + k], needle[k])) {
                out.push(Match {
                    line_idx,
                    col_start: col_at[i],
                    col_end: col_at[i + n],
                });
                i += n;
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Case-insensitive equality of two characters, without changing the column
/// count (each side stays one character), so match columns keep aligning to
/// laid-out cells.
fn chars_eq_ci(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// One of the clickable top-bar controls, recorded with its rect so mouse
/// clicks and hint labels can be hit-tested against it. The navigation trio
/// (back / forward / reload) lives in the footer, not here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// The address slot: a click opens the address editor.
    Address,
    /// The bookmark star: a click toggles the current page's bookmark.
    Bookmark,
}

/// A top-bar control's screen rectangle, for mouse hit-testing in a later phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRect {
    /// Which control this rectangle belongs to.
    pub control: Control,
    /// Where it sits on screen.
    pub rect: Rect,
}

/// A link that is currently on screen, with its screen rectangle. Produced by
/// [`visible_links`] and consumed by hit-testing, hint labelling and focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleLink {
    /// The link's 1-based [`RenderedLink::index`](crate::render::RenderedLink).
    pub index: usize,
    /// The screen row (absolute, below the top-bar) the link sits on.
    pub row: u16,
    /// The first screen column of the clickable span.
    pub col_start: u16,
    /// One past the last screen column of the clickable span.
    pub col_end: u16,
}

/// What a hint label jumps to: a page link, a form field or a top-bar control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintTarget {
    /// The page link with this 1-based index.
    Link(usize),
    /// The form field with this 1-based index.
    Field(usize),
    /// A top-bar control.
    Control(Control),
}

/// A hint badge: its typed label, what it activates, and where it renders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    /// The label the reader types to activate this hint.
    pub label: String,
    /// What activating the hint does.
    pub target: HintTarget,
    /// The screen row the badge renders on.
    pub row: u16,
    /// The first screen column the badge renders at.
    pub col: u16,
}

/// The browsing history: a linear stack of visited targets and a cursor into it.
///
/// [`visit`](History::visit) truncates any forward entries (a new navigation
/// from the middle discards the forward branch) and pushes. [`back`](History::back)
/// and [`forward`](History::forward) move the cursor and return the target now
/// under it, so the shell can re-fetch it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Visited targets in order.
    pub stack: Vec<Target>,
    /// Index of the current entry in `stack` (meaningful only when non-empty).
    pub idx: usize,
}

impl History {
    /// Record a fresh navigation: drop any forward entries, then push `target`
    /// and point the cursor at it.
    pub fn visit(&mut self, target: Target) {
        if self.stack.is_empty() {
            self.stack.push(target);
            self.idx = 0;
        } else {
            self.stack.truncate(self.idx + 1);
            self.stack.push(target);
            self.idx = self.stack.len() - 1;
        }
    }

    /// Move the cursor to an existing index (a back/forward navigation that has
    /// now loaded). Clamped to the stack; a no-op on an empty stack.
    pub fn goto(&mut self, idx: usize) {
        if !self.stack.is_empty() {
            self.idx = idx.min(self.stack.len() - 1);
        }
    }

    /// The target currently under the cursor, if any.
    pub fn current(&self) -> Option<&Target> {
        self.stack.get(self.idx)
    }

    /// Whether a back step is possible.
    pub fn can_back(&self) -> bool {
        self.idx > 0
    }

    /// Whether a forward step is possible.
    pub fn can_forward(&self) -> bool {
        self.idx + 1 < self.stack.len()
    }

    /// Step back and return the target now under the cursor, or `None` at the
    /// start.
    pub fn back(&mut self) -> Option<&Target> {
        if self.can_back() {
            self.idx -= 1;
            self.stack.get(self.idx)
        } else {
            None
        }
    }

    /// Step forward and return the target now under the cursor, or `None` at the
    /// end.
    pub fn forward(&mut self) -> Option<&Target> {
        if self.can_forward() {
            self.idx += 1;
            self.stack.get(self.idx)
        } else {
            None
        }
    }
}

/// How a completed navigation should update the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryAction {
    /// A fresh navigation: [`History::visit`] the target on success.
    Push,
    /// A back/forward/reload navigation: [`History::goto`] this index on success.
    Goto(usize),
}

/// One inline image to fetch: which link it belongs to and where it lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRequest {
    /// The 1-based link index, which every image event refers back to.
    pub link: usize,
    /// The file target to download.
    pub target: Target,
}

/// A navigation in flight: the target being fetched and how history records it
/// once the page loads.
#[derive(Clone, Debug)]
pub struct Pending {
    /// The target being fetched.
    pub target: Target,
    /// How to fold it into history on success.
    pub action: HistoryAction,
}

/// A side effect [`update`] asks the IO shell to perform. Keeping these out of
/// `update` lets navigation logic be unit-tested without any IO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Start (or restart) a fetch for this target.
    Navigate(Target),
    /// Download this `/file/` target: the IO shell fetches the raw bytes,
    /// saves them into the download directory, and folds the outcome back as
    /// [`AppEvent::DownloadDone`] / [`AppEvent::DownloadFailed`]. A download is
    /// not a navigation: the page and history stay put.
    Download(Target),
    /// Open this external URL in the user's default handler (the IO shell runs
    /// `open::that`). Only whitelisted safe schemes ever reach this effect.
    OpenExternal(String),
    /// Fetch the current page's inline images, in this order. The IO shell
    /// works the list one at a time — a link carries one transfer at a time
    /// anyway — and folds each outcome back as an image event. The page is
    /// already on screen when this starts, so nothing waits for a picture.
    /// Starting one drops whatever image queue was running, as do
    /// [`Navigate`](Effect::Navigate) and [`Cancel`](Effect::Cancel): the
    /// transfers still to come belong to a page nobody is looking at.
    FetchImages(Vec<ImageRequest>),
    /// Save an already-fetched inline image to the download directory, with no
    /// second transfer.
    SaveImage {
        /// The link index whose picture to save.
        link: usize,
    },
    /// Save an already-fetched inline image and hand the file to the platform
    /// handler (`xdg-open` on Linux).
    OpenImage {
        /// The link index whose picture to open.
        link: usize,
    },
    /// Cancel the in-flight fetch and stay on the current page.
    Cancel,
    /// Copy this text to the system clipboard (the IO shell writes it as an
    /// OSC 52 sequence to the terminal).
    Copy(String),
    /// Persist the model's bookmarks to disk (the IO shell writes the TOML).
    SaveBookmarks,
    /// Turn identifying to `dest` on or off (the IO shell calls
    /// [`Session::set_identify`], which persists the decision and drops the
    /// reused link). Always emitted BEFORE the accompanying reload's
    /// [`Navigate`](Effect::Navigate), so the fresh link is built under the new
    /// decision.
    SetIdentify {
        /// The destination the decision applies to.
        dest: [u8; 16],
        /// `true` to identify, `false` to go anonymous.
        on: bool,
    },
    /// Quit the UI.
    Quit,
}

/// Whether a transient toast carries an error or a neutral/success note. Drives
/// the toast's accent colour and glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    /// A failure the reader should notice (a bad link, a failed fetch): drawn in
    /// an attention colour with a `⚠` prefix.
    Error,
    /// A neutral confirmation (copied, bookmarked, cancelled): drawn in a calm
    /// colour with a `✓` prefix.
    Info,
}

/// A transient, auto-dismissing notification floated over the content. Replaces
/// the old sticky status-bar messages, leaving the status bar for the key-hints
/// (or the loading spinner during a fetch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    /// Whether this is an error or a neutral note.
    pub kind: ToastKind,
    /// The message text; carries the offending target for error toasts so it is
    /// self-describing.
    pub text: String,
    /// The [`Model::now_tick`] value when the toast was shown, so its age (and
    /// thus expiry) is a pure function of the monotonic tick counter and needs no
    /// wall clock to unit-test.
    pub shown_at: u64,
}

/// The complete UI state. Pure data: [`update`] is the only thing that mutates
/// it, and it never performs IO.
///
/// The model owns the parsed `doc` and its current layout `width` so a resize
/// can re-wrap: [`relayout`](Model::relayout) recomputes `page`/`links` from the
/// stored document at a new width.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// The parsed document, kept so a resize can re-wrap it to a new width.
    pub doc: MicronDocument,
    /// The width the page is currently laid out at.
    pub width: usize,
    /// The page laid out into target-agnostic styled lines.
    pub page: Vec<RLine>,
    /// The page's links, with their laid-out positions for hit-testing.
    pub links: Vec<RenderedLink>,
    /// The title shown in the top-bar (node name or short hash, plus path).
    pub title: String,
    /// The last known terminal size, as `(cols, rows)`.
    pub size: (u16, u16),
    /// Index of the top visible line in `page`. Always clamped to the page.
    pub scroll: usize,
    /// The browsing history and its cursor.
    pub history: History,
    /// The destination currently displayed, for the same-destination URL form.
    pub current_dest: Option<[u8; 16]>,
    /// A navigation in flight, or `None` when idle. `Some` means "loading".
    pub pending: Option<Pending>,
    /// A file download in flight: the filename being fetched, or `None` when
    /// idle. Drives the loading indicator like [`pending`](Model::pending)
    /// does, but its completion raises a toast instead of replacing the page.
    pub download: Option<String>,
    /// The `#anchor` the in-flight navigation should scroll to once its page
    /// loads, or `None`. Kept beside `pending` so the anchor survives the async
    /// round-trip and is resolved against the freshly-loaded page's lines.
    pub pending_anchor: Option<String>,
    /// The current interaction mode (browse, address entry, hint, or search).
    pub mode: Mode,
    /// The address-bar editor.
    pub input: Input,
    /// The in-page search query editor (`/`).
    pub search_input: Input,
    /// The current query's matches over the laid-out page, highlighted while set.
    pub matches: Vec<Match>,
    /// Index into `matches` of the current match (the one scrolled to and drawn
    /// with the stronger highlight), or `None` when there is no current match.
    pub current_match: Option<usize>,
    /// Block index -> first laid-out line, so a `#anchor` (stored as a block
    /// index) resolves to a page line. Recomputed on every relayout.
    pub block_lines: Vec<usize>,
    /// The link the focus cursor is on (Tab navigation), 1-based, or `None`.
    /// Mutually exclusive with [`field_focus`](Model::field_focus): at most one
    /// interactive element is focused at a time.
    pub focus: Option<usize>,
    /// The form field's laid-out positions and metadata, refreshed on every
    /// relayout from the parsed document and the current `field_state`.
    pub field_defs: Vec<RenderedField>,
    /// The editable field store (typed text / toggled state), the source of
    /// truth for rendering and submission. Indexed by a field's 1-based index
    /// minus one; preserved across a relayout, rebuilt on a page load.
    pub field_state: Vec<FieldEdit>,
    /// The form field the focus cursor is on (Tab navigation / a click), 1-based,
    /// or `None`. Set exactly when [`mode`](Model::mode) is [`Mode::Field`].
    pub field_focus: Option<usize>,
    /// The link the mouse is hovering over, 1-based, or `None`.
    pub hover: Option<usize>,
    /// The footer button the mouse is hovering over, as an index into the current
    /// mode's footer item list, or `None`. Highlights that button so it reads as
    /// clickable. Mutually exclusive with a content-link [`hover`](Model::hover).
    pub footer_hover: Option<usize>,
    /// The characters typed so far in hint mode, narrowing the visible labels.
    pub hint_input: String,
    /// The active transient toast (fetch error, "copied", "bookmarked", ...), or
    /// `None`. Rendered as a floating overlay, not in the status bar; auto-expires
    /// and is cleared on the next key event. See [`Model::set_toast`].
    pub toast: Option<Toast>,
    /// A monotonic tick counter, advanced once per animation [`AppEvent::Tick`].
    /// The toast's age is measured against it, so expiry is testable by advancing
    /// this counter without any real time passing.
    pub now_tick: u64,
    /// The loading spinner's animation tick: advances once per redraw while a
    /// fetch is in flight, driving both the circling glyph and the shimmering
    /// rainbow hue. See `spinner_span`.
    pub spin: usize,
    /// Whether the keybinding help overlay is shown.
    pub show_help: bool,
    /// Whether colour is suppressed (NO_COLOR / non-tty): the chrome bars fall
    /// back to reverse video instead of a coloured background.
    pub no_color: bool,
    /// The terminal colour depth. Under [`ColorDepth::Ansi256`] the rendered
    /// buffer's 24-bit RGB colours are downgraded to the nearest xterm-256 index
    /// in a single post-pass at the end of [`view`]; [`ColorDepth::Truecolor`]
    /// leaves them as authored. Subordinate to [`no_color`](Model::no_color),
    /// which suppresses the downgrade along with all colour.
    pub depth: ColorDepth,
    /// The active light/dark theme: the accents baked into `page` and the chrome
    /// colours the view draws. Toggled at runtime with `t`.
    pub theme: Theme,
    /// NomadNet nodes discovered from announces seen on the session's event
    /// stream, folded in from [`AppEvent::NodeDiscovered`]. Listed in the places
    /// panel's "Discovered nodes" section.
    pub node_registry: NomadNodeRegistry,
    /// The persisted bookmarks, loaded at startup and saved on change.
    pub bookmarks: Bookmarks,
    /// Where the bookmarks are persisted, or `None` when no config dir is
    /// resolvable (the browser then runs without persistence).
    pub bookmarks_path: Option<PathBuf>,
    /// Whether the places panel (bookmarks + discovered nodes) overlay is shown.
    pub show_places: bool,
    /// The selected row in the places panel, an index into [`places`].
    pub places_sel: usize,
    /// The places overlay's VIEW offset: the first entry-line shown when the
    /// panel is taller than the terminal. Distinct from `places_sel` (the
    /// selection); the view is scrolled the minimum needed to keep the selected
    /// row visible. Reset to 0 when the panel opens.
    pub places_scroll: usize,
    /// The places-panel entry the mouse is hovering over, an index into
    /// [`places`], or `None`. Highlights that row so it reads as clickable; kept
    /// distinct from the selection (`places_sel`) so the two highlights differ.
    pub places_hover: Option<usize>,
    /// The one-slot undo stash: the bookmark most recently deleted from the
    /// places panel, restored by `u` (`undo_delete_bookmark`). A new deletion
    /// overwrites it (only one level of undo).
    pub last_deleted_bookmark: Option<DeletedBookmark>,
    /// The help overlay's VIEW offset: the first help line shown when the help
    /// is taller than the terminal. Reset to 0 when the help opens.
    pub help_scroll: usize,
    /// The in-RAM LRU cache of recently viewed pages, keyed by fetch target. A
    /// revisit (including back/forward) renders instantly from here; a reload
    /// bypasses it and form submits are never stored. See [`PageCache`].
    pub page_cache: PageCache,
    /// Whether the page currently shown was served from `page_cache` rather
    /// than a fresh fetch. Drives the subtle "cached" top-bar marker; cleared on
    /// the next fresh load.
    pub cached_view: bool,
    /// lnomad's own identity fingerprint as lowercase hex. Stable for the whole
    /// session, so the IO shell sets it ONCE at model construction from
    /// [`Session::fingerprint_hex`]. Empty by default (unit tests set it
    /// directly), in which case the top-bar identity marker stays hidden.
    pub fingerprint: String,
    /// Whether the session identifies to [`current_dest`](Model::current_dest).
    /// The `i` toggle flips it optimistically so the UI updates immediately;
    /// the IO shell's refresh after each fresh load is authoritative, and a
    /// cache-served navigation restores it from `identify_known`
    /// (Model::identify_known). Drives the top-bar marker and the footer label.
    pub identifying: bool,
    /// The destinations the model has learned the session identifies to: every
    /// toggle and every fresh load's authoritative answer is folded in via
    /// [`note_identify`](Model::note_identify). A cache-served navigation (which
    /// asks the session nothing) reads [`identifying`](Model::identifying) from
    /// here; the page cache only holds pages fetched this run, so the answer was
    /// always learned first.
    pub identify_known: std::collections::BTreeSet<[u8; 16]>,
    /// The current page's inline images, one slot per standalone image link,
    /// in link order. Rebuilt on every page load; drives both the rows the
    /// relayout reserves and what is drawn into them.
    pub images: Vec<ImageSlot>,
    /// Whether inline images are suppressed (`--images off`). Named like
    /// [`no_color`](Model::no_color), and for the same reason: the default of
    /// a `bool` field is the ordinary behaviour, not the opt-out. With it set
    /// an image link stays an ordinary link, which is exactly what NomadNet
    /// shows.
    pub no_images: bool,
    /// Set once the user has asked to quit; the IO loop breaks on it.
    pub quit: bool,
}

impl Model {
    /// Lay a parsed document out at `width` and build a model from it, keeping
    /// the document so a later resize can re-wrap it.
    pub fn from_document(
        doc: &MicronDocument,
        width: usize,
        title: impl Into<String>,
        size: (u16, u16),
    ) -> Self {
        let theme = Theme::default();
        let (page, links, block_lines, field_defs) =
            layout_blocks(doc, width, theme, &[] as &[FieldValue]);
        let mut model = Self {
            doc: doc.clone(),
            width,
            page,
            links,
            block_lines,
            field_defs,
            title: title.into(),
            size,
            theme,
            ..Self::default()
        };
        model.rebuild_field_state();
        model
    }

    /// Rebuild the editable field store from the freshly laid-out field defs
    /// (their parsed prefill). Called on construction and on every page load;
    /// NOT on a resize/theme relayout, where the store must be preserved.
    pub fn rebuild_field_state(&mut self) {
        self.field_state = self
            .field_defs
            .iter()
            .map(|f| FieldEdit {
                name: f.name.clone(),
                kind: f.kind,
                input: Input::new(f.value.clone()),
                checked: f.checked,
                value: f.value.clone(),
            })
            .collect();
    }

    /// The current field values, for feeding a relayout so the laid-out widgets
    /// reflect what the reader has typed / toggled. Positionally indexed by a
    /// field's 1-based index minus one.
    fn field_values(&self) -> Vec<FieldValue> {
        self.field_state
            .iter()
            .enumerate()
            .map(|(i, fe)| {
                // Slide the text window so the caret stays inside the box: a
                // message longer than the field must still be typeable. The
                // widths come from the previous layout and never change between
                // relayouts, so reading them here is sound.
                let w = self.field_defs.get(i).map_or(1, |d| d.width.max(1));
                FieldValue {
                    text: fe.input.value().to_string(),
                    checked: fe.checked,
                    scroll: fe.input.visual_scroll(w.saturating_sub(1)),
                }
            })
            .collect()
    }

    /// Re-wrap the stored document to `width` under the current theme, replacing
    /// `page`/`links`. The caller is responsible for re-clamping `scroll`
    /// afterwards. Also used to re-lay-out in place after a theme toggle.
    pub fn relayout(&mut self, width: usize) {
        self.width = width;
        let values = self.field_values();
        let (mut page, mut links, mut block_lines, mut field_defs) =
            layout_blocks(&self.doc, width, self.theme, &values);
        // Inline pictures are reserved AFTER the text is laid out: their height
        // is not known until the image has arrived and been decoded, which is
        // long after the page first appeared.
        let reserved: Vec<(usize, u16)> = self
            .images
            .iter()
            .map(|slot| (slot.link, slot.state.rows()))
            .collect();
        insert_image_rows(
            &mut page,
            &mut links,
            &mut field_defs,
            &mut block_lines,
            &reserved,
        );
        self.page = page;
        self.links = links;
        self.block_lines = block_lines;
        self.field_defs = field_defs;
    }

    /// The slot for a link index, if that link is an inline picture.
    pub fn image_slot(&self, link: usize) -> Option<&ImageSlot> {
        self.images.iter().find(|slot| slot.link == link)
    }

    /// Fold an image state change in and re-lay the page out, since the rows a
    /// slot reserves depend on its state.
    fn set_image_state(&mut self, link: usize, state: SlotState) {
        let Some(slot) = self.images.iter_mut().find(|slot| slot.link == link) else {
            return;
        };
        if slot.state == state {
            return;
        }
        slot.state = state;
        let (w, vp) = (self.width, self.viewport());
        self.relayout(w);
        self.clamp_scroll(vp);
    }

    /// Toggle between the dark and light theme, re-laying the page out so the
    /// new accents take effect and re-clamping the scroll offset.
    pub fn toggle_theme(&mut self) {
        self.theme = self.theme.toggle();
        let (w, vp) = (self.width, self.viewport());
        self.relayout(w);
        self.clamp_scroll(vp);
    }

    /// The number of page lines visible at once: the terminal height minus the
    /// fixed top-bar and status bar.
    pub fn viewport(&self) -> usize {
        content_height(self.size.1)
    }

    /// Whether a fetch or a file download is currently in flight.
    pub fn is_loading(&self) -> bool {
        self.pending.is_some() || self.download.is_some()
    }

    /// Show a transient toast, stamped with the current tick so it can expire.
    /// Every transient message routes through here instead of the status bar.
    pub fn set_toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast {
            kind,
            text: text.into(),
            shown_at: self.now_tick,
        });
    }

    /// Dismiss any active toast (a navigation or key event clears it early).
    pub fn dismiss_toast(&mut self) {
        self.toast = None;
    }

    /// Record whether the session identifies to `dest`: fold it into
    /// [`identify_known`](Model::identify_known) and, when `dest` is the current
    /// page's node, refresh the display flag. Called by the `i` toggle and by
    /// the IO shell with each fresh load's authoritative answer.
    pub fn note_identify(&mut self, dest: [u8; 16], on: bool) {
        if on {
            self.identify_known.insert(dest);
        } else {
            self.identify_known.remove(&dest);
        }
        if self.current_dest == Some(dest) {
            self.identifying = on;
        }
    }

    /// Advance the toast towards expiry: clear it once it has been up for at
    /// least [`TOAST_TICKS`] ticks. Called on every animation tick.
    fn expire_toast(&mut self) {
        if let Some(toast) = &self.toast {
            if self.now_tick.saturating_sub(toast.shown_at) >= TOAST_TICKS {
                self.toast = None;
            }
        }
    }

    /// Whether the UI needs the periodic tick: a fetch spinner is animating, or a
    /// toast is up and must age towards its auto-dismiss.
    pub fn needs_tick(&self) -> bool {
        self.is_loading() || self.toast.is_some()
    }

    /// The largest valid `scroll` for a given viewport: the last position where
    /// the final page line still sits at the bottom of the viewport.
    pub fn max_scroll(&self, viewport: usize) -> usize {
        self.page.len().saturating_sub(viewport)
    }

    /// Clamp `scroll` into `[0, max_scroll(viewport)]` (e.g. after a re-wrap).
    pub fn clamp_scroll(&mut self, viewport: usize) {
        self.scroll = self.scroll.min(self.max_scroll(viewport));
    }

    /// Apply a scroll command against `viewport`, clamped to the page. Never
    /// under- or overflows, even when the page is shorter than the viewport.
    pub fn apply_scroll(&mut self, cmd: ScrollCmd, viewport: usize) {
        let max = self.max_scroll(viewport);
        self.scroll = scrolled(self.scroll, cmd, viewport, max);
    }

    /// Stash the current page's scroll offset into its cache entry (when the
    /// page is cached), so a later revisit restores where the reader was. A
    /// no-op when the current target is not cached (e.g. a form-submit result,
    /// or the very first page still loading).
    pub fn stash_scroll(&mut self) {
        if let Some(target) = self.history.current().cloned() {
            self.page_cache.set_scroll(&target, self.scroll);
        }
    }

    /// Scroll down by `n` lines (mouse wheel), clamped to the page.
    pub fn scroll_lines_down(&mut self, n: usize, viewport: usize) {
        self.scroll = self.scroll.saturating_add(n).min(self.max_scroll(viewport));
    }

    /// Scroll up by `n` lines (mouse wheel), clamped at the top.
    pub fn scroll_lines_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// The frame rectangle implied by the model's last known size, so the pure
    /// layout helpers ([`visible_links`], [`hints`]) can run without a live frame.
    fn frame_area(&self) -> Rect {
        Rect::new(0, 0, self.size.0, self.size.1)
    }
}

/// The links currently on screen, each with its absolute screen rectangle.
///
/// A link on content line `link.line` is visible when
/// `scroll <= link.line < scroll + viewport`; it then sits at screen row
/// `TOPBAR_ROWS + (link.line - scroll)`, columns `[col_start, col_end)` (the
/// content body starts at column 0). Off-viewport links are excluded. Pure, so
/// hit-testing, hints and focus all share one source of truth.
pub fn visible_links(model: &Model) -> Vec<VisibleLink> {
    let viewport = model.viewport();
    let scroll = model.scroll;
    let mut out = Vec::new();
    for link in &model.links {
        if link.line < scroll || link.line >= scroll + viewport {
            continue;
        }
        let row = TOPBAR_ROWS + (link.line - scroll) as u16;
        out.push(VisibleLink {
            index: link.index,
            row,
            col_start: link.col_start as u16,
            col_end: link.col_end as u16,
        });
    }
    out
}

/// The form fields currently on screen, each with its absolute screen rectangle,
/// mirroring [`visible_links`]. A field on content line `field.line` is visible
/// when `scroll <= field.line < scroll + viewport`; it then sits at screen row
/// `TOPBAR_ROWS + (field.line - scroll)`, columns `[col_start, col_end)`. Off-
/// viewport fields are excluded. Pure, so hit-testing, focus and the input
/// overlay share one source of truth.
pub fn visible_fields(model: &Model) -> Vec<VisibleField> {
    let viewport = model.viewport();
    let scroll = model.scroll;
    let mut out = Vec::new();
    for field in &model.field_defs {
        if field.line < scroll || field.line >= scroll + viewport {
            continue;
        }
        let row = TOPBAR_ROWS + (field.line - scroll) as u16;
        out.push(VisibleField {
            index: field.index,
            name: field.name.clone(),
            kind: field.kind,
            row,
            col_start: field.col_start as u16,
            col_end: field.col_end as u16,
        });
    }
    out
}

/// Resolve an anchor name to the page line to scroll to: the first laid-out
/// [`RLine`] of the block the anchor marks. The parser records anchors as block
/// indices (`doc.anchors`); [`Model::block_lines`] maps a block index to its
/// first line. `None` when the anchor is unknown or its block laid out no line.
pub fn anchor_line(model: &Model, anchor: &str) -> Option<usize> {
    let block = *model.doc.anchors.get(anchor)?;
    model.block_lines.get(block).copied()
}

/// Generate `n` unique hint labels from the home-row alphabet: single characters
/// while they suffice, two-character combinations once there are more targets
/// than the alphabet holds.
pub fn hint_labels(n: usize) -> Vec<String> {
    if n <= HINT_ALPHABET.len() {
        return HINT_ALPHABET
            .iter()
            .take(n)
            .map(|c| c.to_string())
            .collect();
    }
    let mut out = Vec::with_capacity(n);
    'outer: for &a in &HINT_ALPHABET {
        for &b in &HINT_ALPHABET {
            out.push(format!("{a}{b}"));
            if out.len() == n {
                break 'outer;
            }
        }
    }
    out
}

/// The hint badges for the current frame: one per visible link (in reading
/// order), then one per visible form field, then one per top-bar control, each
/// wearing a unique label.
pub fn hints(model: &Model) -> Vec<Hint> {
    let [topbar, _content, _status] = regions(model.frame_area());
    let mut slots: Vec<(HintTarget, u16, u16)> = Vec::new();
    for vl in visible_links(model) {
        slots.push((HintTarget::Link(vl.index), vl.row, vl.col_start));
    }
    for vf in visible_fields(model) {
        slots.push((HintTarget::Field(vf.index), vf.row, vf.col_start));
    }
    for cr in top_bar_controls(model, topbar) {
        slots.push((HintTarget::Control(cr.control), cr.rect.y, cr.rect.x));
    }
    hint_labels(slots.len())
        .into_iter()
        .zip(slots)
        .map(|(label, (target, row, col))| Hint {
            label,
            target,
            row,
            col,
        })
        .collect()
}

/// Whether `hint` matches the typed `input`: its label starts with the prefix,
/// or (for a link) its visible text contains the input as a case-insensitive
/// substring. `input` is assumed already lowercased.
fn hint_matches(model: &Model, hint: &Hint, input: &str) -> bool {
    if hint.label.starts_with(input) {
        return true;
    }
    if let HintTarget::Link(idx) = hint.target {
        if let Some(link) = model.links.iter().find(|l| l.index == idx) {
            return link.label.to_lowercase().contains(input);
        }
    }
    false
}

/// A UI input event, decoupled from crossterm so [`update`] is trivially
/// testable and the event source can be swapped later.
#[derive(Clone, Debug)]
pub enum AppEvent {
    /// A key was pressed.
    Key(KeyEvent),
    /// A mouse event occurred (wheel scrolling, left-click to follow a link or
    /// activate a top-bar control, and move-to-hover).
    Mouse(MouseEvent),
    /// The terminal was resized to `(cols, rows)`.
    Resize(u16, u16),
    /// An explicit request to quit.
    Quit,
    /// A navigation completed: the parsed document and its resolved title.
    PageLoaded {
        /// The freshly parsed page.
        doc: MicronDocument,
        /// The resolved top-bar title for it.
        title: String,
    },
    /// A navigation failed with a human-readable message.
    LoadFailed(String),
    /// A file download completed and was saved to disk.
    DownloadDone {
        /// The filename it was saved as.
        name: String,
        /// The number of bytes saved.
        bytes: usize,
        /// The directory it was saved into, for the toast.
        dir: String,
    },
    /// A file download failed with a human-readable message.
    DownloadFailed(String),
    /// An inline image's transfer started.
    ImageLoading {
        /// The link index it belongs to.
        link: usize,
    },
    /// An inline image arrived, decoded, and is ready to be drawn at `cells`.
    /// The encoded protocol itself lives in the IO shell's `ImageStore`; the
    /// model only needs the size, so it stays pure data.
    ImageReady {
        /// The link index it belongs to.
        link: usize,
        /// The cell size it will be drawn at.
        cells: (u16, u16),
        /// What the payload turned out to be.
        probe: Probe,
    },
    /// An inline image arrived but will not be drawn: the terminal cannot show
    /// pictures, or the payload did not decode. The reader gets a status line
    /// instead, which is the whole point of keeping this distinct from a
    /// failure.
    ImageText {
        /// The link index it belongs to.
        link: usize,
        /// What the payload turned out to be, if it could be read at all.
        probe: Option<Probe>,
        /// Why there is no picture.
        reason: String,
    },
    /// An inline image's transfer failed.
    ImageFailed {
        /// The link index it belongs to.
        link: usize,
        /// What went wrong.
        reason: String,
    },
    /// A NomadNet node announce was seen on the session's event stream; fold it
    /// into the model's discovered-node registry.
    NodeDiscovered(DiscoveredNode),
    /// The spinner animation tick, delivered while a fetch is in flight.
    Tick,
}

/// Fold an event into the model, returning any [`Effect`]s for the IO shell.
/// Pure and IO-free.
pub fn update(model: &mut Model, event: AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::Quit => {
            model.quit = true;
            vec![Effect::Quit]
        }
        AppEvent::Tick => {
            model.spin = model.spin.wrapping_add(1);
            model.now_tick = model.now_tick.wrapping_add(1);
            model.expire_toast();
            Vec::new()
        }
        AppEvent::PageLoaded { doc, title } => apply_loaded(model, doc, title),
        AppEvent::LoadFailed(msg) => {
            model.pending = None;
            model.set_toast(ToastKind::Error, msg);
            Vec::new()
        }
        AppEvent::DownloadDone { name, bytes, dir } => {
            model.download = None;
            model.set_toast(
                ToastKind::Info,
                format!("downloaded {name} ({bytes} bytes) to {dir}"),
            );
            Vec::new()
        }
        AppEvent::DownloadFailed(msg) => {
            model.download = None;
            model.set_toast(ToastKind::Error, msg);
            Vec::new()
        }
        AppEvent::ImageLoading { link } => {
            model.set_image_state(link, SlotState::Loading);
            Vec::new()
        }
        AppEvent::ImageReady { link, cells, probe } => {
            model.set_image_state(link, SlotState::Ready { cells, probe });
            Vec::new()
        }
        AppEvent::ImageText {
            link,
            probe,
            reason,
        } => {
            model.set_image_state(link, SlotState::Text { probe, reason });
            Vec::new()
        }
        AppEvent::ImageFailed { link, reason } => {
            model.set_image_state(link, SlotState::Failed(reason));
            Vec::new()
        }
        AppEvent::NodeDiscovered(node) => {
            model.node_registry.upsert_node(&node);
            Vec::new()
        }
        AppEvent::Resize(cols, rows) => {
            model.size = (cols, rows);
            model.relayout(content_width(cols));
            let vp = model.viewport();
            model.clamp_scroll(vp);
            Vec::new()
        }
        AppEvent::Mouse(mouse) => {
            let vp = model.viewport();
            match mouse.kind {
                // The wheel scrolls whichever window owns the screen: an open
                // overlay takes it over the page underneath.
                MouseEventKind::ScrollDown => {
                    if model.show_help {
                        let (_, max) = help_scroll_geometry(model);
                        model.help_scroll = model.help_scroll.saturating_add(WHEEL_STEP).min(max);
                    } else if model.show_places {
                        // The wheel is a VIEW control, not a cursor: it moves the
                        // scroll offset and leaves the selection alone, so no
                        // entry is ever skipped past.
                        wheel_places_view(model, ScrollCmd::LineDown);
                    } else {
                        model.scroll_lines_down(WHEEL_STEP, vp);
                    }
                    Vec::new()
                }
                MouseEventKind::ScrollUp => {
                    if model.show_help {
                        model.help_scroll = model.help_scroll.saturating_sub(WHEEL_STEP);
                    } else if model.show_places {
                        wheel_places_view(model, ScrollCmd::LineUp);
                    } else {
                        model.scroll_lines_up(WHEEL_STEP);
                    }
                    Vec::new()
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    handle_click(model, mouse.column, mouse.row)
                }
                // Mouse side buttons: Back (button 8) and Forward (button 9)
                // drive the same history navigation as Alt-Left / Alt-Right,
                // and are a no-op when there is no history in that direction.
                MouseEventKind::Down(MouseButton::Back) => go_back(model),
                MouseEventKind::Down(MouseButton::Forward) => go_forward(model),
                MouseEventKind::Moved => {
                    update_hover(model, mouse.column, mouse.row);
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }
        AppEvent::Key(key) => update_key(model, key),
    }
}

/// Fold a completed page load: replace the document, relayout, reset scroll, set
/// the title, and record the navigation in history per its pending action.
fn apply_loaded(model: &mut Model, doc: MicronDocument, title: String) -> Vec<Effect> {
    let pending = model.pending.take();
    let anchor = model.pending_anchor.take();
    // A fresh fetch, not a cache hit: clear the "cached" marker.
    model.cached_view = false;
    model.doc = doc;
    // A fresh document replaces the old fields: clear the store so the relayout
    // lays them out from the new prefill, then rebuild the editable store from it.
    model.field_state.clear();
    model.relayout(content_width(model.size.0));
    model.rebuild_field_state();
    model.scroll = 0;
    model.title = title;
    model.dismiss_toast();
    // A fresh page invalidates any focus/hover cursor into the old link set, and
    // any search match highlights against the old page.
    model.focus = None;
    model.field_focus = None;
    model.mode = Mode::Browse;
    model.hover = None;
    model.matches.clear();
    model.current_match = None;
    let loaded_target = pending.map(|pending| {
        model.current_dest = Some(pending.target.dest_hash);
        match pending.action {
            HistoryAction::Push => model.history.visit(pending.target.clone()),
            HistoryAction::Goto(idx) => model.history.goto(idx),
        }
        pending.target
    });
    // The loaded page's node may differ from the previous one: refresh the
    // identify display flag from what the model has learned so far. The IO
    // shell follows up with the session's authoritative answer for this load.
    if let Some(dest) = model.current_dest {
        model.identifying = model.identify_known.contains(&dest);
    }
    // A followed `#anchor` scrolls its block's first line to the top; an unknown
    // anchor falls back to the top of the page with a toast note.
    if let Some(name) = anchor {
        match anchor_line(model, &name) {
            Some(line) => {
                let vp = model.viewport();
                model.scroll = line.min(model.max_scroll(vp));
            }
            None => model.set_toast(ToastKind::Error, format!("anchor #{name} not found")),
        }
    }
    // Cache the freshly loaded page (overwriting any prior entry for this target,
    // as a reload does) so a later revisit renders instantly. A non-idempotent
    // form submit is never cached.
    if let Some(target) = loaded_target {
        if !is_form_submit(&target) {
            model.page_cache.insert(
                target,
                CacheEntry {
                    doc: model.doc.clone(),
                    title: model.title.clone(),
                    scroll: model.scroll,
                },
            );
        }
    }
    plan_images(model)
}

/// Work out which of the page's links are pictures, install a slot for each,
/// and ask the IO shell to fetch them.
///
/// Called after every load, cached or fresh: the page cache holds documents,
/// not payloads, so a revisit re-fetches its pictures. The list is capped at
/// [`MAX_INLINE_IMAGES`]; the rest keep their slots (so the reader is told they
/// exist) but reserve no rows and fetch nothing until followed.
fn plan_images(model: &mut Model) -> Vec<Effect> {
    model.images.clear();
    if model.no_images {
        return Vec::new();
    }
    let standalone = standalone_links(&model.page, &model.links);
    let mut requests = Vec::new();
    let mut slots = Vec::new();
    for index in standalone {
        let Some(link) = model.links.iter().find(|l| l.index == index) else {
            continue;
        };
        let path = crate::image::link_path(&link.target);
        if !crate::image::is_image_path(path) {
            continue;
        }
        let Some(name) = crate::image::file_name(path).map(str::to_string) else {
            continue;
        };
        if requests.len() >= MAX_INLINE_IMAGES {
            slots.push(ImageSlot {
                link: index,
                name,
                state: SlotState::TooMany,
            });
            continue;
        }
        let Ok((target, _)) = browser::resolve_link(link, model.current_dest) else {
            continue;
        };
        requests.push(ImageRequest {
            link: index,
            target,
        });
        slots.push(ImageSlot {
            link: index,
            name,
            state: SlotState::Queued,
        });
    }
    model.images = slots;
    if model.images.is_empty() {
        return Vec::new();
    }
    // The slots reserve a status row each, so re-lay the page out before the
    // first frame: the reader sees where the pictures will be while they load.
    let (w, vp) = (model.width, model.viewport());
    model.relayout(w);
    model.clamp_scroll(vp);
    match requests.is_empty() {
        true => Vec::new(),
        false => vec![Effect::FetchImages(requests)],
    }
}

/// The places panel's placeholder under "Discovered nodes" while the registry
/// is empty.
const NO_NODES_YET: &str = "  (listening for announces…)";

/// The start screen's micron source: what a reader sees when `lnomad` is
/// started without a URL. A real micron document rather than hand-drawn chrome,
/// so it goes through the same parse/layout/render path as a fetched page and
/// re-wraps on a resize or a theme change for free.
const START_PAGE: &str = "\
>lnomad

Terminal browser for NomadNet pages.

No page is open. Node discovery runs in the background, so nodes appear in the \
places panel as their announces arrive.

`!d`! places   `!:`! URL   `!?`! help   `!q`! quit";

/// The title shown for the start screen, in place of a page's node name.
const START_TITLE: &str = "lnomad";

/// Show the start screen and open the places panel over it.
///
/// The page is applied through the ordinary load path with no pending
/// navigation, so nothing is fetched, no history entry is pushed and no current
/// destination is set: the top bar carries no address, `back` / `forward` /
/// `reload` stay unavailable, and the first real navigation starts the history
/// from scratch.
fn show_start_screen(model: &mut Model) {
    apply_loaded(
        model,
        leviculum_micron::parse(START_PAGE),
        START_TITLE.to_string(),
    );
    model.show_places = true;
}

/// Split a trailing `#anchor` off a target's path, returning the anchor-free
/// target and the anchor name (when non-empty). The initial URL keeps its
/// `#anchor` inside `path` after [`parse_url`](crate::url::parse_url); stripping
/// it here keeps the fetched path clean and lets the load handler scroll to it.
fn split_path_anchor(mut target: Target) -> (Target, Option<String>) {
    let split = target
        .path
        .split_once('#')
        .map(|(base, anchor)| (base.to_string(), anchor.to_string()));
    match split {
        Some((base, anchor)) if !anchor.is_empty() => {
            target.path = base;
            (target, Some(anchor))
        }
        _ => (target, None),
    }
}

/// Fold a key press, routed by mode. Returns any effects.
fn update_key(model: &mut Model, key: KeyEvent) -> Vec<Effect> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Any key dismisses the current toast; a handler below may raise a fresh one
    // for this very key (e.g. `m` -> "bookmarked"), which then wins.
    model.dismiss_toast();

    // Ctrl-C quits from any mode.
    if ctrl && key.code == KeyCode::Char('c') {
        model.quit = true;
        return vec![Effect::Quit];
    }

    // The help overlay swallows keys until dismissed. While it is open every
    // scroll key from `SCROLL_KEYS` moves the OVERLAY's view (via the shared
    // `scrolled` rule), not the page underneath.
    if model.show_help {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
        ) {
            model.show_help = false;
        } else if let Some(cmd) = key_to_scroll(&key) {
            let (viewport, max) = help_scroll_geometry(model);
            model.help_scroll = scrolled(model.help_scroll, cmd, viewport, max);
        }
        return Vec::new();
    }

    // The places panel is modal like the help overlay: it owns keys until closed.
    if model.show_places {
        return update_places_key(model, key);
    }

    match model.mode {
        Mode::Address => update_address_key(model, key),
        Mode::Hint => update_hint_key(model, key),
        Mode::Search => update_search_key(model, key),
        Mode::Field => update_field_key(model, key),
        Mode::Browse => update_browse_key(model, key, ctrl),
    }
}

/// Fold a key while a form field has focus. Tab/Shift-Tab move to the next/
/// previous interactive element; Esc leaves editing back to browse. A text field
/// takes the usual editing keys (typing, Backspace/Delete, Left/Right, Home/End)
/// and re-lays the page out so its widget reflects the new value; a checkbox/
/// radio toggles on Space (a radio deselects its group siblings). Any other key
/// is ignored, so field editing never leaks into browse hotkeys.
fn update_field_key(model: &mut Model, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Tab => {
            focus_next(model);
            return Vec::new();
        }
        KeyCode::BackTab => {
            focus_prev(model);
            return Vec::new();
        }
        KeyCode::Esc => {
            model.field_focus = None;
            model.mode = Mode::Browse;
            return Vec::new();
        }
        _ => {}
    }

    let Some(fi) = model.field_focus else {
        model.mode = Mode::Browse;
        return Vec::new();
    };
    let Some(fe) = model.field_state.get(fi - 1) else {
        return Vec::new();
    };

    match fe.kind {
        FieldKind::Text => {
            if matches!(
                key.code,
                KeyCode::Char(_)
                    | KeyCode::Backspace
                    | KeyCode::Delete
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
            ) {
                if let Some(fe) = model.field_state.get_mut(fi - 1) {
                    fe.input.handle_event(&Event::Key(key));
                }
                // The widget text changed width: re-lay the page out so the box
                // and every downstream position stay correct, keeping the scroll.
                let (w, vp) = (model.width, model.viewport());
                model.relayout(w);
                model.clamp_scroll(vp);
            }
        }
        FieldKind::Checkbox | FieldKind::Radio => {
            if key.code == KeyCode::Char(' ') {
                toggle_field(model, fi);
            }
        }
    }
    Vec::new()
}

/// Activate field `fi` (1-based): a checkbox toggles its checked state, a radio
/// selects itself within its group, a text field does nothing. Re-lays the page
/// out so the rendered widget shows the new state, keeping the scroll. The one
/// activation body shared by the Space key, a mouse click and a field hint.
fn toggle_field(model: &mut Model, fi: usize) {
    let Some(kind) = model.field_state.get(fi - 1).map(|fe| fe.kind) else {
        return;
    };
    match kind {
        FieldKind::Text => return,
        FieldKind::Checkbox => {
            if let Some(fe) = model.field_state.get_mut(fi - 1) {
                fe.checked = !fe.checked;
            }
        }
        FieldKind::Radio => select_radio(model, fi),
    }
    let (w, vp) = (model.width, model.viewport());
    model.relayout(w);
    model.clamp_scroll(vp);
}

/// Select radio field `fi` (1-based), deselecting every other radio sharing its
/// name (one selection per group), matching the reference radio-group behaviour.
fn select_radio(model: &mut Model, fi: usize) {
    let Some(name) = model.field_state.get(fi - 1).map(|fe| fe.name.clone()) else {
        return;
    };
    for fe in model.field_state.iter_mut() {
        if fe.kind == FieldKind::Radio && fe.name == name {
            fe.checked = false;
        }
    }
    if let Some(fe) = model.field_state.get_mut(fi - 1) {
        fe.checked = true;
    }
}

/// Fold a key while the in-page search editor has focus. `Enter` commits the
/// query (compute matches, jump to the first); `Esc` cancels and clears all
/// highlights; any other key edits the query.
fn update_search_key(model: &mut Model, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            exit_search(model);
            Vec::new()
        }
        KeyCode::Enter => {
            let query = model.search_input.value().to_string();
            model.mode = Mode::Browse;
            commit_search(model, &query);
            Vec::new()
        }
        _ => {
            model.search_input.handle_event(&Event::Key(key));
            Vec::new()
        }
    }
}

/// Fold a key while hint mode is active. A character narrows the visible labels
/// (by label prefix or link-text substring); a unique match follows the link,
/// activates the form field, or drives the control. `Esc` exits; a non-matching
/// key is ignored.
fn update_hint_key(model: &mut Model, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            exit_hint(model);
            Vec::new()
        }
        KeyCode::Backspace => {
            model.hint_input.pop();
            Vec::new()
        }
        KeyCode::Char(c) => {
            let mut candidate = model.hint_input.clone();
            candidate.push(c.to_ascii_lowercase());
            let all = hints(model);
            // An exact label match activates immediately (handles single-char
            // labels, where the label is also its own prefix).
            if let Some(hint) = all.iter().find(|h| h.label == candidate) {
                let target = hint.target;
                exit_hint(model);
                return activate_hint_target(model, target);
            }
            let matches: Vec<&Hint> = all
                .iter()
                .filter(|h| hint_matches(model, h, &candidate))
                .collect();
            match matches.as_slice() {
                // A key that matches nothing is ignored (not accepted).
                [] => Vec::new(),
                [only] => {
                    let target = only.target;
                    exit_hint(model);
                    activate_hint_target(model, target)
                }
                _ => {
                    model.hint_input = candidate;
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    }
}

/// Fold a key while the address editor has focus.
fn update_address_key(model: &mut Model, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            model.mode = Mode::Browse;
            model.input.reset();
            model.dismiss_toast();
            Vec::new()
        }
        KeyCode::Enter => {
            let raw = model.input.value().trim().to_string();
            match parse_url(&raw, model.current_dest) {
                Ok(target) => {
                    model.mode = Mode::Browse;
                    model.input.reset();
                    begin_navigation(model, target, HistoryAction::Push, None)
                }
                Err(err) => {
                    model.set_toast(ToastKind::Error, bad_url_note("bad URL", &raw, err));
                    Vec::new()
                }
            }
        }
        _ => {
            model.input.handle_event(&Event::Key(key));
            Vec::new()
        }
    }
}

/// Fold a key while browsing (scrolling, navigation, mode switches).
fn update_browse_key(model: &mut Model, key: KeyEvent, ctrl: bool) -> Vec<Effect> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if key.code == KeyCode::Char('q') {
        model.quit = true;
        return vec![Effect::Quit];
    }

    // Cancel an in-flight fetch or download (Esc or Ctrl-g), returning to the
    // current page.
    if model.is_loading() && (key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g')))
    {
        model.pending = None;
        model.download = None;
        model.set_toast(ToastKind::Info, "cancelled");
        return vec![Effect::Cancel];
    }

    // Ctrl-L: recompute the layout and redraw, keeping the (re-clamped) offset.
    if ctrl && key.code == KeyCode::Char('l') {
        let (w, vp) = (model.width, model.viewport());
        model.relayout(w);
        model.clamp_scroll(vp);
        return Vec::new();
    }

    // Enter address mode.
    if key.code == KeyCode::Char(':') {
        enter_address(model);
        return Vec::new();
    }

    // Enter hint mode (`f`).
    if key.code == KeyCode::Char('f') && !ctrl && !alt {
        enter_hint(model);
        return Vec::new();
    }

    // Enter in-page search mode (`/`).
    if key.code == KeyCode::Char('/') && !ctrl && !alt {
        enter_search(model);
        return Vec::new();
    }

    // Cycle the current search match: `n` next, `N` previous (both wrap), each
    // scrolling the current match into view. A no-op with no active matches.
    if key.code == KeyCode::Char('n') && !ctrl && !alt {
        next_match(model);
        return Vec::new();
    }
    if key.code == KeyCode::Char('N') && !ctrl && !alt {
        prev_match(model);
        return Vec::new();
    }

    // Toggle the light/dark theme (`t`), correcting a wrong auto-detection.
    if key.code == KeyCode::Char('t') && !ctrl && !alt {
        model.toggle_theme();
        return Vec::new();
    }

    // Toggle the places panel (`d`): bookmarks + discovered nodes.
    if key.code == KeyCode::Char('d') && !ctrl && !alt {
        toggle_places(model);
        return Vec::new();
    }

    // Bookmark (or un-bookmark) the current page (`m`).
    if key.code == KeyCode::Char('m') && !ctrl && !alt {
        return toggle_bookmark_current(model);
    }

    // Open the focused picture in the system image viewer (`o`): it is saved
    // to the download directory first, because the platform handler needs a
    // file and the picture has only ever existed in memory. A no-op unless an
    // arrived picture is focused.
    if key.code == KeyCode::Char('o') && !ctrl && !alt {
        if let Some(effect) = model
            .focus
            .and_then(|index| saved_image_effect(model, index, true))
        {
            model.dismiss_toast();
            return vec![effect];
        }
        return Vec::new();
    }

    // Yank the current page (or focused link) URL to the clipboard (`y`).
    if key.code == KeyCode::Char('y') && !ctrl && !alt {
        return yank_url(model);
    }

    // Toggle identifying to the current page's node (`i`), reloading it so the
    // decision takes effect on a fresh link.
    if key.code == KeyCode::Char('i') && !ctrl && !alt {
        return toggle_identify(model);
    }

    // Tab / Shift-Tab move the focus cursor across links; Enter follows it.
    if key.code == KeyCode::Tab {
        focus_next(model);
        return Vec::new();
    }
    if key.code == KeyCode::BackTab {
        focus_prev(model);
        return Vec::new();
    }
    if key.code == KeyCode::Enter {
        return match model.focus {
            Some(idx) => follow_link(model, idx),
            None => Vec::new(),
        };
    }

    // Toggle the help overlay.
    if key.code == KeyCode::Char('?') {
        model.show_help = true;
        model.help_scroll = 0;
        return Vec::new();
    }

    // Reload the current page (no history change). Bound to `R`, `Ctrl-R` and
    // `F5` to match browser muscle memory; bare lowercase `r` stays free, as a
    // mesh reload is expensive and single letters are reserved for cheap local
    // actions.
    if key.code == KeyCode::Char('R')
        || (ctrl && key.code == KeyCode::Char('r'))
        || key.code == KeyCode::F(5)
    {
        return reload_current(model);
    }

    // Back / forward (Alt-Left / Alt-Right): peek the target and re-fetch it,
    // moving the cursor only once it loads.
    if alt && key.code == KeyCode::Left {
        return go_back(model);
    }
    if alt && key.code == KeyCode::Right {
        return go_forward(model);
    }

    if let Some(cmd) = key_to_scroll(&key) {
        let vp = model.viewport();
        model.apply_scroll(cmd, vp);
    }
    Vec::new()
}

/// Enter address-entry mode with a cleared editor.
fn enter_address(model: &mut Model) {
    model.mode = Mode::Address;
    model.input.reset();
    model.dismiss_toast();
}

/// Enter hint mode with a cleared filter buffer.
fn enter_hint(model: &mut Model) {
    model.mode = Mode::Hint;
    model.hint_input.clear();
    model.dismiss_toast();
}

/// Leave hint mode, discarding the filter buffer.
fn exit_hint(model: &mut Model) {
    model.mode = Mode::Browse;
    model.hint_input.clear();
}

/// Enter in-page search mode with a cleared query editor, dropping any prior
/// match highlights so the reader starts from a clean slate.
fn enter_search(model: &mut Model) {
    model.mode = Mode::Search;
    model.search_input.reset();
    model.matches.clear();
    model.current_match = None;
    model.dismiss_toast();
}

/// Leave search mode (Esc): back to Browse, clearing the query and all match
/// highlights plus the current-match marker.
fn exit_search(model: &mut Model) {
    model.mode = Mode::Browse;
    model.search_input.reset();
    model.matches.clear();
    model.current_match = None;
    model.dismiss_toast();
}

/// Commit a search query: recompute matches over the current page, mark the
/// first as current and scroll it into view. An empty result clears the current
/// match and notes it in a toast.
fn commit_search(model: &mut Model, query: &str) {
    model.matches = find_matches(&model.page, query);
    if model.matches.is_empty() {
        model.current_match = None;
        model.set_toast(ToastKind::Info, format!("no matches for \"{query}\""));
    } else {
        model.current_match = Some(0);
        model.dismiss_toast();
        scroll_to_current_match(model);
    }
}

/// Advance the current match to the next one (wrapping) and scroll it into view.
/// A no-op when there are no matches.
fn next_match(model: &mut Model) {
    let n = model.matches.len();
    if n == 0 {
        return;
    }
    let cur = model.current_match.unwrap_or(0);
    model.current_match = Some((cur + 1) % n);
    scroll_to_current_match(model);
}

/// Move the current match to the previous one (wrapping) and scroll it into
/// view. A no-op when there are no matches.
fn prev_match(model: &mut Model) {
    let n = model.matches.len();
    if n == 0 {
        return;
    }
    let cur = model.current_match.unwrap_or(0);
    model.current_match = Some((cur + n - 1) % n);
    scroll_to_current_match(model);
}

/// Scroll so the current match's line is inside the viewport (minimal motion,
/// like Tab focus), clamped to the page. A no-op when there is no current match.
fn scroll_to_current_match(model: &mut Model) {
    let Some(ci) = model.current_match else {
        return;
    };
    let Some(m) = model.matches.get(ci) else {
        return;
    };
    let line = m.line_idx;
    let vp = model.viewport();
    if line < model.scroll {
        model.scroll = line;
    } else if vp > 0 && line >= model.scroll + vp {
        model.scroll = line + 1 - vp;
    }
    model.clamp_scroll(vp);
}

/// One row in the places panel: a saved bookmark or a discovered node. Built by
/// [`places`] as a flat, ordered list (bookmarks first, then discovered nodes)
/// that [`Model::places_sel`] indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// A saved bookmark: the URL to reopen and its captured title.
    Bookmark {
        /// The full page URL to reopen.
        url: String,
        /// The page title captured when it was bookmarked.
        title: String,
    },
    /// A discovered NomadNet node: its default page is opened on activation.
    Node {
        /// The node's destination hash.
        dest_hash: [u8; 16],
        /// The node's display name, if its announce carried one.
        name: Option<String>,
        /// The hop count from the most recent announce, if known.
        hops: Option<u32>,
        /// Unix seconds of the most recent announce.
        last_seen: u64,
    },
}

/// A bookmark deleted from the places panel, stashed for the one-slot `u` undo:
/// enough to rebuild it and put it back where it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedBookmark {
    /// The deleted bookmark's URL.
    pub url: String,
    /// The title it was saved under.
    pub title: String,
    /// Its position within the bookmarks list, so the undo restores it in
    /// place (clamped to the list length at restore time).
    pub index: usize,
}

/// The flat, ordered list of places: every bookmark (in insertion order), then
/// every discovered node (in discovery order). The single source of truth for
/// both the panel's rendering and its selection/activation, so a row index means
/// the same thing to each.
pub fn places(model: &Model) -> Vec<Place> {
    let mut out = Vec::new();
    for b in model.bookmarks.list() {
        out.push(Place::Bookmark {
            url: b.url.clone(),
            title: b.title.clone(),
        });
    }
    for n in model.node_registry.nodes() {
        out.push(Place::Node {
            dest_hash: n.dest_hash,
            name: n.name.clone(),
            hops: n.hops,
            last_seen: n.last_seen,
        });
    }
    out
}

/// Toggle the places panel. Opening it resets the selection to the first row and
/// clears any transient status.
fn toggle_places(model: &mut Model) {
    model.show_places = !model.show_places;
    if model.show_places {
        model.places_sel = 0;
        model.places_scroll = 0;
        model.places_hover = None;
        model.dismiss_toast();
    }
}

/// Close the places panel.
fn close_places(model: &mut Model) {
    model.show_places = false;
    model.places_hover = None;
}

/// Fold a key while the places panel is open. The up/down motions are the SAME
/// as the content scroll: [`key_to_scroll`] maps `j`/`k`/`Ctrl-n`/`Ctrl-p`/arrows
/// (line), `Ctrl-f`/`Ctrl-b`/`Ctrl-d`/`Ctrl-u` (page/half-page, clamped to the
/// ends), and `g`/`G`/Home/End (first/last), applied to the panel SELECTION
/// instead of the page. `Enter` opens the selected place, `x`/`Delete` delete
/// the selected bookmark, `u` undoes the last deletion, `Esc`/`d` close the
/// panel.
fn update_places_key(model: &mut Model, key: KeyEvent) -> Vec<Effect> {
    // Movement shares the content keymap, so the panel and the page scroll the
    // same way. This is checked first: `Ctrl-d` is a half-page motion here, not
    // the `d` that closes the panel.
    if let Some(cmd) = key_to_scroll(&key) {
        apply_places_scroll(model, cmd);
        return Vec::new();
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('d') => {
            close_places(model);
            Vec::new()
        }
        KeyCode::Enter => {
            let idx = model.places_sel;
            activate_place(model, idx)
        }
        KeyCode::Char('x') | KeyCode::Delete => delete_bookmark_at(model, model.places_sel),
        KeyCode::Char('u') => undo_delete_bookmark(model),
        _ => Vec::new(),
    }
}

/// The places panel's line layout: the rendered LINE each entry sits on (indexed
/// by its flat [`places`] index) and the total number of panel lines. The panel
/// interleaves section titles and a blank spacer with the entry rows, so an
/// entry's selection index is not its line index; this is the single mapping the
/// renderer's sizing and the scroll math both read, so the selected row's on-
/// screen position is always exact. Mirrors the line order `render_places` draws.
struct PlacesLayout {
    /// `entry_line[i]` is the rendered line of the i-th entry in [`places`].
    entry_line: Vec<usize>,
    /// Total rendered panel lines: titles, the spacer, placeholders and rows.
    total: usize,
}

/// Compute the [`PlacesLayout`] for the current model, walking the same line
/// order [`render_places`] builds so the two never disagree.
fn places_layout(model: &Model) -> PlacesLayout {
    let count = places(model).len();
    let bm_count = model.bookmarks.len();
    let mut entry_line = vec![0usize; count];
    let mut line = 0usize;
    line += 1; // "Bookmarks" title
    if bm_count == 0 {
        line += 1; // "  (none)"
    } else {
        for slot in entry_line.iter_mut().take(bm_count) {
            *slot = line;
            line += 1;
        }
    }
    line += 1; // blank spacer
    line += 1; // "Discovered nodes" title
    if count == bm_count {
        line += 1; // "  (none)"
    } else {
        for slot in entry_line.iter_mut().skip(bm_count) {
            *slot = line;
            line += 1;
        }
    }
    PlacesLayout {
        entry_line,
        total: line,
    }
}

/// The places overlay's inner viewport in rows for a panel of `total` lines: the
/// overlay is at most the full terminal height, two rows go to the border.
/// Mirrors `render_places`'s sizing so the scroll math and the draw agree.
fn places_viewport(model: &Model, total: usize) -> usize {
    let rows = model.size.1 as usize;
    let height = (total + 2).min(rows);
    height.saturating_sub(2)
}

/// The places overlay's screen rectangle: the same centred box `render_places`
/// draws, sized from the panel's line count and capped to the terminal. Shared by
/// the renderer and the click / hover hit-test so the two never disagree on where
/// the panel and its rows sit.
fn places_overlay_rect(model: &Model, area: Rect) -> Rect {
    let total = places_layout(model).total;
    let width = 72u16.min(area.width);
    let height = (total as u16 + 2).min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// The places-panel entry under screen point `(col, row)`, honouring the panel's
/// scroll offset, or `None` when the point is not on an entry row — the border, a
/// section title, the blank spacer, a `(none)` placeholder, the scrollbar, or
/// anywhere outside the panel. The screen row maps to a panel line through the
/// clamped scroll offset, and that line maps back to an entry through the same
/// `entry_line` table the scroll math tracks, so a click can never land on the
/// wrong row.
fn place_at(model: &Model, col: u16, row: u16) -> Option<usize> {
    let overlay = places_overlay_rect(model, model.frame_area());
    let inner_top = overlay.y + 1;
    // Strictly inside the border: the top/bottom border rows and the left/right
    // border columns (the right one carries the scrollbar) are not entry rows.
    if row < inner_top
        || row + 1 >= overlay.bottom()
        || col <= overlay.x
        || col + 1 >= overlay.right()
    {
        return None;
    }
    let layout = places_layout(model);
    let vp = places_viewport(model, layout.total);
    let max = layout.total.saturating_sub(vp);
    let offset = model.places_scroll.min(max);
    let line = offset + (row - inner_top) as usize;
    layout.entry_line.iter().position(|&l| l == line)
}

/// Whether a places row shows the `×` delete marker: a bookmark row that is the
/// selection or under the mouse (the browser-tab convention). A node row never
/// shows one — discovered nodes are not deletable. The single predicate behind
/// both the renderer and the click hit-test.
fn marks_x(place: &Place, idx: usize, sel: usize, hover: Option<usize>) -> bool {
    matches!(place, Place::Bookmark { .. }) && (idx == sel || hover == Some(idx))
}

/// The `×` delete marker's column within the panel's inner width: the LAST
/// inner cell, or `None` when the panel is too narrow to reserve one. The one
/// place the marker's cell is computed, so [`place_line`]'s rendering and the
/// click hit-test can never disagree on where it sits.
fn place_x_col(inner: usize) -> Option<usize> {
    (inner >= 2).then(|| inner - 1)
}

/// The `×` marker's SCREEN column for the current overlay geometry, or `None`
/// when the panel has no room for one. Derived from the same overlay rect and
/// inner width `render_places` draws with.
fn place_x_screen_col(model: &Model) -> Option<u16> {
    let overlay = places_overlay_rect(model, model.frame_area());
    let inner = overlay.width.saturating_sub(2) as usize;
    place_x_col(inner).map(|c| overlay.x + 1 + c as u16)
}

/// Whether places row `idx` currently shows the `×` delete marker, through the
/// same [`marks_x`] predicate the renderer uses.
fn place_shows_x(model: &Model, idx: usize) -> bool {
    places(model)
        .get(idx)
        .is_some_and(|p| marks_x(p, idx, model.places_sel, model.places_hover))
}

/// Scroll the places view the MINIMUM needed to keep the selected entry's row
/// inside the viewport, then clamp it to the list. The single piece of view
/// state, so the selected row can never end up off-screen (mirrors the content's
/// keep-line-visible logic).
fn clamp_places_scroll(model: &mut Model) {
    let layout = places_layout(model);
    let vp = places_viewport(model, layout.total);
    let max = layout.total.saturating_sub(vp);
    let sel_line = layout
        .entry_line
        .get(model.places_sel)
        .copied()
        .unwrap_or(0);
    if sel_line < model.places_scroll {
        model.places_scroll = sel_line;
    } else if vp > 0 && sel_line >= model.places_scroll + vp {
        model.places_scroll = sel_line + 1 - vp;
    }
    model.places_scroll = model.places_scroll.min(max);
}

/// Scroll the places VIEW (not the selection) by `WHEEL_STEP` lines in `cmd`'s
/// direction, clamped to `0..=max` through the shared [`scrolled`] rule. This is
/// the mouse wheel: unlike the keys it never touches [`Model::places_sel`], so a
/// notch cannot jump over entries and leave them unreachable. A later key motion
/// moves the selection by one and `clamp_places_scroll` snaps the view back to
/// it — the familiar file-manager behaviour.
fn wheel_places_view(model: &mut Model, cmd: ScrollCmd) {
    let layout = places_layout(model);
    let vp = places_viewport(model, layout.total);
    let max = layout.total.saturating_sub(vp);
    for _ in 0..WHEEL_STEP {
        model.places_scroll = scrolled(model.places_scroll, cmd, vp, max);
    }
}

/// Apply a [`ScrollCmd`] to the places-panel selection through the shared
/// [`scrolled`] rule, so the panel and the page move by the same semantics. Line
/// motions step one entry; page/half-page motions move by the number of entries
/// that fit in the overlay's inner height; top/bottom select the first/last
/// entry. The view then follows via [`clamp_places_scroll`]. A no-op when empty.
fn apply_places_scroll(model: &mut Model, cmd: ScrollCmd) {
    let len = places(model).len();
    if len == 0 {
        model.places_sel = 0;
        model.places_scroll = 0;
        return;
    }
    let vp = places_viewport(model, places_layout(model).total);
    model.places_sel = scrolled(model.places_sel, cmd, vp, len - 1);
    clamp_places_scroll(model);
}

/// Open the selected place: a bookmark's URL, or a discovered node's default
/// page. Closes the panel and starts a fresh navigation. A malformed bookmark
/// URL surfaces an error toast instead.
fn activate_place(model: &mut Model, idx: usize) -> Vec<Effect> {
    let Some(place) = places(model).into_iter().nth(idx) else {
        return Vec::new();
    };
    let target = match place {
        Place::Bookmark { url, .. } => match parse_url(&url, model.current_dest) {
            Ok(target) => target,
            Err(err) => {
                model.set_toast(ToastKind::Error, bad_url_note("bad bookmark", &url, err));
                return Vec::new();
            }
        },
        Place::Node { dest_hash, .. } => Target {
            dest_hash,
            path: DEFAULT_PATH.to_string(),
            fields: Vec::new(),
            is_file: false,
        },
    };
    close_places(model);
    begin_navigation(model, target, HistoryAction::Push, None)
}

/// Delete the bookmark on places row `idx` and persist: stash it in the
/// one-slot undo ([`Model::last_deleted_bookmark`]), keep the selection in
/// range, and toast the title with the undo hint. The single delete path,
/// shared by `x`/`Delete` on the selection and a click on a row's `×` marker.
/// A no-op (no save, no toast) when that row is a discovered node.
fn delete_bookmark_at(model: &mut Model, idx: usize) -> Vec<Effect> {
    let Some(Place::Bookmark { url, title }) = places(model).into_iter().nth(idx) else {
        return Vec::new();
    };
    // Bookmarks lead the flat places list, so the place index IS the bookmark's
    // position within the bookmarks list — the slot the undo restores it to.
    model.last_deleted_bookmark = Some(DeletedBookmark {
        url: url.clone(),
        title: title.clone(),
        index: idx,
    });
    model.bookmarks.remove(&url);
    // Keep the selection on the entry it was on (rows above the selection shift
    // up by one), then clamp it into the shrunk list.
    if model.places_sel > idx {
        model.places_sel -= 1;
    }
    let len = places(model).len();
    model.places_sel = model.places_sel.min(len.saturating_sub(1));
    clamp_places_scroll(model);
    let label = if title.is_empty() { url } else { title };
    model.set_toast(ToastKind::Info, format!("removed {label} — u to undo"));
    vec![Effect::SaveBookmarks]
}

/// Undo the last places-panel bookmark deletion (`u`): re-insert the stashed
/// bookmark at its remembered position (clamped to the current length, so an
/// out-of-range slot appends), clear the one-slot stash, select the restored
/// row and persist. A muted no-op toast when there is nothing to undo, and no
/// save when the URL was re-bookmarked in the meantime.
fn undo_delete_bookmark(model: &mut Model) -> Vec<Effect> {
    let Some(del) = model.last_deleted_bookmark.take() else {
        model.set_toast(ToastKind::Info, "nothing to undo");
        return Vec::new();
    };
    let at = del.index.min(model.bookmarks.len());
    let restored = model.bookmarks.insert(
        at,
        Bookmark {
            url: del.url.clone(),
            title: del.title.clone(),
        },
    );
    if !restored {
        model.set_toast(ToastKind::Info, format!("already bookmarked: {}", del.url));
        return Vec::new();
    }
    model.places_sel = at;
    clamp_places_scroll(model);
    let label = if del.title.is_empty() {
        del.url
    } else {
        del.title
    };
    model.set_toast(ToastKind::Info, format!("restored {label}"));
    vec![Effect::SaveBookmarks]
}

/// Toggle a bookmark for the current page: remove it when the page is already
/// bookmarked, else add it under the current title. Persists on change. A no-op
/// with a toast note when nothing is loaded.
fn toggle_bookmark_current(model: &mut Model) -> Vec<Effect> {
    let Some(url) = current_url(model) else {
        model.set_toast(ToastKind::Info, "nothing to bookmark");
        return Vec::new();
    };
    if model.bookmarks.contains(&url) {
        model.bookmarks.remove(&url);
        model.set_toast(ToastKind::Info, format!("removed bookmark {url}"));
    } else {
        let title = model.title.trim().to_string();
        model.bookmarks.add(Bookmark {
            url: url.clone(),
            title,
        });
        model.set_toast(ToastKind::Info, format!("bookmarked {url}"));
    }
    vec![Effect::SaveBookmarks]
}

/// Yank the focused link's target URL, or (with nothing focused) the current
/// page URL, to the clipboard. A no-op with a toast note when there is nothing
/// to copy.
fn yank_url(model: &mut Model) -> Vec<Effect> {
    let url = match focused_link_url(model).or_else(|| current_url(model)) {
        Some(url) => url,
        None => {
            model.set_toast(ToastKind::Info, "nothing to copy");
            return Vec::new();
        }
    };
    model.set_toast(ToastKind::Info, format!("copied {url}"));
    vec![Effect::Copy(url)]
}

/// The full URL of the focused link's resolved target, or `None` when no link is
/// focused (or it fails to resolve).
fn focused_link_url(model: &Model) -> Option<String> {
    let idx = model.focus?;
    let link = model.links.iter().find(|l| l.index == idx)?;
    let (target, _anchor) = browser::resolve_link(link, model.current_dest).ok()?;
    Some(target_url(&target))
}

/// The full URL of the current page (`<dest_hex>:<path>`), or `None` when no
/// page is loaded.
fn current_url(model: &Model) -> Option<String> {
    model.history.current().map(target_url)
}

/// A full, reopenable URL for a target: `<dest_hex>:<path>`, with any query
/// fields reattached as the backtick blob [`parse_url`] understands (the stored
/// `var_` key prefix stripped back off).
fn target_url(target: &Target) -> String {
    let base = format!("{}:{}", full_hex(&target.dest_hash), target.path);
    if target.fields.is_empty() {
        return base;
    }
    let blob = target
        .fields
        .iter()
        .map(|(k, v)| format!("{}={}", k.strip_prefix("var_").unwrap_or(k), v))
        .collect::<Vec<_>>()
        .join("|");
    format!("{base}`{blob}")
}

/// The full 32-character lowercase hex of a destination hash.
fn full_hex(dest: &[u8; 16]) -> String {
    let mut s = String::with_capacity(dest.len() * 2);
    for byte in dest {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Encode `text` as an OSC 52 clipboard-set sequence: `ESC ] 52 ; c ;
/// <base64(text)> ST` (ST = `ESC \`). The IO shell writes this straight to the
/// terminal, which copies the payload to the system clipboard. Pure, so the
/// encoding is unit-testable without a terminal.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", base64_encode(text.as_bytes()))
}

/// Standard base64 (RFC 4648, with `=` padding) of `bytes`. Small and local so
/// the OSC 52 helper needs no extra dependency.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Whether `target` is a non-idempotent form submit: it carries at least one
/// `field_<name>` value collected from an on-page form. Such requests are never
/// served from (or stored in) the page cache. Preset-only (`var_`) targets are
/// idempotent and cacheable.
fn is_form_submit(target: &Target) -> bool {
    target.fields.iter().any(|(k, _)| k.starts_with("field_"))
}

/// Start a navigation to `target`, recording it in history per `action` and
/// scrolling to `anchor` (if any) once shown.
///
/// A cache hit for an idempotent target renders instantly from
/// [`Model::page_cache`] with its stored scroll restored and emits NO fetch and
/// NO spinner. A miss, or a non-idempotent form submit (never cached), sets the
/// pending fetch and emits [`Effect::Navigate`]. The single entry point every
/// navigation (address bar, link, bookmark, back/forward) routes through so the
/// cache is checked uniformly; a reload deliberately bypasses it.
fn begin_navigation(
    model: &mut Model,
    target: Target,
    action: HistoryAction,
    anchor: Option<String>,
) -> Vec<Effect> {
    // A /file/ target is a download, not a navigation: the shown page and the
    // history stay put while the IO shell fetches and saves the bytes.
    if target.is_file {
        return begin_download(model, target);
    }

    // Remember where the reader was on the page being left, so a later revisit
    // of it restores the scroll position.
    model.stash_scroll();
    model.dismiss_toast();

    if !is_form_submit(&target) {
        if let Some(entry) = model.page_cache.get(&target).cloned() {
            return load_from_cache(model, target, action, anchor, entry);
        }
    }

    model.pending = Some(Pending {
        target: target.clone(),
        action,
    });
    model.pending_anchor = anchor;
    // The fetch supersedes any download in flight (they share the IO slot).
    model.download = None;
    vec![Effect::Navigate(target)]
}

/// Start a file download for `target`: mark it in flight (which shows the
/// loading indicator) and hand the fetch+save to the IO shell. Supersedes any
/// fetch or download already in flight, exactly as starting a fetch does.
fn begin_download(model: &mut Model, target: Target) -> Vec<Effect> {
    model.dismiss_toast();
    model.pending = None;
    model.pending_anchor = None;
    model.download = Some(crate::download::basename(&target.path));
    vec![Effect::Download(target)]
}

/// Show `target` from its cached parsed document: relayout at the current width/
/// theme, restore the stored scroll (or scroll to `anchor`), fold the navigation
/// into history exactly as a fresh load would, and flag the view as cached.
///
/// Emits no fetch. When a fetch was in flight it emits [`Effect::Cancel`] so a
/// late result cannot clobber the page we just restored.
fn load_from_cache(
    model: &mut Model,
    target: Target,
    action: HistoryAction,
    anchor: Option<String>,
    entry: CacheEntry,
) -> Vec<Effect> {
    let was_loading = model.is_loading();
    model.pending = None;
    model.pending_anchor = None;
    model.download = None;

    model.doc = entry.doc;
    model.title = entry.title;
    // The cached document brings its own fields: rebuild the editable store from
    // the freshly laid-out prefill, exactly as a fresh load does.
    model.field_state.clear();
    model.relayout(content_width(model.size.0));
    model.rebuild_field_state();

    // A different page invalidates any focus/hover cursor and search matches
    // held against the old one.
    model.focus = None;
    model.field_focus = None;
    model.mode = Mode::Browse;
    model.hover = None;
    model.matches.clear();
    model.current_match = None;

    model.current_dest = Some(target.dest_hash);
    // A cache hit asks the session nothing, so the identify display flag comes
    // from what the model learned when this run fetched (or toggled) the node.
    model.identifying = model.identify_known.contains(&target.dest_hash);
    match action {
        HistoryAction::Push => model.history.visit(target),
        HistoryAction::Goto(idx) => model.history.goto(idx),
    }

    // Restore the reader's last scroll, or scroll to a followed `#anchor`.
    let vp = model.viewport();
    match anchor {
        Some(name) => match anchor_line(model, &name) {
            Some(line) => model.scroll = line.min(model.max_scroll(vp)),
            None => {
                model.scroll = 0;
                model.set_toast(ToastKind::Error, format!("anchor #{name} not found"));
            }
        },
        None => model.scroll = entry.scroll.min(model.max_scroll(vp)),
    }

    model.cached_view = true;

    // The page cache holds documents, not payloads: a revisit re-fetches its
    // pictures. Cheap to change later (the store could keep the bytes), but
    // caching an unbounded pile of photographs is not obviously the kindness
    // it looks like.
    let mut effects = plan_images(model);
    if was_loading {
        effects.insert(0, Effect::Cancel);
    }
    effects
}

/// Reload the page under the history cursor without changing history.
fn reload_current(model: &mut Model) -> Vec<Effect> {
    let Some(target) = model.history.current().cloned() else {
        return Vec::new();
    };
    let idx = model.history.idx;
    model.pending = Some(Pending {
        target: target.clone(),
        action: HistoryAction::Goto(idx),
    });
    model.pending_anchor = None;
    model.download = None;
    model.dismiss_toast();
    vec![Effect::Navigate(target)]
}

/// Toggle identifying to the current page's node: flip the model's flag
/// optimistically (the shell's post-reload refresh is authoritative), ask the
/// shell to persist the decision, and reload the page — identify binds to the
/// link, so the new decision only takes effect on a fresh one. The single
/// source of truth for both the `i` key and the footer's identify button.
fn toggle_identify(model: &mut Model) -> Vec<Effect> {
    let Some(dest) = model.current_dest else {
        return Vec::new();
    };
    let on = !model.identifying;
    model.note_identify(dest, on);
    let mut effects = vec![Effect::SetIdentify { dest, on }];
    effects.extend(reload_current(model));
    // After reload_current, whose toast dismissal must not clear this one.
    let dest8: String = crate::identity::fingerprint_hex(&dest)
        .chars()
        .take(8)
        .collect();
    if on {
        let fp8: String = model.fingerprint.chars().take(8).collect();
        model.set_toast(
            ToastKind::Info,
            format!("Identifying to {dest8} as {fp8}  (i to stop)"),
        );
    } else {
        model.set_toast(ToastKind::Info, format!("Now anonymous to {dest8}"));
    }
    effects
}

/// Navigate back one history entry (re-fetching it), if possible.
fn go_back(model: &mut Model) -> Vec<Effect> {
    if !model.history.can_back() {
        return Vec::new();
    }
    let idx = model.history.idx - 1;
    let target = model.history.stack[idx].clone();
    begin_navigation(model, target, HistoryAction::Goto(idx), None)
}

/// Navigate forward one history entry (re-fetching it), if possible.
fn go_forward(model: &mut Model) -> Vec<Effect> {
    if !model.history.can_forward() {
        return Vec::new();
    }
    let idx = model.history.idx + 1;
    let target = model.history.stack[idx].clone();
    begin_navigation(model, target, HistoryAction::Goto(idx), None)
}

/// Follow the link with 1-based `index`: resolve its target against the current
/// destination and start a fresh navigation, or raise an error toast.
fn follow_link(model: &mut Model, index: usize) -> Vec<Effect> {
    let Some(link) = model.links.iter().find(|l| l.index == index).cloned() else {
        return Vec::new();
    };
    // A picture already on the page has been transferred once; following its
    // link saves those bytes rather than fetching them again. Over a mesh link
    // that difference is minutes, not milliseconds.
    if let Some(effect) = saved_image_effect(model, index, false) {
        return vec![effect];
    }
    // An external URL is opened in the user's default handler; an unsafe scheme
    // is refused outright. Only an RNS target is fetched in-mesh.
    match classify_link(&link.target) {
        LinkKind::External(url) => {
            model.dismiss_toast();
            vec![Effect::OpenExternal(url)]
        }
        LinkKind::Unsafe(scheme) => {
            model.set_toast(ToastKind::Error, format!("won't open {scheme}: link"));
            Vec::new()
        }
        // NomadNet opens a conversation here. We are a page browser with no
        // composer, so the useful thing is to hand the address over: show it
        // and put it on the clipboard, ready to paste into a client that can
        // send. Anything is better than the "malformed URL" this used to
        // produce, which read as a broken page rather than an unsupported
        // action.
        LinkKind::Lxmf(hash) => {
            model.set_toast(ToastKind::Info, format!("LXMF address copied: {hash}"));
            vec![Effect::Copy(hash)]
        }
        LinkKind::Rns => match browser::resolve_link(&link, model.current_dest) {
            Ok((mut target, anchor)) => {
                // A submit link (one that references form fields, or `*` for all)
                // carries the CURRENT field values, packaged as NomadNet expects:
                // `field_<name>` entries alongside the `var_<key>` presets that
                // resolve_link already put on the target. Preset `var_` and
                // collected `field_` keys never collide, so we just append.
                target.fields.extend(collect_submit_fields(model, &link));
                // A submit target is routed through the same entry point but is
                // never served from cache (see [`is_form_submit`]); any `#anchor`
                // is resolved against the shown page.
                begin_navigation(model, target, HistoryAction::Push, anchor)
            }
            Err(err) => {
                model.set_toast(
                    ToastKind::Error,
                    bad_url_note("bad link", &link.target, err),
                );
                Vec::new()
            }
        },
    }
}

/// The toast for a URL that would not parse: what was wrong and, when the fix
/// is mechanical, the URL that would have worked.
///
/// A page that links to its own node with `/page/about.mu` has dropped the `:`
/// that a local URL keeps. That is a dead end in the reference browser too, so
/// the useful thing is not to restate the grammar but to show the reader (and,
/// through them, the node's author) the exact string that works.
///
/// The suggestion leads and the explanation trails, because the toast is one
/// truncated line: an over-long path eats the parenthetical rather than the fix.
/// The rejected URL is not repeated — the suggestion already contains it.
/// The save-or-open effect for a link that is an inline picture already held
/// in memory, or `None` when it is not one (or has not arrived).
///
/// `open` picks between saving and saving-then-opening. A slot that is still
/// queued, loading or failed has no bytes, so it falls through to the ordinary
/// download path and fetches them.
fn saved_image_effect(model: &Model, index: usize, open: bool) -> Option<Effect> {
    let slot = model.image_slot(index)?;
    match slot.state {
        SlotState::Ready { .. } | SlotState::Text { .. } => Some(match open {
            true => Effect::OpenImage { link: index },
            false => Effect::SaveImage { link: index },
        }),
        _ => None,
    }
}

fn bad_url_note(what: &str, raw: &str, err: UrlError) -> String {
    match err {
        UrlError::NoDestination if raw.starts_with('/') => {
            format!("{what} — try :{raw} (a local URL keeps the \":\")")
        }
        other => format!("{what}: {raw} — {other}"),
    }
}

/// Collect the current values of the form fields a link references, packaged as
/// the NomadNet request map expects: each becomes a `field_<name>` entry.
///
/// Mirrors the reference `Browser.handle_link` (NomadNet `Browser.py`): a link's
/// field components that carry no `=` are field-NAME references (a `*` component
/// means "all fields"); their referenced fields' current widget values are added
/// under a `field_` prefix. A text field is always included; a checkbox/radio is
/// included only when checked (its submit value), and several checked checkboxes
/// sharing a name are comma-joined. The `k=v` preset components are handled by
/// [`browser::resolve_link`] as `var_<k>`, so they are ignored here.
fn collect_submit_fields(model: &Model, link: &RenderedLink) -> Vec<(String, String)> {
    let mut all = false;
    let mut referenced: Vec<&str> = Vec::new();
    for (k, v) in &link.fields {
        if !v.is_empty() {
            continue; // a `k=v` preset -> handled as var_ by resolve_link.
        }
        if k == "*" {
            all = true;
        } else {
            referenced.push(k.as_str());
        }
    }
    if !all && referenced.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<(String, String)> = Vec::new();
    for fe in &model.field_state {
        if !all && !referenced.iter().any(|n| *n == fe.name) {
            continue;
        }
        let key = format!("field_{}", fe.name);
        match fe.kind {
            FieldKind::Text => out.push((key, fe.input.value().to_string())),
            FieldKind::Checkbox => {
                if fe.checked {
                    if let Some(existing) = out.iter_mut().find(|(k, _)| *k == key) {
                        existing.1 = format!("{},{}", existing.1, fe.value);
                    } else {
                        out.push((key, fe.value.clone()));
                    }
                }
            }
            FieldKind::Radio => {
                if fe.checked {
                    if let Some(existing) = out.iter_mut().find(|(k, _)| *k == key) {
                        existing.1 = fe.value.clone();
                    } else {
                        out.push((key, fe.value.clone()));
                    }
                }
            }
        }
    }
    out
}

/// Activate a hint target: follow the link, activate the form field, or drive
/// the top-bar control.
fn activate_hint_target(model: &mut Model, target: HintTarget) -> Vec<Effect> {
    match target {
        HintTarget::Link(idx) => follow_link(model, idx),
        HintTarget::Field(fi) => {
            activate_field(model, fi);
            Vec::new()
        }
        HintTarget::Control(Control::Address) => {
            enter_address(model);
            Vec::new()
        }
        HintTarget::Control(Control::Bookmark) => toggle_bookmark_current(model),
    }
}

/// Focus and activate form field `fi` (1-based): a checkbox toggles, a radio
/// selects, a text field only takes focus (entering [`Mode::Field`]). The one
/// body shared by a mouse click and a hint activation, so they never diverge.
fn activate_field(model: &mut Model, fi: usize) {
    apply_focus(model, Focus::Field(fi));
    toggle_field(model, fi);
}

/// The interactive elements (links and form fields) in document order: sorted by
/// laid-out `(line, col_start)`, so the Tab cycle walks them exactly as they read
/// down the page. The single source of truth for focus traversal.
fn interactive_order(model: &Model) -> Vec<Focus> {
    let mut items: Vec<(usize, usize, Focus)> = Vec::new();
    for l in &model.links {
        items.push((l.line, l.col_start, Focus::Link(l.index)));
    }
    for f in &model.field_defs {
        items.push((f.line, f.col_start, Focus::Field(f.index)));
    }
    items.sort_by_key(|(line, col, _)| (*line, *col));
    items.into_iter().map(|(_, _, focus)| focus).collect()
}

/// The currently focused interactive element, as a [`Focus`], or `None`.
fn current_focus(model: &Model) -> Option<Focus> {
    if let Some(i) = model.field_focus {
        Some(Focus::Field(i))
    } else {
        model.focus.map(Focus::Link)
    }
}

/// Move the focus cursor to the next interactive element (link or field) in
/// reading order (wrapping), scrolling it into view. A no-op when the page has no
/// interactive elements.
fn focus_next(model: &mut Model) {
    let order = interactive_order(model);
    if order.is_empty() {
        return;
    }
    let next = match current_focus(model).and_then(|c| order.iter().position(|x| *x == c)) {
        Some(i) => order[(i + 1) % order.len()],
        None => order[0],
    };
    apply_focus(model, next);
}

/// Move the focus cursor to the previous interactive element (wrapping),
/// scrolling it into view. A no-op when the page has no interactive elements.
fn focus_prev(model: &mut Model) {
    let order = interactive_order(model);
    if order.is_empty() {
        return;
    }
    let last = order.len() - 1;
    let prev = match current_focus(model).and_then(|c| order.iter().position(|x| *x == c)) {
        Some(0) | None => order[last],
        Some(i) => order[i - 1],
    };
    apply_focus(model, prev);
}

/// Focus an interactive element: set the link/field focus (they are mutually
/// exclusive), switch mode (a field enters [`Mode::Field`] editing; a link
/// returns to [`Mode::Browse`]), and auto-scroll it into view.
fn apply_focus(model: &mut Model, focus: Focus) {
    match focus {
        Focus::Link(index) => {
            model.focus = Some(index);
            model.field_focus = None;
            model.mode = Mode::Browse;
            if let Some(line) = model
                .links
                .iter()
                .find(|l| l.index == index)
                .map(|l| l.line)
            {
                scroll_line_into_view(model, line);
            }
        }
        Focus::Field(index) => {
            model.field_focus = Some(index);
            model.focus = None;
            model.mode = Mode::Field;
            if let Some(line) = model
                .field_defs
                .iter()
                .find(|f| f.index == index)
                .map(|f| f.line)
            {
                scroll_line_into_view(model, line);
            }
        }
    }
}

/// Scroll the minimal amount so page `line` is inside the viewport.
fn scroll_line_into_view(model: &mut Model, line: usize) {
    let vp = model.viewport();
    if line < model.scroll {
        model.scroll = line;
    } else if vp > 0 && line >= model.scroll + vp {
        model.scroll = line + 1 - vp;
    }
    model.clamp_scroll(vp);
}

/// Hit-test a mouse click at `(col, row)`: follow the link under it, else
/// activate the top-bar control under it, else do nothing.
fn handle_click(model: &mut Model, col: u16, row: u16) -> Vec<Effect> {
    // The help overlay is modal: a left click anywhere while it is open closes
    // it, before any other hit-testing, so the click cannot fall through to a
    // link, control or footer button underneath.
    if model.show_help {
        model.show_help = false;
        return Vec::new();
    }
    // The places panel is modal too: a single left click on an entry selects and
    // activates it (the same language as links and footer buttons); a click
    // elsewhere inside the panel does nothing; a click outside closes it. Every
    // case is swallowed here, so a click can never fall through to a link,
    // control or footer button underneath.
    if model.show_places {
        if let Some(idx) = place_at(model, col, row) {
            // The `×` marker's cell is special-cased before the normal row
            // action: on a row showing the marker it deletes that bookmark; on
            // any row without one (a node row never has one) it is inert, so a
            // click there can never delete or activate. The cell comes from the
            // same layout `place_line` renders, so click and draw agree.
            if place_x_screen_col(model) == Some(col) {
                if place_shows_x(model, idx) {
                    return delete_bookmark_at(model, idx);
                }
                return Vec::new();
            }
            model.places_sel = idx;
            return activate_place(model, idx);
        }
        if !rect_contains(places_overlay_rect(model, model.frame_area()), col, row) {
            close_places(model);
        }
        return Vec::new();
    }
    for vl in visible_links(model) {
        if row == vl.row && col >= vl.col_start && col < vl.col_end {
            return follow_link(model, vl.index);
        }
    }
    // A click on a form field focuses it (entering field-edit mode) and, for a
    // checkbox or radio, also activates it — the same as pressing Space, and
    // the same as the reference browser's mouse behaviour.
    for vf in visible_fields(model) {
        if row == vf.row && col >= vf.col_start && col < vf.col_end {
            activate_field(model, vf.index);
            return Vec::new();
        }
    }
    let [topbar, _content, footer] = regions(model.frame_area());
    for cr in top_bar_controls(model, topbar) {
        if rect_contains(cr.rect, col, row) {
            return activate_hint_target(model, HintTarget::Control(cr.control));
        }
    }
    // A click on a footer button runs its action (same as pressing its key).
    if footer_shows_buttons(model) {
        for slot in footer_layout(model, footer) {
            if slot.item.action != FooterAction::None && rect_contains(slot.rect, col, row) {
                return run_footer_action(model, slot.item.action);
            }
        }
    }
    Vec::new()
}

/// Set the hovered link to whichever visible link is under `(col, row)`, or
/// `None` when the cursor is over no link.
fn update_hover(model: &mut Model, col: u16, row: u16) {
    // The places panel owns the pointer while it is open: a move over an entry
    // highlights that row, a move off any entry clears it. The link and footer
    // hovers stay cleared beneath the modal panel.
    if model.show_places {
        model.places_hover = place_at(model, col, row);
        model.hover = None;
        model.footer_hover = None;
        return;
    }
    // A move over a footer button highlights it; a footer hover and a content
    // link hover are mutually exclusive.
    if footer_shows_buttons(model) {
        let [_topbar, _content, footer] = regions(model.frame_area());
        if let Some(slot) = footer_layout(model, footer)
            .into_iter()
            .find(|s| s.item.action != FooterAction::None && rect_contains(s.rect, col, row))
        {
            model.footer_hover = Some(slot.index);
            model.hover = None;
            return;
        }
    }
    model.footer_hover = None;
    model.hover = visible_links(model)
        .into_iter()
        .find(|vl| row == vl.row && col >= vl.col_start && col < vl.col_end)
        .map(|vl| vl.index);
}

/// Whether `rect` contains the point `(x, y)`.
fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

/// The modifier class a [`ScrollChord`] matches: any modifiers, none, Ctrl, or
/// Alt. `Any` mirrors the bare arrow / paging keys, which scroll regardless of
/// modifiers; `Plain` requires neither Ctrl nor Alt.
#[derive(Clone, Copy)]
enum ScrollMods {
    Any,
    Plain,
    Ctrl,
    Alt,
}

/// One key chord that produces a scroll motion: the key, the modifier class it
/// needs, and the [`ScrollCmd`] it maps to.
#[derive(Clone, Copy)]
struct ScrollChord {
    code: KeyCode,
    mods: ScrollMods,
    cmd: ScrollCmd,
}

/// One row of the scroll keymap: the keys as displayed in the help overlay, the
/// description beside them, and every chord that triggers the row's motions.
/// [`SCROLL_KEYS`] is the SINGLE source of truth read by BOTH [`key_to_scroll`]
/// and the help overlay, so a scroll binding can never exist in one without
/// appearing in the other.
struct ScrollKey {
    keys: &'static str,
    desc: &'static str,
    chords: &'static [ScrollChord],
}

/// The scroll keymap. Both the key handler and the help overlay read this table,
/// so the two can never drift apart.
const SCROLL_KEYS: &[ScrollKey] = &[
    ScrollKey {
        keys: "j / k   ↓ / ↑   Ctrl-n / Ctrl-p",
        desc: "scroll a line",
        chords: &[
            ScrollChord {
                code: KeyCode::Char('j'),
                mods: ScrollMods::Plain,
                cmd: ScrollCmd::LineDown,
            },
            ScrollChord {
                code: KeyCode::Char('n'),
                mods: ScrollMods::Ctrl,
                cmd: ScrollCmd::LineDown,
            },
            ScrollChord {
                code: KeyCode::Down,
                mods: ScrollMods::Any,
                cmd: ScrollCmd::LineDown,
            },
            ScrollChord {
                code: KeyCode::Char('k'),
                mods: ScrollMods::Plain,
                cmd: ScrollCmd::LineUp,
            },
            ScrollChord {
                code: KeyCode::Char('p'),
                mods: ScrollMods::Ctrl,
                cmd: ScrollCmd::LineUp,
            },
            ScrollChord {
                code: KeyCode::Up,
                mods: ScrollMods::Any,
                cmd: ScrollCmd::LineUp,
            },
        ],
    },
    ScrollKey {
        keys: "Ctrl-f / Ctrl-b   Ctrl-v   PageDown / PageUp",
        desc: "page down / up",
        chords: &[
            ScrollChord {
                code: KeyCode::Char('f'),
                mods: ScrollMods::Ctrl,
                cmd: ScrollCmd::PageDown,
            },
            ScrollChord {
                code: KeyCode::Char('v'),
                mods: ScrollMods::Ctrl,
                cmd: ScrollCmd::PageDown,
            },
            ScrollChord {
                code: KeyCode::Char(' '),
                mods: ScrollMods::Plain,
                cmd: ScrollCmd::PageDown,
            },
            ScrollChord {
                code: KeyCode::PageDown,
                mods: ScrollMods::Any,
                cmd: ScrollCmd::PageDown,
            },
            ScrollChord {
                code: KeyCode::Char('b'),
                mods: ScrollMods::Ctrl,
                cmd: ScrollCmd::PageUp,
            },
            ScrollChord {
                code: KeyCode::Char('v'),
                mods: ScrollMods::Alt,
                cmd: ScrollCmd::PageUp,
            },
            ScrollChord {
                code: KeyCode::PageUp,
                mods: ScrollMods::Any,
                cmd: ScrollCmd::PageUp,
            },
        ],
    },
    ScrollKey {
        keys: "Ctrl-d / Ctrl-u",
        desc: "half a page",
        chords: &[
            ScrollChord {
                code: KeyCode::Char('d'),
                mods: ScrollMods::Ctrl,
                cmd: ScrollCmd::HalfPageDown,
            },
            ScrollChord {
                code: KeyCode::Char('u'),
                mods: ScrollMods::Ctrl,
                cmd: ScrollCmd::HalfPageUp,
            },
        ],
    },
    ScrollKey {
        keys: "g / G   Home / End",
        desc: "top / bottom",
        chords: &[
            ScrollChord {
                code: KeyCode::Char('g'),
                mods: ScrollMods::Plain,
                cmd: ScrollCmd::Top,
            },
            ScrollChord {
                code: KeyCode::Char('<'),
                mods: ScrollMods::Alt,
                cmd: ScrollCmd::Top,
            },
            ScrollChord {
                code: KeyCode::Home,
                mods: ScrollMods::Any,
                cmd: ScrollCmd::Top,
            },
            ScrollChord {
                code: KeyCode::Char('G'),
                mods: ScrollMods::Plain,
                cmd: ScrollCmd::Bottom,
            },
            ScrollChord {
                code: KeyCode::Char('>'),
                mods: ScrollMods::Alt,
                cmd: ScrollCmd::Bottom,
            },
            ScrollChord {
                code: KeyCode::End,
                mods: ScrollMods::Any,
                cmd: ScrollCmd::Bottom,
            },
        ],
    },
];

/// Map a key press to a [`ScrollCmd`], honouring both vi and emacs idioms, by
/// scanning [`SCROLL_KEYS`]. Returns `None` for keys that are not scroll motions.
fn key_to_scroll(key: &KeyEvent) -> Option<ScrollCmd> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let plain = !ctrl && !alt;
    for row in SCROLL_KEYS {
        for chord in row.chords {
            let mods_ok = match chord.mods {
                ScrollMods::Any => true,
                ScrollMods::Plain => plain,
                ScrollMods::Ctrl => ctrl,
                ScrollMods::Alt => alt,
            };
            if mods_ok && chord.code == key.code {
                return Some(chord.cmd);
            }
        }
    }
    None
}

/// The ` · ` separator drawn between the title and the address on the top bar.
const TOPBAR_SEP: &str = " · ";

/// The resolved geometry of the one-row top bar: the title and address (each
/// already ellipsized to fit) and the right-aligned status cluster (bookmark
/// star, cache bolt, hop count). The renderer and the click / hint hit-testing
/// both derive from this one function, so they can never disagree on where a
/// span sits.
struct TopBar {
    /// The title text (label only), ellipsized to fit; empty when none.
    title: String,
    /// The column the title starts at.
    title_x: u16,
    /// The `·` separator's column, present only when both title and address show.
    sep_x: Option<u16>,
    /// The address text, ellipsized to fit; empty when it did not fit at all.
    address: String,
    /// The address slot's rect; a click here opens the address editor.
    address_rect: Rect,
    /// The bookmark star: its glyph (★ / ☆) and rect, absent when no page is
    /// loaded. A click toggles the bookmark.
    star: Option<(&'static str, Rect)>,
    /// The identity marker (key glyph + fingerprint prefix) and its column,
    /// present only while identifying to the current node. Anonymous — the
    /// default — shows nothing: the visible state is the one with consequences.
    identity: Option<(String, u16)>,
    /// The cache-bolt glyph's column, present only for a cached view.
    cached_x: Option<u16>,
    /// The hop-count text (`N hops` / `unknown`) and its column.
    hops: Option<(String, u16)>,
}

/// The bookmark-star glyph for the current page: filled when bookmarked, hollow
/// when not, or `None` when no page is loaded (nothing to bookmark).
fn star_glyph(model: &Model) -> Option<&'static str> {
    let url = current_url(model)?;
    Some(if model.bookmarks.contains(&url) {
        STAR_ON
    } else {
        STAR_OFF
    })
}

/// The identity-marker cluster text: the key glyph plus the first 8 hex chars
/// of the session's own fingerprint, or `None` when not identifying to the
/// current node (or the fingerprint is unset, as in a bare test model).
fn identity_text(model: &Model) -> Option<String> {
    if !model.identifying || model.fingerprint.is_empty() {
        return None;
    }
    let fp8: String = model.fingerprint.chars().take(8).collect();
    Some(format!("{IDENTIFY_GLYPH} {fp8}"))
}

/// The hop-count cluster text for the current destination: `N hops` when the
/// destination is a known announced node with a hop count, `unknown` when it is
/// not in the registry, or `None` when no page is loaded.
fn hops_text(model: &Model) -> Option<String> {
    let dest = model.current_dest?;
    Some(
        match model.node_registry.get_by_hash(&dest).and_then(|n| n.hops) {
            Some(h) => format!("{h} hops"),
            None => "unknown".to_string(),
        },
    )
}

fn topbar_layout(model: &Model, area: Rect) -> TopBar {
    let y = area.y;
    let right = area.right();

    // Right-aligned status cluster: star, identity marker, cache bolt, hop
    // count, each present only when it applies, separated by a single column.
    let star = star_glyph(model);
    let identity = identity_text(model);
    let mut cached = model.cached_view;
    let mut hops = hops_text(model);
    let seg_w = |g: &str| UnicodeWidthStr::width(g) as u16;
    let cluster_width = |cached: bool, hops: &Option<String>| -> u16 {
        let mut w = 0u16;
        let mut segs = 0u16;
        if let Some(g) = star {
            w += seg_w(g);
            segs += 1;
        }
        if let Some(t) = &identity {
            w += seg_w(t);
            segs += 1;
        }
        if cached {
            w += seg_w(CACHED_GLYPH);
            segs += 1;
        }
        if let Some(h) = hops {
            w += seg_w(h);
            segs += 1;
        }
        w + segs.saturating_sub(1) // one blank column between segments
    };
    // When even the bare cluster cannot fit the bar, drop the least important
    // segments: the hop count first, then the cache bolt. The star and the
    // identity marker — the security-relevant state — always survive.
    if cluster_width(cached, &hops) > area.width && hops.is_some() {
        hops = None;
    }
    if cluster_width(cached, &hops) > area.width {
        cached = false;
    }
    let cluster_w = cluster_width(cached, &hops);
    let cluster_x = right.saturating_sub(cluster_w).max(area.x);

    // Place each cluster segment left to right from cluster_x.
    let mut cx = cluster_x;
    let star = star.map(|g| {
        let w = seg_w(g);
        let rect = Rect {
            x: cx,
            y,
            width: w,
            height: 1,
        };
        cx = cx.saturating_add(w).saturating_add(1);
        (g, rect)
    });
    let identity = identity.map(|t| {
        let x = cx;
        cx = cx.saturating_add(seg_w(&t)).saturating_add(1);
        (t, x)
    });
    let cached_x = if cached {
        let x = cx;
        cx = cx.saturating_add(seg_w(CACHED_GLYPH)).saturating_add(1);
        Some(x)
    } else {
        None
    };
    let hops = hops.map(|h| {
        let x = cx;
        (h, x)
    });

    // Left identity: title, separator, address. The cluster keeps its width; the
    // title comes next; the address ellipsizes; and, only when even the title
    // plus separator will not fit, the title ellipsizes too.
    let left_end = if cluster_w > 0 {
        cluster_x.saturating_sub(CTRL_GAP)
    } else {
        right
    };
    let left_w = left_end.saturating_sub(area.x);
    let title_full = model.title.trim().to_string();
    let addr_full = breadcrumb(model);
    let sep_w = UnicodeWidthStr::width(TOPBAR_SEP) as u16;
    let tw = UnicodeWidthStr::width(title_full.as_str()) as u16;

    // Decide the title and how much width the address slot gets. The cluster
    // already has its width; the title comes next; the address slot takes the
    // rest and ellipsizes; and only when even the title plus separator plus one
    // address column will not fit does the title ellipsize and the address drop.
    let (title, has_sep, addr_avail) = if title_full.is_empty() {
        (String::new(), false, left_w)
    } else if tw + sep_w + 1 > left_w {
        (truncate_to_cols(&title_full, left_w as usize), false, 0)
    } else {
        (title_full, true, left_w - tw - sep_w)
    };

    let title_x = area.x;
    let title_w = UnicodeWidthStr::width(title.as_str()) as u16;
    let (sep_x, addr_x) = if has_sep {
        (Some(area.x + title_w), area.x + title_w + sep_w)
    } else if title.is_empty() {
        (None, area.x)
    } else {
        (None, area.x + title_w)
    };
    // The address text ellipsizes to the slot; the slot itself is the full
    // remaining width so a click anywhere in it, and the live editor, both land.
    let address = truncate_to_cols(&addr_full, addr_avail as usize);
    let address_rect = Rect {
        x: addr_x,
        y,
        width: addr_avail,
        height: 1,
    };

    TopBar {
        title,
        title_x,
        sep_x,
        address,
        address_rect,
        star,
        identity,
        cached_x,
        hops,
    }
}

/// The clickable top-bar controls (the address slot and, when a page is loaded,
/// the bookmark star), derived from [`topbar_layout`] so clicks and hint labels
/// hit-test against exactly what the renderer draws.
fn top_bar_controls(model: &Model, area: Rect) -> Vec<ControlRect> {
    let tb = topbar_layout(model, area);
    let mut out = vec![ControlRect {
        control: Control::Address,
        rect: tb.address_rect,
    }];
    if let Some((_, rect)) = tb.star {
        out.push(ControlRect {
            control: Control::Bookmark,
            rect,
        });
    }
    out
}

/// Draw the whole UI: the top-bar, the scrollable content, and the status bar,
/// plus the help overlay when active.
pub fn view(model: &Model, images: &ImageStore, frame: &mut Frame) {
    let [topbar, content, status] = regions(frame.area());
    render_topbar(model, frame, topbar);
    render_content(model, frame, content);
    // Pictures go over the reserved rows, before every other overlay: a toast,
    // the help or the places panel must be able to cover one.
    render_images(model, images, frame, content);
    // The bottom-left status chip floats over the content, above the footer row,
    // so the footer's clickable buttons below it are never covered.
    render_status_chip(model, frame, frame.area());
    render_status(model, frame, status);
    // Overlays drawn on top of the laid-out page: the form-field input boxes, the
    // search-match highlights, the focus highlight, the mouse-hover highlight, and
    // the hint badges while hint mode is active.
    render_fields(model, frame);
    render_search_matches(model, frame);
    render_focus(model, frame);
    render_hover(model, frame);
    if model.mode == Mode::Hint {
        render_hints(model, frame);
    }
    if model.show_places {
        render_places(model, frame, frame.area());
    }
    if model.show_help {
        render_help(model, frame, frame.area());
    }
    // The toast floats on top of everything so it is always visible.
    render_toast(model, frame, frame.area());
    // Colour-depth post-pass: on a terminal without true colour, downgrade every
    // 24-bit RGB colour in the finished buffer to its nearest xterm-256 index in
    // one sweep. A single chokepoint over the rendered buffer keeps every colour
    // site (content, chrome, spinner, toast, highlights) media-agnostic.
    downgrade_buffer(model, frame);
}

/// Downgrade the rendered buffer's 24-bit colours to the xterm-256 palette when
/// the resolved [`ColorDepth`] is [`ColorDepth::Ansi256`]. A no-op under
/// [`ColorDepth::Truecolor`] and under `no_color` (where `NO_COLOR` already wins
/// and the buffer carries the reverse-video chrome instead of colour), so both
/// leave the buffer byte-identical to before this feature.
fn downgrade_buffer(model: &Model, frame: &mut Frame) {
    if model.no_color || model.depth == ColorDepth::Truecolor {
        return;
    }
    let area = frame.area();
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.fg = downgrade_color(cell.fg);
                cell.bg = downgrade_color(cell.bg);
            }
        }
    }
}

/// Map a single 24-bit [`Color::Rgb`] to its nearest xterm-256 [`Color::Indexed`];
/// every other colour (already-indexed, named, reset) passes through unchanged.
fn downgrade_color(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Indexed(rgb_to_ansi256(r, g, b)),
        other => other,
    }
}

/// Draw the form-field input boxes over the laid-out page: every field's cells
/// get an input style (a distinct chrome-slate box, or an underline box under
/// `no_color`); the focused field is reversed on top so it stands out. A focused
/// TEXT field also places the hardware cursor at its edit position.
fn render_fields(model: &Model, frame: &mut Frame) {
    let visible = visible_fields(model);
    if visible.is_empty() {
        return;
    }
    let area = frame.area();
    let base = if model.no_color {
        Style::default().add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(rgb(model.theme.chrome_fg()))
            .bg(rgb(model.theme.chrome_bg()))
    };
    let focused = base.add_modifier(Modifier::REVERSED);
    // The hardware cursor position for a focused text field, resolved after the
    // buffer paint so it lands on the correct cell.
    let mut cursor: Option<(u16, u16)> = None;
    {
        let buf = frame.buffer_mut();
        for vf in &visible {
            let is_focused = model.field_focus == Some(vf.index);
            let style = if is_focused { focused } else { base };
            for x in vf.col_start..vf.col_end {
                if x >= area.width || vf.row >= area.height {
                    continue;
                }
                if let Some(c) = buf.cell_mut((x, vf.row)) {
                    c.set_style(style);
                }
            }
            if is_focused && vf.kind == FieldKind::Text {
                if let Some(fe) = model.field_state.get(vf.index - 1) {
                    // The box carries no bracket chrome, and its text may be
                    // scrolled: the caret sits at the editor offset minus the
                    // window's scroll, clamped to the box's last cell. Same
                    // window the relayout used, so caret and text agree.
                    let w = (vf.col_end - vf.col_start) as usize;
                    let scroll = fe.input.visual_scroll(w.saturating_sub(1));
                    let cx = vf.col_start + fe.input.visual_cursor().saturating_sub(scroll) as u16;
                    let clamped = cx.min(vf.col_end.saturating_sub(1));
                    cursor = Some((clamped, vf.row));
                }
            }
        }
    }
    if let Some(pos) = cursor {
        frame.set_cursor_position(pos);
    }
}

/// Highlight the focused link's cells (reverse video) so Tab navigation is
/// visible. A no-op when nothing is focused or the focus is off-screen.
fn render_focus(model: &Model, frame: &mut Frame) {
    let Some(idx) = model.focus else {
        return;
    };
    let area = frame.area();
    let buf = frame.buffer_mut();
    let highlight = Style::default().add_modifier(Modifier::REVERSED);
    for vl in visible_links(model) {
        if vl.index != idx {
            continue;
        }
        for x in vl.col_start..vl.col_end {
            if x >= area.width || vl.row >= area.height {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, vl.row)) {
                cell.set_style(highlight);
            }
        }
    }
}

/// Highlight the link the mouse is hovering over, so the pointer's target is
/// visible in the content as well as the status bar. Kept distinct from the Tab
/// focus (reverse video): hover patches bold + underline onto the link's own
/// colour, so the two can coexist on different links. A no-op when nothing is
/// hovered or the hovered link is off-screen.
fn render_hover(model: &Model, frame: &mut Frame) {
    let Some(idx) = model.hover else {
        return;
    };
    let area = frame.area();
    let buf = frame.buffer_mut();
    let hover = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    for vl in visible_links(model) {
        if vl.index != idx {
            continue;
        }
        for x in vl.col_start..vl.col_end {
            if x >= area.width || vl.row >= area.height {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, vl.row)) {
                // Patch (not replace) so the link colour and any focus reverse
                // underneath are preserved and the hover modifiers add on top.
                let style = cell.style().patch(hover);
                cell.set_style(style);
            }
        }
    }
}

/// Highlight the on-screen search matches: every match wears a theme-aware
/// background tint, and the current match a stronger reversed highlight on top.
/// Off-viewport matches are skipped. A no-op when there are no matches.
fn render_search_matches(model: &Model, frame: &mut Frame) {
    if model.matches.is_empty() {
        return;
    }
    let area = frame.area();
    let viewport = model.viewport();
    let scroll = model.scroll;
    let match_style = Style::default()
        .fg(rgb(model.theme.search_match_fg()))
        .bg(rgb(model.theme.search_match_bg()));
    let current_style = Style::default().add_modifier(Modifier::REVERSED);
    let buf = frame.buffer_mut();
    for (i, m) in model.matches.iter().enumerate() {
        if m.line_idx < scroll || m.line_idx >= scroll + viewport {
            continue;
        }
        let row = TOPBAR_ROWS + (m.line_idx - scroll) as u16;
        let style = if model.current_match == Some(i) {
            current_style
        } else {
            match_style
        };
        for col in m.col_start..m.col_end {
            let x = col as u16;
            if x >= area.width || row >= area.height {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, row)) {
                cell.set_style(style);
            }
        }
    }
}

/// Draw the hint badges: each matching hint's label over the first cells of its
/// link or control, on a distinct background. Only hints still matching the
/// typed prefix are shown, so typing narrows the field.
fn render_hints(model: &Model, frame: &mut Frame) {
    let area = frame.area();
    let input = model.hint_input.clone();
    let badge = Style::default()
        .fg(rgb(model.theme.hint_badge_fg()))
        .bg(rgb(model.theme.hint_badge_bg()))
        .add_modifier(Modifier::BOLD);
    let buf = frame.buffer_mut();
    for hint in hints(model) {
        if !hint.label.starts_with(&input) {
            continue;
        }
        for (i, ch) in hint.label.chars().enumerate() {
            let x = hint.col + i as u16;
            if x >= area.width || hint.row >= area.height {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, hint.row)) {
                cell.set_char(ch);
                cell.set_style(badge);
            }
        }
    }
}

/// Draw the visible slice of the page and the scrollbar into the content region.
fn render_content(model: &Model, frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let viewport = area.height as usize;
    let body = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(SCROLLBAR_COLS),
        height: area.height,
    };
    let end = model.scroll.saturating_add(viewport).min(model.page.len());
    let start = model.scroll.min(end);
    let text = to_ratatui_text(&model.page[start..end]);
    frame.render_widget(Paragraph::new(text), body);

    // Map the scrollbar over the SCROLL POSITIONS, not the line count: content
    // length is `max_scroll` so `position` spans the full `[0, max_scroll]` range
    // across the track. This makes the thumb top hit the track top at `scroll==0`
    // and the thumb bottom hit the track bottom at `scroll==max_scroll`. Setting
    // `content_length` to `page.len()` with `viewport_content_length` instead left
    // the thumb short of the bottom for pages only a little taller than the
    // viewport (the position denominator over-counted by the viewport height).
    let mut state = ScrollbarState::new(model.max_scroll(viewport).max(1)).position(model.scroll);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

/// Draw the current page's inline images into the rows the layout reserved
/// for them, and a status line for every slot that is not a picture.
///
/// A picture is drawn with [`SlicedImage`], which crops it to the area: an
/// image half-scrolled off the top is drawn from its middle rather than
/// jumping or spilling over the top bar. The position is signed for exactly
/// that reason.
fn render_images(model: &Model, images: &ImageStore, frame: &mut Frame, area: Rect) {
    if model.images.is_empty() || area.height == 0 || area.width == 0 {
        return;
    }
    let body = Rect {
        width: area.width.saturating_sub(SCROLLBAR_COLS),
        ..area
    };
    for slot in &model.images {
        let Some(link) = model.links.iter().find(|l| l.index == slot.link) else {
            continue;
        };
        // The reserved rows sit directly under the link's line.
        let first = link.line + 1;
        let y = first as i64 - model.scroll as i64;
        let x = link.col_start.min(u16::MAX as usize) as i16;

        match (&slot.state, images.protocol(slot.link)) {
            (SlotState::Ready { cells, .. }, Some(protocol)) => {
                // Entirely above or below the viewport: nothing to draw.
                if y + cells.1 as i64 <= 0 || y >= body.height as i64 {
                    continue;
                }
                let position =
                    SignedPosition::from((x, y.clamp(i16::MIN as i64, i16::MAX as i64) as i16));
                frame.render_widget(SlicedImage::new(protocol, position), body);
            }
            // Everything else is one line of text: queued, loading, failed, or
            // a terminal that cannot draw pictures at all.
            (state, _) => {
                if y < 0 || y >= body.height as i64 {
                    continue;
                }
                let row = body.y + y as u16;
                let text = crate::image::status_line(&slot.name, state);
                let style = match model.no_color {
                    true => Style::default().add_modifier(Modifier::DIM),
                    false => Style::default().fg(rgb(model.theme.chrome_muted_fg())),
                };
                let rect = Rect {
                    x: body.x + x as u16,
                    y: row,
                    width: body.width.saturating_sub(x as u16),
                    height: 1,
                };
                frame.render_widget(Paragraph::new(RtSpan::styled(text, style)), rect);
            }
        }
    }
}

/// Draw the one-row top bar: the page title, a `·`, and the address on the left,
/// then the right-aligned status cluster (bookmark star, cache bolt, hop count).
/// In address mode the address slot becomes the live editor.
fn render_topbar(model: &Model, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    // Fill the bar full width with the chrome style first; the title, address and
    // cluster render on top and inherit its background.
    frame.render_widget(
        Block::default().style(chrome_style(model.no_color, model.theme)),
        area,
    );
    let tb = topbar_layout(model, area);

    // Title in bold bright chrome fg.
    if !tb.title.is_empty() {
        let style = chrome_text_style(model.no_color, model.theme).add_modifier(Modifier::BOLD);
        frame.render_widget(
            Paragraph::new(RtSpan::styled(tb.title.clone(), style)),
            Rect {
                x: tb.title_x,
                y: area.y,
                width: UnicodeWidthStr::width(tb.title.as_str()) as u16,
                height: 1,
            },
        );
    }
    // The `·` separator in the muted chrome fg.
    if let Some(sx) = tb.sep_x {
        let style = chrome_muted_style(model.no_color, model.theme);
        frame.render_widget(
            Paragraph::new(RtSpan::styled(TOPBAR_SEP, style)),
            Rect {
                x: sx,
                y: area.y,
                width: UnicodeWidthStr::width(TOPBAR_SEP) as u16,
                height: 1,
            },
        );
    }

    // The address slot: the live editor in address mode, else the address text
    // with an input-field look (underlined) so it reads as editable.
    if model.mode == Mode::Address {
        render_address_editor(model, frame, tb.address_rect);
    } else if !tb.address.is_empty() {
        let style =
            chrome_text_style(model.no_color, model.theme).add_modifier(Modifier::UNDERLINED);
        frame.render_widget(
            Paragraph::new(RtSpan::styled(tb.address.clone(), style)),
            tb.address_rect,
        );
    }

    // Right-aligned status cluster. The star reads in the bright chrome fg (it is
    // a click target); the bolt and hop count in the muted chrome fg.
    let bright = chrome_text_style(model.no_color, model.theme);
    let muted = chrome_muted_style(model.no_color, model.theme);
    if let Some((glyph, rect)) = tb.star {
        frame.render_widget(Paragraph::new(RtSpan::styled(glyph, bright)), rect);
    }
    // The identity marker reads in the bright chrome fg: it is security-relevant
    // state, at least as prominent as the star, never muted.
    if let Some((text, x)) = &tb.identity {
        frame.render_widget(
            Paragraph::new(RtSpan::styled(text.clone(), bright)),
            Rect {
                x: *x,
                y: area.y,
                width: UnicodeWidthStr::width(text.as_str()) as u16,
                height: 1,
            },
        );
    }
    if let Some(x) = tb.cached_x {
        frame.render_widget(
            Paragraph::new(RtSpan::styled(CACHED_GLYPH, muted)),
            Rect {
                x,
                y: area.y,
                width: UnicodeWidthStr::width(CACHED_GLYPH) as u16,
                height: 1,
            },
        );
    }
    if let Some((text, x)) = &tb.hops {
        frame.render_widget(
            Paragraph::new(RtSpan::styled(text.clone(), muted)),
            Rect {
                x: *x,
                y: area.y,
                width: UnicodeWidthStr::width(text.as_str()) as u16,
                height: 1,
            },
        );
    }
}

/// Draw the live address editor over the address slot: a `:` prompt, then the
/// tui-input value with its cursor.
fn render_address_editor(model: &Model, frame: &mut Frame, area: Rect) {
    if area.width == 0 {
        return;
    }
    let style = chrome_text_style(model.no_color, model.theme);
    frame.render_widget(
        Paragraph::new(RtSpan::styled(":", style)),
        Rect { width: 1, ..area },
    );
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y,
        width: area.width.saturating_sub(1),
        height: 1,
    };
    if inner.width == 0 {
        return;
    }
    let w = inner.width as usize;
    let scroll = model.input.visual_scroll(w.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(RtSpan::styled(model.input.value().to_string(), style))
            .scroll((0, scroll as u16)),
        inner,
    );
    let cx = inner.x + (model.input.visual_cursor().saturating_sub(scroll)) as u16;
    frame.set_cursor_position((cx.min(inner.right().saturating_sub(1)), inner.y));
}

/// The address-slot breadcrumb for the current page, or a hint when nothing is
/// loaded yet.
fn breadcrumb(model: &Model) -> String {
    match model.history.current() {
        Some(target) => format!("{}:{}", short_hex(&target.dest_hash), target.path),
        None => "press : to enter a URL".to_string(),
    }
}

/// A short, glanceable form of a destination hash: the first 8 hex characters
/// followed by an ellipsis.
fn short_hex(dest: &[u8; 16]) -> String {
    let mut s = String::with_capacity(9);
    for byte in &dest[..4] {
        s.push_str(&format!("{byte:02x}"));
    }
    s.push('…');
    s
}

/// The six braille glyphs of the loading spinner: a single dot circling once per
/// full cycle. `spin % 6` picks the current frame.
const SPIN_FRAMES: [&str; 6] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"];

/// Degrees of hue the spinner colour advances per animation tick. The colour
/// glides continuously through the HSV wheel rather than snapping between a few
/// fixed rainbow stops. At the [`SPINNER_TICK_MS`] (120 ms) redraw cadence a full
/// 360 degree rainbow takes `360 / HUE_STEP` ticks = 40 ticks = 4.8 s, and each
/// tick shifts the hue by only 9 degrees, so the change reads as a smooth glide
/// with no flicker.
const HUE_STEP: f32 = 9.0;

/// Convert an HSV colour to 8-bit RGB. Pure function. `h_deg` is the hue in
/// degrees (wrapped into `[0, 360)`), `s` and `v` are the saturation and value in
/// `[0, 1]`. Uses the standard sextant formulation; the spinner calls it with
/// `s = v = 1.0` for maximally vivid, full-brightness colours.
fn hsv_to_rgb(h_deg: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = h_deg.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let sextant = h / 60.0;
    let x = c * (1.0 - (sextant.rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = match sextant as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to_u8 = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to_u8(r1), to_u8(g1), to_u8(b1))
}

/// Render one frame of the loading spinner: the current braille glyph as a bold,
/// full-brightness span whose colour rotates slowly and continuously through the
/// HSV hue wheel. The glyph advances one step per `spin` (the dot circles) while
/// the hue advances a small [`HUE_STEP`] per tick, so the colour glides smoothly
/// and much more slowly than the dot position.
fn spinner_span(spin: usize) -> RtSpan<'static> {
    let glyph = SPIN_FRAMES[spin % SPIN_FRAMES.len()];
    let hue = (spin as f32 * HUE_STEP).rem_euclid(360.0);
    let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
    RtSpan::styled(
        glyph,
        Style::default()
            .fg(Color::Rgb(r, g, b))
            .add_modifier(Modifier::BOLD),
    )
}

// --- the footer: a unified strip of clickable button / keybinding hints -----

/// What a footer button activates when pressed or clicked. Mode-specific keys
/// (`Esc`, `Tab`, `Space`, `Enter`, `n`, `N`) route back through the key handler
/// so the click behaves exactly like the key press it mirrors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterAction {
    Back,
    Forward,
    Reload,
    /// Toggle identifying to the current page's node (same as the `i` key).
    Identify,
    Hint,
    Search,
    Places,
    Mark,
    Copy,
    Help,
    Quit,
    /// Synthesize a plain key press (the mode-action buttons).
    Key(KeyCode),
    /// A non-interactive sentence (hint mode's instruction).
    None,
}

/// One footer button: a key you can press and a label, which are the same thing
/// you can also click. The key cell renders bright + bold, the label muted.
#[derive(Clone, Copy, Debug)]
struct FooterItem {
    /// The pressable key (bright + bold), e.g. `Alt-←`, `f`, `Esc`.
    key: &'static str,
    /// The muted descriptive label, e.g. `back`, `hint` (empty for a sentence).
    label: &'static str,
    /// Whether the key renders before the label. Everything leads with its key
    /// (`Alt-← back`, `f hint`).
    key_first: bool,
    /// What a press or click runs.
    action: FooterAction,
    /// Truncation priority: `None` never drops; `Some(rank)` drops when the row
    /// is too narrow, lowest rank first.
    drop_rank: Option<u8>,
    /// Whether the control is currently usable; an unavailable nav item reads
    /// muted rather than active.
    available: bool,
}

impl FooterItem {
    /// A never-dropped, always-available mode-action button.
    const fn action(key: &'static str, label: &'static str, act: FooterAction) -> Self {
        FooterItem {
            key,
            label,
            key_first: true,
            action: act,
            drop_rank: None,
            available: true,
        }
    }

    /// The same, but droppable at rank `r` when the row is too narrow.
    const fn droppable(key: &'static str, label: &'static str, act: FooterAction, r: u8) -> Self {
        FooterItem {
            drop_rank: Some(r),
            ..FooterItem::action(key, label, act)
        }
    }

    /// This item's rendered width, with labels (`false`) or collapsed to the key
    /// glyph only (`true`).
    fn width(&self, collapsed: bool) -> u16 {
        let kw = UnicodeWidthStr::width(self.key) as u16;
        if collapsed || self.label.is_empty() {
            return kw.max(UnicodeWidthStr::width(self.label) as u16);
        }
        let lw = UnicodeWidthStr::width(self.label) as u16;
        // key + one space + label (the key may be empty for a sentence).
        if kw == 0 {
            lw
        } else {
            kw + 1 + lw
        }
    }

    /// Whether this item is part of the navigation trio (rendered before the
    /// stronger divider).
    fn is_nav(&self) -> bool {
        matches!(
            self.action,
            FooterAction::Back | FooterAction::Forward | FooterAction::Reload
        )
    }
}

/// The footer's navigation trio, always first: back, forward, reload. They are
/// ordinary `key + label` items like the rest of the strip, spelling their keys
/// (`Alt-←`, `Alt-→`, `R`) rather than using glyphs. Their availability tracks
/// the history so an impossible motion reads muted.
fn footer_nav(model: &Model) -> [FooterItem; 3] {
    [
        FooterItem {
            available: model.history.can_back(),
            ..FooterItem::action("Alt-←", "back", FooterAction::Back)
        },
        FooterItem {
            available: model.history.can_forward(),
            ..FooterItem::action("Alt-→", "forward", FooterAction::Forward)
        },
        FooterItem {
            available: model.history.current().is_some(),
            ..FooterItem::action("R", "reload", FooterAction::Reload)
        },
    ]
}

/// The footer button list for the current mode: the nav trio, then the
/// mode-dependent action set. The first non-nav item carries the stronger
/// divider before it.
fn footer_items(model: &Model) -> Vec<FooterItem> {
    let mut items: Vec<FooterItem> = footer_nav(model).to_vec();
    match model.mode {
        Mode::Browse => {
            // The identify toggle, only when there is a page to identify to. A
            // state action, so under truncation it outlives the hint/search/
            // places/mark/copy conveniences (highest drop rank, right below the
            // never-dropped reload).
            if model.current_dest.is_some() {
                let label = if model.identifying {
                    "anonymous"
                } else {
                    "identify"
                };
                items.push(FooterItem::droppable("i", label, FooterAction::Identify, 6));
            }
            items.extend_from_slice(&[
                FooterItem::droppable("f", "hint", FooterAction::Hint, 5),
                FooterItem::droppable("/", "search", FooterAction::Search, 4),
                FooterItem::droppable("d", "places", FooterAction::Places, 3),
                FooterItem::droppable("m", "mark", FooterAction::Mark, 2),
                FooterItem::droppable("y", "copy", FooterAction::Copy, 1),
                FooterItem::action("?", "help", FooterAction::Help),
                FooterItem::action("q", "quit", FooterAction::Quit),
            ]);
        }
        Mode::Field => items.extend_from_slice(&[
            FooterItem::action("Esc", "done", FooterAction::Key(KeyCode::Esc)),
            FooterItem::droppable("Tab", "next field", FooterAction::Key(KeyCode::Tab), 1),
            FooterItem::droppable("Space", "toggle", FooterAction::Key(KeyCode::Char(' ')), 2),
            FooterItem::action("?", "help", FooterAction::Help),
        ]),
        Mode::Search => items.extend_from_slice(&[
            FooterItem::action("Enter", "go", FooterAction::Key(KeyCode::Enter)),
            FooterItem::droppable("n", "next", FooterAction::Key(KeyCode::Char('n')), 2),
            FooterItem::droppable("N", "prev", FooterAction::Key(KeyCode::Char('N')), 3),
            FooterItem::action("Esc", "cancel", FooterAction::Key(KeyCode::Esc)),
            FooterItem::droppable("?", "help", FooterAction::Help, 1),
        ]),
        Mode::Address => items.extend_from_slice(&[
            FooterItem::action("Enter", "go", FooterAction::Key(KeyCode::Enter)),
            FooterItem::action("Esc", "cancel", FooterAction::Key(KeyCode::Esc)),
            FooterItem::droppable("?", "help", FooterAction::Help, 1),
        ]),
        Mode::Hint => items.extend_from_slice(&[
            FooterItem {
                key: "",
                label: HINT_SENTENCE,
                key_first: true,
                action: FooterAction::None,
                drop_rank: None,
                available: true,
            },
            FooterItem::action("Esc", "cancel", FooterAction::Key(KeyCode::Esc)),
        ]),
    }
    items
}

/// The width of the divider drawn before the item at `kept`-position `pos`: a
/// ` │ ` between items (a single space in collapsed mode, except at the stronger
/// nav/mode boundary). Position 0 has no leading divider.
fn footer_divider_width(pos: usize, is_strong: bool, collapsed: bool) -> u16 {
    if pos == 0 {
        0
    } else if !collapsed || is_strong {
        3 // " │ "
    } else {
        1 // a single space between collapsed glyphs
    }
}

/// A laid-out footer button: the original item, whether its label is shown, and
/// its screen rect. Drives rendering, click hit-testing and hover highlight.
#[derive(Clone, Copy, Debug)]
struct FooterSlot {
    item: FooterItem,
    /// Index into [`footer_items`] for the current mode (stable per mode), so a
    /// hover can be remembered across the move-then-render round trip.
    index: usize,
    /// Whether the divider before this slot is the stronger nav/mode one.
    strong_divider: bool,
    /// The width of the divider gap before this slot (0 for the first slot).
    div_before: u16,
    collapsed: bool,
    rect: Rect,
}

/// Lay the footer buttons out across `area`, dropping and then collapsing by
/// priority so the row never overflows. Returns the kept slots (in order) with
/// their screen rects.
fn footer_layout(model: &Model, area: Rect) -> Vec<FooterSlot> {
    let items = footer_items(model);

    // Total width of a kept subset, laid with the given collapse.
    let total_width = |kept: &[usize], collapsed: bool| -> u16 {
        let mut w = 0u16;
        for (pos, &i) in kept.iter().enumerate() {
            let strong = footer_strong_before(&items, kept, pos);
            w = w.saturating_add(footer_divider_width(pos, strong, collapsed));
            w = w.saturating_add(items[i].width(collapsed));
        }
        w
    };

    // Drop droppable items (lowest rank first) until the subset fits.
    let shrink = |collapsed: bool| -> Vec<usize> {
        let mut kept: Vec<usize> = (0..items.len()).collect();
        while total_width(&kept, collapsed) > area.width {
            let victim = kept
                .iter()
                .copied()
                .filter(|&i| items[i].drop_rank.is_some())
                .min_by_key(|&i| items[i].drop_rank.unwrap_or(u8::MAX));
            match victim {
                Some(v) => kept.retain(|&i| i != v),
                None => break,
            }
        }
        kept
    };

    // Prefer full labels; only when even the non-droppable core overflows do we
    // collapse to keys (re-adding items, since keys keep more of them).
    let (kept, collapsed) = {
        let labelled = shrink(false);
        if total_width(&labelled, false) <= area.width {
            (labelled, false)
        } else {
            (shrink(true), true)
        }
    };

    // Assign rects left to right.
    let mut out = Vec::with_capacity(kept.len());
    let mut x = area.x;
    for (pos, &i) in kept.iter().enumerate() {
        let strong = footer_strong_before(&items, &kept, pos);
        let dw = footer_divider_width(pos, strong, collapsed);
        x = x.saturating_add(dw);
        let w = items[i].width(collapsed);
        out.push(FooterSlot {
            item: items[i],
            index: i,
            strong_divider: strong,
            div_before: dw,
            collapsed,
            rect: Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
        });
        x = x.saturating_add(w);
    }
    out
}

/// Whether the divider before the kept item at `pos` is the stronger nav/mode
/// boundary: true for the first non-nav item that follows a nav item.
fn footer_strong_before(items: &[FooterItem], kept: &[usize], pos: usize) -> bool {
    if pos == 0 {
        return false;
    }
    let cur = &items[kept[pos]];
    let prev = &items[kept[pos - 1]];
    prev.is_nav() && !cur.is_nav()
}

/// Whether the footer currently shows its button strip. It now always does,
/// except in search mode where the status bar becomes the `/<query>` editor: the
/// loading spinner and a focused/hovered link's target float in the bottom-left
/// [`render_status_chip`] over the content instead of covering the buttons.
fn footer_shows_buttons(model: &Model) -> bool {
    model.mode != Mode::Search
}

/// Run a footer button's action, mirroring the key it stands for.
fn run_footer_action(model: &mut Model, action: FooterAction) -> Vec<Effect> {
    match action {
        FooterAction::Back => go_back(model),
        FooterAction::Forward => go_forward(model),
        FooterAction::Reload => reload_current(model),
        FooterAction::Identify => toggle_identify(model),
        FooterAction::Hint => {
            enter_hint(model);
            Vec::new()
        }
        FooterAction::Search => {
            enter_search(model);
            Vec::new()
        }
        FooterAction::Places => {
            toggle_places(model);
            Vec::new()
        }
        FooterAction::Mark => toggle_bookmark_current(model),
        FooterAction::Copy => yank_url(model),
        FooterAction::Help => {
            model.show_help = !model.show_help;
            if model.show_help {
                model.help_scroll = 0;
            }
            Vec::new()
        }
        FooterAction::Quit => {
            model.quit = true;
            vec![Effect::Quit]
        }
        FooterAction::Key(code) => update_key(model, KeyEvent::new(code, KeyModifiers::NONE)),
        FooterAction::None => Vec::new(),
    }
}

/// Draw the status bar: always the unified footer button strip, so the reader's
/// clickable controls are never covered. In search mode it is the live `/<query>`
/// editor instead. The loading spinner and a focused/hovered link's target float
/// in the bottom-left [`render_status_chip`] over the content; transient messages
/// float as a [`render_toast`] overlay.
fn render_status(model: &Model, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    // Fill the status bar full width with the chrome style; the buttons render on
    // top and inherit its background.
    frame.render_widget(
        Block::default().style(chrome_style(model.no_color, model.theme)),
        area,
    );
    // In search mode the status bar becomes the `/<query>` editor.
    if model.mode == Mode::Search {
        render_search_bar(model, frame, area);
        return;
    }
    render_footer_strip(model, frame, area);
}

/// Draw the bottom-left floating status chip: a one-line field on the content's
/// last row showing the CURRENT pointer/page state, floating over the page just
/// above the footer so the footer's clickable buttons are never covered. In
/// priority order it shows, while a fetch is in flight, the spinner and the
/// loading path; else, when a link is focused (Tab) or hovered (mouse), that
/// link's target (`→ <address>`); else nothing at all. Transient events use
/// [`render_toast`] (bottom-right) instead.
fn render_status_chip(model: &Model, frame: &mut Frame, area: Rect) {
    let [_topbar, content, _status] = regions(area);
    if content.width == 0 || content.height == 0 {
        return;
    }
    // One column of padding each side; the body is truncated to whatever inner
    // width is left so the field never overflows the content width.
    let pad = 1u16;
    let inner_budget = content.width.saturating_sub(2 * pad) as usize;
    if inner_budget == 0 {
        return;
    }
    let text_style = chrome_muted_style(model.no_color, model.theme);
    // Priority: a fetch in flight wins over a focused/hovered link; idle draws
    // nothing at all and leaves the content row untouched.
    let (spans, inner_w): (Vec<RtSpan<'static>>, u16) = if let Some(pending) = &model.pending {
        let label = truncate_to_cols(
            &format!(" loading {}", pending.target.path),
            inner_budget.saturating_sub(1),
        );
        let w = 1 + UnicodeWidthStr::width(label.as_str()) as u16;
        (
            vec![spinner_span(model.spin), RtSpan::styled(label, text_style)],
            w,
        )
    } else if let Some(name) = &model.download {
        let label = truncate_to_cols(
            &format!(" downloading {name}"),
            inner_budget.saturating_sub(1),
        );
        let w = 1 + UnicodeWidthStr::width(label.as_str()) as u16;
        (
            vec![spinner_span(model.spin), RtSpan::styled(label, text_style)],
            w,
        )
    } else if let Some(target) = focused_link_target(model) {
        let body = truncate_to_cols(&format!("→ {target}"), inner_budget);
        let w = UnicodeWidthStr::width(body.as_str()) as u16;
        (vec![RtSpan::styled(body, text_style)], w)
    } else {
        return;
    };
    let field_w = (inner_w + 2 * pad).min(content.width);
    let field_y = content.bottom().saturating_sub(1);
    let chip_bg = chrome_style(model.no_color, model.theme);
    // A thin top edge one row above the field sets the card off from the page and
    // from the footer beneath it, without a full border box. Theme-aware; under
    // `no_color` the reverse-video chrome fill plus an underline reads as an edge.
    if content.height >= 2 {
        let edge = Rect {
            x: content.x,
            y: field_y.saturating_sub(1),
            width: field_w,
            height: 1,
        };
        let edge_style = if model.no_color {
            chip_bg.add_modifier(Modifier::UNDERLINED)
        } else {
            chip_bg.patch(Style::default().fg(rgb(model.theme.chrome_muted_fg())))
        };
        frame.render_widget(Clear, edge);
        frame.render_widget(
            Paragraph::new(RtSpan::styled("▔".repeat(field_w as usize), edge_style)),
            edge,
        );
    }
    // The one-line field itself: chrome background, one-column left padding, the
    // spinner / target painted on top (same Clear-then-paint technique as the toast).
    let field = Rect {
        x: content.x,
        y: field_y,
        width: field_w,
        height: 1,
    };
    frame.render_widget(Clear, field);
    frame.render_widget(Block::default().style(chip_bg), field);
    let inner = Rect {
        x: field.x.saturating_add(pad),
        y: field.y,
        width: field.width.saturating_sub(2 * pad),
        height: 1,
    };
    frame.render_widget(Paragraph::new(RtLine::from(spans)), inner);
}

/// Draw the footer button strip: each button's key in the bright chrome fg and
/// bold, its label in the muted chrome fg, a `│` divider between buttons (a
/// brighter one at the nav/mode boundary), and a bold + underline highlight over
/// the hovered button so it reads as clickable.
fn render_footer_strip(model: &Model, frame: &mut Frame, area: Rect) {
    let key_style = chrome_text_style(model.no_color, model.theme).add_modifier(Modifier::BOLD);
    let label_style = chrome_muted_style(model.no_color, model.theme);
    let dim_div = chrome_muted_style(model.no_color, model.theme);
    let strong_div = chrome_text_style(model.no_color, model.theme);

    let slots = footer_layout(model, area);
    for slot in &slots {
        // The divider before this button (none before the first).
        let dw = slot.div_before;
        if dw > 0 {
            let (glyph, style) = if slot.strong_divider {
                ("│", strong_div)
            } else {
                ("│", dim_div)
            };
            // Centre the │ in its 1-or-3-column gap.
            let gx = slot.rect.x.saturating_sub(dw).saturating_add(dw / 2);
            frame.render_widget(
                Paragraph::new(RtSpan::styled(glyph, style)),
                Rect {
                    x: gx,
                    y: area.y,
                    width: 1,
                    height: 1,
                },
            );
        }

        // The button itself: key bright + bold, label muted; an unavailable nav
        // button reads wholly muted.
        let (kstyle, lstyle) = if slot.item.available {
            (key_style, label_style)
        } else {
            (label_style, label_style)
        };
        let mut spans: Vec<RtSpan<'static>> = Vec::new();
        let key = slot.item.key;
        let label = slot.item.label;
        let show_label = !slot.collapsed && !label.is_empty();
        if slot.item.key_first {
            if !key.is_empty() {
                spans.push(RtSpan::styled(key, kstyle));
            }
            if show_label {
                if !key.is_empty() {
                    spans.push(RtSpan::styled(" ", lstyle));
                }
                spans.push(RtSpan::styled(label, lstyle));
            }
        } else {
            if show_label {
                spans.push(RtSpan::styled(label, lstyle));
                spans.push(RtSpan::styled(" ", lstyle));
            }
            if !key.is_empty() {
                spans.push(RtSpan::styled(key, kstyle));
            }
        }
        frame.render_widget(Paragraph::new(RtLine::from(spans)), slot.rect);
    }

    // Highlight the hovered button, reusing the link-hover look (bold + underline
    // patched over the button's own styling), so it reads as a button.
    if let Some(idx) = model.footer_hover {
        if let Some(slot) = slots.iter().find(|s| s.index == idx) {
            let area_all = frame.area();
            let buf = frame.buffer_mut();
            let hover = Modifier::BOLD | Modifier::UNDERLINED;
            for x in slot.rect.x..slot.rect.right() {
                if x >= area_all.width || slot.rect.y >= area_all.height {
                    continue;
                }
                if let Some(cell) = buf.cell_mut((x, slot.rect.y)) {
                    let style = cell.style().patch(Style::default().add_modifier(hover));
                    cell.set_style(style);
                }
            }
        }
    }
}

/// Draw the in-page search editor into the status bar: a `/` prompt, then the
/// live query with its cursor, styled to read on the chrome bar.
fn render_search_bar(model: &Model, frame: &mut Frame, area: Rect) {
    if area.width == 0 {
        return;
    }
    let text_style = chrome_text_style(model.no_color, model.theme);
    frame.render_widget(
        Paragraph::new(RtSpan::styled("/", text_style)),
        Rect { width: 1, ..area },
    );
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y,
        width: area.width.saturating_sub(1),
        height: 1,
    };
    if inner.width == 0 {
        return;
    }
    let w = inner.width as usize;
    let scroll = model.search_input.visual_scroll(w.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(RtSpan::styled(
            model.search_input.value().to_string(),
            text_style,
        ))
        .scroll((0, scroll as u16)),
        inner,
    );
    let cx = inner.x + (model.search_input.visual_cursor().saturating_sub(scroll)) as u16;
    frame.set_cursor_position((cx.min(inner.right().saturating_sub(1)), inner.y));
}

/// The status-bar text for the focused-or-hovered link's target, if any: the
/// current node's short hash and the link's path (`node · :/page/x.mu`).
fn focused_link_target(model: &Model) -> Option<String> {
    let idx = model.focus.or(model.hover)?;
    let link = model.links.iter().find(|l| l.index == idx)?;
    // A same-destination link's target begins with ':'; prefix the current node
    // so the reader sees which node it resolves against.
    match model.current_dest {
        Some(dest) if link.target.starts_with(':') => {
            Some(format!("{} · {}", short_hex(&dest), link.target))
        }
        _ => Some(link.target.clone()),
    }
}

/// One keybinding shown in the help overlay: the keys and their description. The
/// description column is COMPUTED from the widest `keys` across all groups (see
/// [`help_lines`]), never hand-padded, so it can never misalign.
struct HelpEntry {
    keys: &'static str,
    desc: &'static str,
}

/// A titled group of [`HelpEntry`]s in the help overlay.
struct HelpGroup {
    title: &'static str,
    entries: Vec<HelpEntry>,
}

/// The indent applied to every entry (its keys and description) under a group.
const HELP_INDENT: &str = "  ";
/// The gap between the (padded) keys column and the description column.
const HELP_GAP: usize = 2;

/// The keybinding groups shown in the help overlay. The Move group's scroll rows
/// are pulled straight from [`SCROLL_KEYS`], so the overlay can never list a
/// scroll binding that the key handler does not honour, nor omit one it does.
fn help_groups() -> Vec<HelpGroup> {
    // The scroll rows come straight from the keymap table; the mouse wheel is the
    // one Move affordance that is not a key chord, so it is appended here.
    let mut move_entries: Vec<HelpEntry> = SCROLL_KEYS
        .iter()
        .map(|s| HelpEntry {
            keys: s.keys,
            desc: s.desc,
        })
        .collect();
    move_entries.push(HelpEntry {
        keys: "wheel",
        desc: "scroll",
    });
    vec![
        HelpGroup {
            title: "Move",
            entries: move_entries,
        },
        HelpGroup {
            title: "Follow",
            entries: vec![
                HelpEntry {
                    keys: "Tab / Shift-Tab",
                    desc: "focus a link or field",
                },
                HelpEntry {
                    keys: "Enter / click",
                    desc: "follow a link or footer button",
                },
                HelpEntry {
                    keys: "f",
                    desc: "hint — type a label or the link text",
                },
                HelpEntry {
                    keys: "Alt-← / Alt-→   mouse side buttons",
                    desc: "back / forward",
                },
                HelpEntry {
                    keys: "hover a link",
                    desc: "show its URL",
                },
            ],
        },
        HelpGroup {
            title: "Page",
            entries: vec![
                HelpEntry {
                    keys: ": / click the URL",
                    desc: "enter a URL",
                },
                HelpEntry {
                    keys: "R / Ctrl-R / F5",
                    desc: "reload (always refetches)",
                },
                HelpEntry {
                    keys: "Esc / Ctrl-g",
                    desc: "cancel a load",
                },
                HelpEntry {
                    keys: "m / click star",
                    desc: "bookmark this page",
                },
                HelpEntry {
                    keys: "y",
                    desc: "copy the link's / page's URL",
                },
                HelpEntry {
                    keys: "d / click a place",
                    desc: "places panel — click an entry to open it",
                },
                HelpEntry {
                    keys: "i",
                    desc: "identify / go anonymous to this node",
                },
                HelpEntry {
                    keys: "",
                    desc: "default anonymous; identifying lets this node attribute your posts",
                },
                HelpEntry {
                    keys: "/ n N",
                    desc: "search / next / prev",
                },
            ],
        },
        HelpGroup {
            title: "Images",
            entries: vec![
                HelpEntry {
                    keys: "Enter",
                    desc: "save the focused picture (no second transfer)",
                },
                HelpEntry {
                    keys: "o",
                    desc: "open it in the system viewer",
                },
                HelpEntry {
                    keys: "",
                    desc: "start with --images off to leave them as plain links",
                },
            ],
        },
        HelpGroup {
            title: "Fields",
            entries: vec![HelpEntry {
                keys: "type / Space / Esc",
                desc: "edit / toggle / leave",
            }],
        },
        HelpGroup {
            title: "App",
            entries: vec![
                HelpEntry {
                    keys: "t",
                    desc: "light / dark theme",
                },
                HelpEntry {
                    keys: "? / Esc / click",
                    desc: "close this help",
                },
                HelpEntry {
                    keys: "q / Ctrl-c",
                    desc: "quit",
                },
            ],
        },
    ]
}

/// The widest `keys` display width across every entry of every group: the width
/// the keys column is padded to so all descriptions align.
fn help_key_col(groups: &[HelpGroup]) -> usize {
    groups
        .iter()
        .flat_map(|g| g.entries.iter())
        .map(|e| UnicodeWidthStr::width(e.keys))
        .max()
        .unwrap_or(0)
}

/// The plain-text lines of the help overlay: each group's title, then its
/// entries with every description aligned to the computed keys column, with a
/// blank line between groups. The renderer and the layout both derive from this,
/// so widths can never disagree and no padding is hand-counted. The block's own
/// border title carries the heading, so no heading line lives in the body.
fn help_lines() -> Vec<String> {
    let groups = help_groups();
    let col = help_key_col(&groups);
    let mut lines = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            lines.push(String::new());
        }
        lines.push(group.title.to_string());
        for e in &group.entries {
            let pad = col.saturating_sub(UnicodeWidthStr::width(e.keys)) + HELP_GAP;
            lines.push(format!(
                "{HELP_INDENT}{}{}{}",
                e.keys,
                " ".repeat(pad),
                e.desc
            ));
        }
    }
    lines
}

/// Draw the centered places panel: a chrome-styled overlay with a "Bookmarks"
/// section and a "Discovered nodes" section, the selected row reverse-video
/// highlighted. Both the fill and the border use the theme's chrome colours, so
/// the panel tracks the light/dark theme.
fn render_places(model: &Model, frame: &mut Frame, area: Rect) {
    let entries = places(model);
    let bm_count = model.bookmarks.len();
    let text_style = chrome_text_style(model.no_color, model.theme);
    let muted = chrome_muted_style(model.no_color, model.theme);
    let header = text_style.add_modifier(Modifier::BOLD);
    let selected = Style::default().add_modifier(Modifier::REVERSED);

    // The centred overlay box, shared with the click / hover hit-test so both
    // agree on where the panel sits. Its width fixes the inner display width (the
    // two border columns removed) rows are truncated to: a long bookmark row that
    // reached the border let the selected row's reversed bar spill past it;
    // truncating to the inner width keeps every row strictly inside the border.
    let overlay = places_overlay_rect(model, area);
    let inner = overlay.width.saturating_sub(2) as usize;

    let mut lines: Vec<RtLine<'static>> = Vec::new();
    lines.push(RtLine::from(RtSpan::styled("Bookmarks", header)));
    if bm_count == 0 {
        lines.push(RtLine::from(RtSpan::styled("  (none)", muted)));
    } else {
        for (i, place) in entries.iter().enumerate().take(bm_count) {
            lines.push(place_line(
                place,
                i,
                model.places_sel,
                model.places_hover,
                text_style,
                selected,
                inner,
            ));
        }
    }
    lines.push(RtLine::from(""));
    lines.push(RtLine::from(RtSpan::styled("Discovered nodes", header)));
    if entries.len() == bm_count {
        // Not "(none)": discovery is always running, so an empty list means
        // nothing has announced YET. Saying so keeps a first run — where this
        // panel opens by itself and both sections are empty — from reading as a
        // broken feature rather than a quiet mesh.
        lines.push(RtLine::from(RtSpan::styled(NO_NODES_YET, muted)));
    } else {
        for (i, place) in entries.iter().enumerate().skip(bm_count) {
            lines.push(place_line(
                place,
                i,
                model.places_sel,
                model.places_hover,
                text_style,
                selected,
                inner,
            ));
        }
    }

    // The lines built above interleave the two section titles, an "(none)"
    // placeholder or the entry rows, and one blank spacer — exactly the order
    // `places_layout` models, so the entry -> line mapping there (and the overlay
    // height derived from it) stays exact.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" places ")
        .style(chrome_style(model.no_color, model.theme));
    frame.render_widget(Clear, overlay);

    // The block's inner height is the viewport; when the list is taller than the
    // terminal, draw the slice starting at `places_scroll` (clamped to keep the
    // selected row visible on the update side) and paint a scrollbar on the right
    // border. Same shared treatment as the help overlay.
    let total = lines.len();
    let inner_h = overlay.height.saturating_sub(2) as usize;
    let max = total.saturating_sub(inner_h);
    let offset = model.places_scroll.min(max);
    let visible: Vec<RtLine> = lines.into_iter().skip(offset).take(inner_h).collect();
    frame.render_widget(Paragraph::new(Text::from(visible)).block(block), overlay);
    if max > 0 {
        let mut state = ScrollbarState::new(max).position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(scrollbar, overlay.inner(Margin::new(0, 1)), &mut state);
    }
}

/// One places-panel row: its label, reverse-video highlighted when it is the
/// selected row. A selected or hovered BOOKMARK row additionally carries a
/// right-aligned `×` delete marker on the last inner cell ([`marks_x`]).
fn place_line(
    place: &Place,
    idx: usize,
    sel: usize,
    hover: Option<usize>,
    normal: Style,
    selected: Style,
    inner: usize,
) -> RtLine<'static> {
    // Selection wins over hover, and the two styles differ (reverse video vs the
    // bold + underline the links and footer buttons use on hover), so a hovered
    // row stays distinguishable from the selected row even when they coincide.
    let style = if idx == sel {
        selected
    } else if hover == Some(idx) {
        normal.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        normal
    };
    let x_col = place_x_col(inner).filter(|_| marks_x(place, idx, sel, hover));
    let Some(x_col) = x_col else {
        let label = truncate_to_cols(&place_label(place), inner);
        return RtLine::from(RtSpan::styled(label, style));
    };
    // The marker's cell plus a one-column gap are reserved: the label truncates
    // before them, so it can never overlap the `×` or spill past the border.
    let label = truncate_to_cols(&place_label(place), x_col.saturating_sub(1));
    // The pad is measured with ratatui's ADDRESSING width (see `fit_width`'s
    // position-vs-paint note), so the `×` lands exactly on the cell
    // `place_x_screen_col` hit-tests.
    let pad = x_col.saturating_sub(UnicodeWidthStr::width(label.as_str()));
    RtLine::from(vec![
        RtSpan::styled(label, style),
        RtSpan::raw(" ".repeat(pad)),
        RtSpan::styled("×", normal),
    ])
}

/// The one-line label for a place: a bookmark's title and URL, or a node's name,
/// short hash and hop count.
fn place_label(place: &Place) -> String {
    match place {
        Place::Bookmark { url, title } => {
            if title.is_empty() {
                format!("  {url}")
            } else {
                format!("  {title}  ·  {url}")
            }
        }
        Place::Node {
            dest_hash,
            name,
            hops,
            ..
        } => {
            let label = name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| short_hex(dest_hash));
            let hop = hops
                .map(|h| format!("{h} hops"))
                .unwrap_or_else(|| "? hops".to_string());
            format!("  {label}  ·  {}  ·  {hop}", short_hex(dest_hash))
        }
    }
}

/// Draw the transient toast: a small bordered box floated at the bottom-right of
/// the content (never over the status bar), coloured by kind. An error is an
/// attention red with a `⚠`; an info note is a calm green with a `✓`. Under
/// `no_color` both fall back to reverse video. A no-op when no toast is up.
fn render_toast(model: &Model, frame: &mut Frame, area: Rect) {
    let Some(toast) = &model.toast else {
        return;
    };
    let [_topbar, content, _status] = regions(area);
    // Too little room to float a bordered box: skip rather than draw garbage.
    if content.width < 8 || content.height < 3 {
        return;
    }
    let (glyph, accent) = match toast.kind {
        ToastKind::Error => ('⚠', Color::Rgb(255, 95, 95)),
        ToastKind::Info => ('✓', Color::Rgb(120, 220, 120)),
    };
    let style = if model.no_color {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(accent)
    };
    // A one-column margin from the content's right edge, room for both borders,
    // and the message truncated to whatever inner width is left.
    let margin = 1u16;
    let max_inner = content.width.saturating_sub(2 + 2 * margin).max(1) as usize;
    let body = truncate_to_cols(&format!("{glyph} {}", toast.text), max_inner);
    let inner_w = UnicodeWidthStr::width(body.as_str()) as u16;
    let box_w = inner_w + 2;
    let box_h = 3u16;
    // Bottom-right of the content, sitting just above the status bar.
    let overlay = Rect {
        x: content.right().saturating_sub(box_w + margin),
        y: content.bottom().saturating_sub(box_h),
        width: box_w,
        height: box_h,
    };
    let block = Block::default().borders(Borders::ALL).style(style);
    frame.render_widget(Clear, overlay);
    frame.render_widget(
        Paragraph::new(RtSpan::styled(body, style)).block(block),
        overlay,
    );
}

/// Draw the centered help overlay: the keybindings grouped under dim titles,
/// each entry's keys in the bright chrome fg and BOLD and its description in the
/// muted chrome fg — the same visual language as the footer strip, theme-aware
/// and `no_color`-safe. The block's border title carries the heading and the
/// close hints, so the body has no heading line.
fn render_help(model: &Model, frame: &mut Frame, area: Rect) {
    let groups = help_groups();
    let col = help_key_col(&groups);

    // Group titles are dim; under `no_color` the reverse-video fill already
    // delineates them, so drop the DIM (no DIM under reverse) and let the plain
    // style show through.
    let muted = chrome_muted_style(model.no_color, model.theme);
    let title_style = if model.no_color {
        muted
    } else {
        muted.add_modifier(Modifier::DIM)
    };
    let keys_style = chrome_text_style(model.no_color, model.theme).add_modifier(Modifier::BOLD);
    let desc_style = muted;

    // Build the styled lines. Each entry's keys cell and its (padded) description
    // reproduce `help_lines` exactly, so the styled overlay and its plain-text
    // projection stay byte-for-byte aligned.
    let mut lines: Vec<RtLine> = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            lines.push(RtLine::from(""));
        }
        lines.push(RtLine::from(RtSpan::styled(
            group.title.to_string(),
            title_style,
        )));
        for e in &group.entries {
            let pad = col.saturating_sub(UnicodeWidthStr::width(e.keys)) + HELP_GAP;
            lines.push(RtLine::from(vec![
                RtSpan::styled(format!("{HELP_INDENT}{}", e.keys), keys_style),
                RtSpan::styled(format!("{}{}", " ".repeat(pad), e.desc), desc_style),
            ]));
        }
    }

    // Size the overlay to the widest line plus its two borders, so no line wraps
    // in a normal-width terminal. `Wrap` below stays as a safety net for
    // terminals too narrow to hold the widest line. The plain projection gives
    // the widths so the sizing can never drift from the rendered content.
    let inner = help_lines()
        .iter()
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0) as u16;
    let width = (inner + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help — Esc, ? or click to close ")
        .style(chrome_style(model.no_color, model.theme));
    frame.render_widget(Clear, overlay);

    // The block's inner height is the viewport; the help lines are the content.
    // When the terminal is too short to hold them all, draw the slice starting at
    // `help_scroll` (clamped) and paint a scrollbar on the right border. The
    // shared `scrolled` rule keeps the offset in range, so slicing is always
    // sound.
    let total = lines.len();
    let inner_h = height.saturating_sub(2) as usize;
    let max = total.saturating_sub(inner_h);
    let offset = model.help_scroll.min(max);
    let visible: Vec<RtLine> = lines.into_iter().skip(offset).take(inner_h).collect();
    frame.render_widget(
        Paragraph::new(Text::from(visible))
            .block(block)
            .wrap(Wrap { trim: false }),
        overlay,
    );
    if max > 0 {
        let mut state = ScrollbarState::new(max).position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(scrollbar, overlay.inner(Margin::new(0, 1)), &mut state);
    }
}

/// The help overlay's scroll geometry for the current terminal size: the number
/// of help lines that fit at once (the viewport) and the largest valid
/// [`Model::help_scroll`] (`max`). Shared by the key/wheel handler (to clamp the
/// offset via [`scrolled`]) and the renderer (to slice the visible lines), so
/// the two can never disagree on the bounds. Mirrors `render_help`'s sizing: the
/// overlay is at most the full terminal height, two rows go to the border.
fn help_scroll_geometry(model: &Model) -> (usize, usize) {
    let total = help_lines().len();
    let rows = model.size.1 as usize;
    let height = (total + 2).min(rows);
    let viewport = height.saturating_sub(2);
    (viewport, total.saturating_sub(viewport))
}

/// Map laid-out styled lines to a ratatui [`Text`], grouping runs of equal
/// style into one span each (the same run-grouping the ANSI sink uses).
pub fn to_ratatui_text(lines: &[RLine]) -> Text<'static> {
    let mut out_lines: Vec<RtLine<'static>> = Vec::with_capacity(lines.len());
    for line in lines {
        let mut spans: Vec<RtSpan<'static>> = Vec::new();
        let mut i = 0;
        while i < line.cells.len() {
            let st = line.cells[i].st;
            let mut j = i;
            let mut text = String::new();
            while j < line.cells.len() && line.cells[j].st == st {
                text.push(line.cells[j].ch);
                j += 1;
            }
            spans.push(RtSpan::styled(text, rstyle_to_ratatui(st)));
            i = j;
        }
        out_lines.push(RtLine::from(spans));
    }
    Text::from(out_lines)
}

/// Translate a resolved [`RStyle`] into a ratatui [`Style`]: RGB colours map
/// directly, and bold/underline/italic map to the matching modifiers.
fn rstyle_to_ratatui(st: RStyle) -> Style {
    let mut style = Style::default();
    if let Some((r, g, b)) = st.fg {
        style = style.fg(Color::Rgb(r, g, b));
    }
    if let Some((r, g, b)) = st.bg {
        style = style.bg(Color::Rgb(r, g, b));
    }
    if st.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if st.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if st.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    style
}

/// The terminal side-effects the IO shell needs, abstracted so a test can
/// substitute a mock and assert that `restore` runs, with no real terminal.
pub trait TerminalOps {
    /// Enter the UI: alternate screen, raw mode, mouse capture.
    fn enter(&mut self) -> io::Result<()>;
    /// Restore the terminal to how it was before [`enter`](TerminalOps::enter).
    fn restore(&mut self) -> io::Result<()>;
}

/// The real crossterm terminal: alt-screen + raw mode + mouse capture on
/// [`enter`](TerminalOps::enter), reversed on [`restore`](TerminalOps::restore).
pub struct CrosstermTerminal {
    out: Stdout,
}

impl CrosstermTerminal {
    /// A terminal driver writing to stdout.
    pub fn new() -> Self {
        Self { out: io::stdout() }
    }
}

impl Default for CrosstermTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalOps for CrosstermTerminal {
    fn enter(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(self.out, EnterAlternateScreen, EnableMouseCapture)
    }

    fn restore(&mut self) -> io::Result<()> {
        execute!(self.out, LeaveAlternateScreen, DisableMouseCapture)?;
        disable_raw_mode()
    }
}

/// A RAII guard that restores the terminal on drop. Constructing it enters the
/// UI; dropping it (normally, on `?`, or during a panic unwind) restores it, so
/// no code path can leave the terminal in raw/alt-screen mode.
pub struct TerminalGuard<T: TerminalOps> {
    ops: T,
    restored: bool,
}

impl<T: TerminalOps> TerminalGuard<T> {
    /// Enter the UI and return a guard that will restore it.
    pub fn new(mut ops: T) -> io::Result<Self> {
        ops.enter()?;
        Ok(Self {
            ops,
            restored: false,
        })
    }

    /// Restore the terminal now (idempotent); the drop becomes a no-op.
    pub fn restore_now(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        self.ops.restore()
    }
}

impl<T: TerminalOps> Drop for TerminalGuard<T> {
    fn drop(&mut self) {
        let _ = self.restore_now();
    }
}

/// Install a panic hook that restores the terminal BEFORE the previous hook
/// runs, so a panic's backtrace prints to a sane (cooked) terminal instead of
/// the raw alternate screen.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = io::stdout();
        let _ = execute!(out, LeaveAlternateScreen, DisableMouseCapture);
        let _ = disable_raw_mode();
        previous(info);
    }));
}

/// The result of a background fetch, tagged with the generation that issued it
/// so a superseded or cancelled fetch's late result can be ignored.
struct FetchOutcome {
    generation: u64,
    result: LoadResult,
}

/// A fetch's outcome: a parsed page and its title, or an error message. A
/// success also carries the session's authoritative identify answer for the
/// fetched destination (read under the same lock as the fetch), so the loop can
/// refresh the model's display flag after every fresh load.
enum LoadResult {
    Ok {
        doc: MicronDocument,
        title: String,
        dest: [u8; 16],
        identifying: bool,
    },
    Err(String),
    /// A file download completed and was saved to disk.
    Downloaded {
        name: String,
        bytes: usize,
        dir: String,
    },
    /// A file download (or the save to disk) failed.
    DownloadErr(String),
}

/// Start (or restart) a background fetch for `target`. Aborts any in-flight
/// task, bumps the generation, and spawns a task that locks the session, fetches
/// and parses the page, resolves the title, and reports the tagged outcome.
fn spawn_fetch(
    inflight: &mut Option<tokio::task::JoinHandle<()>>,
    generation: &mut u64,
    session: &Arc<Mutex<Session>>,
    tx: &mpsc::UnboundedSender<FetchOutcome>,
    timeout: Duration,
    target: Target,
) {
    if let Some(handle) = inflight.take() {
        handle.abort();
    }
    *generation = generation.wrapping_add(1);
    let generation_now = *generation;
    let session = session.clone();
    let tx = tx.clone();
    let handle = tokio::spawn(async move {
        let mut guard = session.lock().await;
        let result = match browser::fetch_document(&mut guard, &target, timeout).await {
            Ok(doc) => {
                let name = guard.node_name(&target.dest_hash);
                let title = browser::page_title(name.as_deref(), &target.dest_hash, &target.path);
                LoadResult::Ok {
                    doc,
                    title,
                    dest: target.dest_hash,
                    identifying: guard.is_identifying(&target.dest_hash),
                }
            }
            Err(err) => LoadResult::Err(err.to_string()),
        };
        let _ = tx.send(FetchOutcome {
            generation: generation_now,
            result,
        });
    });
    *inflight = Some(handle);
}

/// Start a background file download for `target`, sharing the in-flight slot
/// and generation counter with fetches: starting either supersedes the other.
/// The spawned task fetches the raw bytes over the session, saves them into
/// the download directory (`$XDG_DOWNLOAD_DIR`, else `$HOME/Downloads`,
/// created if missing, existing names de-duplicated), and reports the outcome.
fn spawn_download(
    inflight: &mut Option<tokio::task::JoinHandle<()>>,
    generation: &mut u64,
    session: &Arc<Mutex<Session>>,
    tx: &mpsc::UnboundedSender<FetchOutcome>,
    timeout: Duration,
    target: Target,
) {
    if let Some(handle) = inflight.take() {
        handle.abort();
    }
    *generation = generation.wrapping_add(1);
    let generation_now = *generation;
    let session = session.clone();
    let tx = tx.clone();
    let handle = tokio::spawn(async move {
        let mut guard = session.lock().await;
        let fetched = guard.download_file(&target, timeout).await;
        drop(guard);
        let result = match fetched {
            Ok((bytes, server_name)) => {
                let name = crate::download::choose_name(server_name.as_deref(), &target.path);
                match crate::download::download_dir() {
                    Some(dir) => match crate::download::save_to_dir(&dir, &name, &bytes) {
                        Ok(path) => LoadResult::Downloaded {
                            name: path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or(name),
                            bytes: bytes.len(),
                            dir: dir.to_string_lossy().into_owned(),
                        },
                        Err(err) => {
                            LoadResult::DownloadErr(format!("could not save {name}: {err}"))
                        }
                    },
                    None => LoadResult::DownloadErr(
                        "no download directory (neither XDG_DOWNLOAD_DIR nor HOME set)".to_string(),
                    ),
                }
            }
            Err(err) => LoadResult::DownloadErr(err.to_string()),
        };
        let _ = tx.send(FetchOutcome {
            generation: generation_now,
            result,
        });
    });
    *inflight = Some(handle);
}

/// One inline image's outcome, reported by the image-queue task.
struct ImageOutcome {
    /// The queue generation this belongs to; a stale one is dropped.
    generation: u64,
    /// The link index it belongs to.
    link: usize,
    /// Where it was fetched from, which is the cache key.
    target: Target,
    /// What happened.
    result: ImageResult,
}

/// What the image-queue task has to say about one picture.
enum ImageResult {
    /// Its transfer has started.
    Started,
    /// Its bytes arrived.
    Bytes(Vec<u8>),
    /// Its transfer failed.
    Failed(String),
}

/// One picture the shell is holding: the payload, and the encoded form the
/// terminal will actually be handed.
struct ImageEntry {
    /// The file name, for saving and for the toast.
    name: String,
    /// The payload as fetched, kept so the picture can be re-encoded on a
    /// resize and saved without a second transfer.
    bytes: Vec<u8>,
    /// The encoded picture, absent when the backend is [`Backend::Text`] or
    /// the payload did not decode.
    protocol: Option<SlicedProtocol>,
    /// Where it was written, once the reader has saved it.
    saved: Option<PathBuf>,
}

/// The IO shell's side of inline images: what this terminal can draw, the
/// pictures held for the current page, and the transfer queue.
///
/// Kept out of [`Model`] on purpose. The model is pure, cloneable data that
/// every unit test builds by hand; an encoded Kitty or Sixel payload is
/// neither, and the fetch queue is IO. The model carries only the slot states,
/// which is all the layout and the assertions need.
pub struct ImageStore {
    picker: Option<Picker>,
    backend: Backend,
    entries: HashMap<usize, ImageEntry>,
    task: Option<tokio::task::JoinHandle<()>>,
    generation: u64,
    /// Payloads held across page changes, so a revisit costs no airtime.
    /// Bounded by bytes; see [`ImageCache`].
    cache: ImageCache,
}

impl Default for ImageStore {
    /// A store that draws nothing: no terminal was asked, so there is no
    /// picture to draw. What the unit tests and the print sink use.
    fn default() -> Self {
        ImageStore {
            picker: None,
            backend: Backend::Text,
            entries: HashMap::new(),
            task: None,
            generation: 0,
            cache: ImageCache::default(),
        }
    }
}

impl ImageStore {
    /// Ask the terminal what it can draw, and settle on a backend.
    ///
    /// The query talks to the terminal directly and wants raw mode, so this
    /// runs after the terminal is set up. Any failure — a terminal that
    /// answers nothing, a pipe, a query that times out — falls back to
    /// half-blocks rather than propagating: a browser that refuses to start
    /// because it could not identify the terminal would be absurd.
    pub fn detect(no_color: bool, depth: ColorDepth, cache_bytes: u64) -> Self {
        let picker = match no_color {
            // Nothing will be drawn, so do not write query sequences at all.
            true => None,
            false => Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())),
        };
        let detected = picker
            .as_ref()
            .map(|p| p.protocol_type())
            .unwrap_or(ProtocolType::Halfblocks);
        ImageStore {
            picker,
            backend: crate::image::choose_backend(detected, depth, no_color),
            entries: HashMap::new(),
            task: None,
            generation: 0,
            cache: ImageCache::new(cache_bytes),
        }
    }

    /// Split a page's image requests into the ones the cache can answer at
    /// once and the ones that have to be fetched.
    ///
    /// The cached ones come back as folded-ready events: they are decoded and
    /// encoded here exactly as a fresh payload would be, so a cache hit and a
    /// fetch are indistinguishable from the model's side apart from the
    /// waiting. Note that the ENCODED form is not cached, only the payload —
    /// the cell size an encode targets depends on the terminal width, which
    /// changes between visits.
    fn take_cached(
        &mut self,
        requests: Vec<ImageRequest>,
        model: &Model,
    ) -> (Vec<AppEvent>, Vec<ImageRequest>) {
        let mut events = Vec::new();
        let mut pending = Vec::new();
        for request in requests {
            let Some(bytes) = self.cache.get(&request.target).map(<[u8]>::to_vec) else {
                pending.push(request);
                continue;
            };
            let name = model
                .image_slot(request.link)
                .map(|slot| slot.name.clone())
                .unwrap_or_else(|| "image".to_string());
            events.push(self.accept(request.link, name, bytes, model));
        }
        (events, pending)
    }

    /// The encoded picture for a link, if there is one to draw.
    fn protocol(&self, link: usize) -> Option<&SlicedProtocol> {
        self.entries.get(&link)?.protocol.as_ref()
    }

    /// Drop the current page's pictures and stop its transfer queue. The
    /// cache is deliberately untouched: it exists precisely to outlive the
    /// page that filled it.
    fn reset(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
    }

    /// Take a fetched payload, encode it if this terminal can draw it, and
    /// return the event the model should fold.
    ///
    /// Every failure path ends in [`AppEvent::ImageText`] rather than an error:
    /// the reader still gets a line saying what the file is and can still save
    /// or open it, which is a better answer than a picture-shaped hole.
    fn accept(&mut self, link: usize, name: String, bytes: Vec<u8>, model: &Model) -> AppEvent {
        let probe = crate::image::probe(&bytes);
        let entry = ImageEntry {
            name,
            bytes,
            protocol: None,
            saved: None,
        };
        self.entries.insert(link, entry);

        if self.backend == Backend::Text {
            return AppEvent::ImageText {
                link,
                probe,
                reason: "this terminal shows no images".to_string(),
            };
        }
        let Some(probe) = probe else {
            return AppEvent::ImageText {
                link,
                probe: None,
                reason: "not a format this browser decodes".to_string(),
            };
        };
        match self.encode(link, probe, model) {
            Ok(cells) => AppEvent::ImageReady { link, cells, probe },
            Err(reason) => AppEvent::ImageText {
                link,
                probe: Some(probe),
                reason,
            },
        }
    }

    /// Decode and encode one held payload at the size the current layout
    /// allows, storing the result. Returns the cell size it will occupy.
    fn encode(&mut self, link: usize, probe: Probe, model: &Model) -> Result<(u16, u16), String> {
        let Some(picker) = self.picker.as_ref() else {
            return Err("this terminal shows no images".to_string());
        };
        let Some(entry) = self.entries.get_mut(&link) else {
            return Err("no payload".to_string());
        };
        let decoded = crate::image::decode(&entry.bytes)?;
        // Cell geometry and the row ceiling are both backend-dependent: a
        // graphics protocol paints the cell's full pixel box, half-blocks
        // carry one pixel across and two down whatever the font says.
        let cells = crate::image::fit_cells(
            (probe.width, probe.height),
            self.backend.cell_pixels(picker.font_size()),
            model.width.min(u16::MAX as usize) as u16,
            self.backend.max_rows(model.viewport()),
        );
        let size = Size::new(cells.0, cells.1);
        // `Fit` never enlarges, and it measures "large enough" in font-size
        // pixels. On the half-block path that silently undoes the sizing
        // above: a 300x300 portrait is only 38x19 cells of font box, so the
        // protocol would shrink back to that and paint 38x38 half-block
        // pixels. `Scale` honours the area we asked for, which is where the
        // resolution lives when a cell carries one pixel across and two down.
        let resize = match self.backend {
            Backend::Halfblocks => Resize::Scale(Some(FilterType::Triangle)),
            _ => Resize::Fit(None),
        };
        let sliced = SlicedProtocol::new_with_resize(picker, decoded, size, resize)
            .map_err(|e| format!("cannot draw: {e}"))?;
        entry.protocol = Some(sliced);
        Ok(cells)
    }

    /// Re-encode every held picture for the current layout, after a resize.
    ///
    /// Returns the events the caller should fold, so a picture that no longer
    /// fits (or now fits better) changes the rows it reserves.
    fn reencode_all(&mut self, model: &Model) -> Vec<AppEvent> {
        let links: Vec<usize> = model
            .images
            .iter()
            .filter(|slot| matches!(slot.state, SlotState::Ready { .. }))
            .map(|slot| slot.link)
            .collect();
        let mut events = Vec::new();
        for link in links {
            let Some(probe) = self
                .entries
                .get(&link)
                .and_then(|entry| crate::image::probe(&entry.bytes))
            else {
                continue;
            };
            match self.encode(link, probe, model) {
                Ok(cells) => events.push(AppEvent::ImageReady { link, cells, probe }),
                Err(reason) => events.push(AppEvent::ImageText {
                    link,
                    probe: Some(probe),
                    reason,
                }),
            }
        }
        events
    }

    /// Write a held picture into the download directory, once. A second save
    /// returns the path it already has, so `s` then `o` does not produce two
    /// files.
    fn save(&mut self, link: usize) -> Result<(PathBuf, usize), String> {
        let Some(entry) = self.entries.get_mut(&link) else {
            return Err("that image has not arrived yet".to_string());
        };
        if let Some(path) = &entry.saved {
            return Ok((path.clone(), entry.bytes.len()));
        }
        let dir = crate::download::download_dir()
            .ok_or("no download directory (neither XDG_DOWNLOAD_DIR nor HOME set)")?;
        let path = crate::download::save_to_dir(&dir, &entry.name, &entry.bytes)
            .map_err(|e| format!("could not save {}: {e}", entry.name))?;
        entry.saved = Some(path.clone());
        Ok((path, entry.bytes.len()))
    }
}

/// Fetch a page's inline images one after another, reporting each outcome.
///
/// Sequential on purpose: a link carries one transfer at a time, so issuing
/// them in parallel would only queue them inside the stack while holding the
/// session lock longer. Between images the lock is released, so a navigation
/// the reader starts is served promptly — and it aborts this task anyway.
fn spawn_image_queue(
    store: &mut ImageStore,
    session: &Arc<Mutex<Session>>,
    tx: &mpsc::UnboundedSender<ImageOutcome>,
    timeout: Duration,
    requests: Vec<ImageRequest>,
) {
    let generation = store.generation;
    let session = session.clone();
    let tx = tx.clone();
    store.task = Some(tokio::spawn(async move {
        for request in requests {
            let _ = tx.send(ImageOutcome {
                generation,
                link: request.link,
                target: request.target.clone(),
                result: ImageResult::Started,
            });
            let fetched = {
                let mut guard = session.lock().await;
                guard.download_file(&request.target, timeout).await
            };
            let result = match fetched {
                Ok((bytes, _)) => ImageResult::Bytes(bytes),
                Err(err) => ImageResult::Failed(err.to_string()),
            };
            if tx
                .send(ImageOutcome {
                    generation,
                    link: request.link,
                    target: request.target,
                    result,
                })
                .is_err()
            {
                return;
            }
        }
    }));
}

/// How long the background discovery drain parks on the shared session before
/// yielding the lock. A fetch that wants the session waits at most this slice,
/// while an idle mesh still has its announces drained continuously. Kept small
/// relative to a page fetch so the hand-off is imperceptible.
const DISCOVERY_LOCK_SLICE: Duration = Duration::from_millis(250);

/// Spawn the continuous NomadNet node-discovery drain.
///
/// The single shared-instance event stream carries announces alongside fetch
/// responses and has one consumer (whoever holds the session), so discovery
/// cannot own a second stream. Instead this task drives the same session in
/// short slices: it locks the session, parks for the next node announce, then
/// releases so a queued navigation runs next (the session mutex is fair). Any
/// node announce it drains — or that a concurrent fetch drains — is forwarded to
/// the announce sink wired to the loop's `node_rx`, surfacing as an
/// [`AppEvent::NodeDiscovered`]. So the registry keeps filling whether or not a
/// fetch is in flight, and there is never a second reader racing the stream.
///
/// The returned handle is aborted at teardown (mirroring the in-flight fetch),
/// dropping this task's session clone so the session can be unwrapped and closed.
fn spawn_discovery(session: &Arc<Mutex<Session>>) -> tokio::task::JoinHandle<()> {
    let session = session.clone();
    tokio::spawn(async move {
        loop {
            let mut guard = session.lock().await;
            // `recv_node_announce` forwards each node announce to the sink itself;
            // it is cancel-safe, so capping the park at one slice drops the wait
            // without losing a buffered event. `Ok(None)` means the stream closed
            // (the session is stopping) — stop draining. Otherwise (an announce
            // arrived, or the slice elapsed) release the lock and re-park.
            let closed = matches!(
                tokio::time::timeout(DISCOVERY_LOCK_SLICE, guard.recv_node_announce()).await,
                Ok(None)
            );
            drop(guard);
            if closed {
                break;
            }
        }
    })
}

/// The IO handles [`run_effects`] works with, bundled.
///
/// A struct rather than eight positional arguments: the list grew past the
/// point where a caller could get the order right by reading it, and every
/// member is borrowed from the same run loop anyway.
struct Shell<'a> {
    /// The in-flight page fetch or download.
    inflight: &'a mut Option<tokio::task::JoinHandle<()>>,
    /// The fetch generation, bumped to invalidate a superseded result.
    generation: &'a mut u64,
    /// The shared session every transfer runs over.
    session: &'a Arc<Mutex<Session>>,
    /// Where a fetch reports back.
    tx: &'a mpsc::UnboundedSender<FetchOutcome>,
    /// The inline-image store and its queue.
    images: &'a mut ImageStore,
    /// Where the image queue reports back.
    image_tx: &'a mpsc::UnboundedSender<ImageOutcome>,
    /// Per-request timeout.
    timeout: Duration,
}

/// Run the effects [`update`] returned: start navigations, cancel the in-flight
/// fetch, persist an identify decision, or record a quit.
async fn run_effects(effects: Vec<Effect>, model: &mut Model, shell: Shell<'_>) {
    let Shell {
        inflight,
        generation,
        session,
        tx,
        images,
        image_tx,
        timeout,
    } = shell;
    for effect in effects {
        match effect {
            Effect::Navigate(target) => {
                // The pictures of the page being left are of no use to anyone.
                images.reset();
                spawn_fetch(inflight, generation, session, tx, timeout, target);
            }
            Effect::FetchImages(requests) => {
                // Whatever the queue was working on belongs to the page just
                // left. Reset before serving, so a cache hit is not wiped by
                // its own reset.
                images.reset();
                let (cached, pending) = images.take_cached(requests, model);
                for event in cached {
                    update(model, event);
                }
                if !pending.is_empty() {
                    spawn_image_queue(images, session, image_tx, timeout, pending);
                }
            }
            Effect::SaveImage { link } => match images.save(link) {
                Ok((path, bytes)) => model.set_toast(
                    ToastKind::Info,
                    format!("saved {} ({bytes} bytes)", path.display()),
                ),
                Err(err) => model.set_toast(ToastKind::Error, err),
            },
            Effect::OpenImage { link } => match images.save(link) {
                // Saved first, because the platform handler needs a file: the
                // picture only ever existed in this process's memory.
                Ok((path, _)) => match open::that(&path) {
                    Ok(_) => model.set_toast(
                        ToastKind::Info,
                        format!("opened {} externally", path.display()),
                    ),
                    Err(_) => model.set_toast(
                        ToastKind::Error,
                        format!("failed to open {}", path.display()),
                    ),
                },
                Err(err) => model.set_toast(ToastKind::Error, err),
            },
            Effect::Download(target) => {
                spawn_download(inflight, generation, session, tx, timeout, target);
            }
            Effect::SetIdentify { dest, on } => {
                // The decision must be stored (and the reused link dropped)
                // BEFORE the accompanying reload's fetch builds its fresh link.
                // An in-flight fetch may hold the session lock for seconds, but
                // it is superseded by that reload anyway — abort it now so the
                // lock frees within one discovery slice.
                if let Some(handle) = inflight.take() {
                    handle.abort();
                }
                *generation = generation.wrapping_add(1);
                if let Err(err) = session.lock().await.set_identify(&dest, on) {
                    model.set_toast(
                        ToastKind::Error,
                        format!("could not save identify decision: {err}"),
                    );
                }
            }
            Effect::OpenExternal(url) => {
                // Hand the URL to the platform default handler (`xdg-open` on
                // Linux). `open::that` launches the handler and returns; a
                // failure only raises a toast, never takes the UI down.
                match open::that(&url) {
                    Ok(_) => model.set_toast(ToastKind::Info, format!("opened externally: {url}")),
                    Err(_) => model.set_toast(ToastKind::Error, format!("failed to open: {url}")),
                }
            }
            Effect::Cancel => {
                if let Some(handle) = inflight.take() {
                    handle.abort();
                }
                // Cancelling means cancelling: the queued pictures stop too.
                images.reset();
                // Invalidate any late result from the cancelled task.
                *generation = generation.wrapping_add(1);
            }
            Effect::Copy(text) => {
                // Write the OSC 52 clipboard-set sequence straight to the
                // terminal; the update side has already raised the "copied" toast.
                let _ =
                    crossterm::execute!(std::io::stdout(), crossterm::style::Print(osc52(&text)));
            }
            Effect::SaveBookmarks => {
                // Persist the bookmarks, ignoring IO errors: a failed write must
                // not take down the browser (no config dir → nothing to do).
                if let Some(path) = &model.bookmarks_path {
                    let _ = model.bookmarks.save(path);
                }
            }
            Effect::Quit => model.quit = true,
        }
    }
}

/// How long to wait for the terminal's OSC 11 background reply before giving up
/// and falling back to the dark theme. Kept short so a terminal that never
/// answers does not stall startup.
const BG_QUERY_TIMEOUT: Duration = Duration::from_millis(100);

/// Query the terminal background for `--theme auto`, returning `None` (→ dark)
/// when the flag is explicit, the query fails, or the terminal does not answer.
///
/// The `termbg` crate sends the OSC 11 query (falling back to `COLORFGBG`),
/// parses the reply, and handles the timeout, tmux/screen wrapping and raw-mode
/// save/restore itself. Its 16-bit-per-channel result is reduced to 8-bit; the
/// foreground, when `COLORFGBG` provides one, is a luminance tiebreaker only.
fn detect_background(flag: ThemeFlag) -> Option<Bg> {
    if !matches!(flag, ThemeFlag::Auto) {
        return None;
    }
    let bg = termbg::rgb(BG_QUERY_TIMEOUT).ok()?;
    Some(Bg {
        bg: rgb16_to_rgb8(bg),
        fg: colorfgbg_foreground(),
    })
}

/// Reduce a `termbg` 16-bit-per-channel colour to 8-bit by taking the high byte.
fn rgb16_to_rgb8(c: termbg::Rgb) -> (u8, u8, u8) {
    ((c.r >> 8) as u8, (c.g >> 8) as u8, (c.b >> 8) as u8)
}

/// The foreground colour implied by `COLORFGBG` (`"<fg>;<bg>"` of palette
/// indices), as a coarse grey used only as a mid-tone tiebreaker. `None` when
/// the variable is absent or unparsable.
fn colorfgbg_foreground() -> Option<(u8, u8, u8)> {
    let raw = std::env::var("COLORFGBG").ok()?;
    let fg_index: u8 = raw.split(';').next()?.trim().parse().ok()?;
    Some(ansi_index_grey(fg_index))
}

/// A grey whose luminance approximates a 16-colour palette index: the normal
/// set (0..8) reads dark, the bright set (8..16) light. Only the relative
/// luminance matters, so an exact palette is unnecessary.
fn ansi_index_grey(index: u8) -> (u8, u8, u8) {
    let v = match index {
        0 => 0,
        7 => 192,
        8 => 96,
        15 => 255,
        i if i < 8 => 128,
        _ => 224,
    };
    (v, v, v)
}

/// Run the interactive TUI, owning the [`Session`] and driving navigation.
///
/// Does the initial fetch of `initial` and every subsequent navigation as a
/// background task; the task's result rejoins the `tokio::select!` loop as a
/// [`AppEvent::PageLoaded`] / [`AppEvent::LoadFailed`], so the UI stays live
/// (spinner animates, keys and the address editor work) while a page loads. A
/// cancel drops the in-flight task; a slow or failed fetch never blocks the UI.
pub async fn run_tui(
    session: Session,
    initial: Option<Target>,
    opts: BrowserOptions,
    theme_flag: ThemeFlag,
) -> io::Result<()> {
    install_panic_hook();

    // Resolve the theme BEFORE entering the alt-screen/raw event loop: the OSC 11
    // background query (via `termbg`) briefly toggles raw mode and reads a reply
    // on stdin, and doing it now keeps that reply from leaking into key input.
    // `termbg` restores the terminal state and drains stdin itself.
    let theme = resolve_theme(detect_background(theme_flag), theme_flag);

    let mut guard = TerminalGuard::new(CrosstermTerminal::new())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut events = EventStream::new();

    // Load persisted bookmarks (best-effort): resolve the store path and read it,
    // tolerating a missing or corrupt file. `None` when no config dir is
    // resolvable, in which case bookmarks stay in-memory only and
    // `Effect::SaveBookmarks` becomes a no-op.
    let bookmarks_path = crate::bookmarks::default_path();
    let bookmarks = bookmarks_path
        .as_deref()
        .map(Bookmarks::load)
        .unwrap_or_default();

    let mut model = Model {
        no_color: opts.no_color,
        no_images: opts.no_images,
        depth: opts.depth,
        theme,
        bookmarks,
        bookmarks_path,
        // The session's own fingerprint is stable for its whole life: set once.
        fingerprint: session.fingerprint_hex(),
        ..Model::default()
    };
    let size = terminal.size()?;
    model.size = (size.width, size.height);
    model.relayout(content_width(size.width));

    // Wire continuous node discovery: the session forwards every node announce it
    // sees (idle-drained by the background task below, or drained by a fetch) to
    // this channel, which the select loop folds into the model registry. Set the
    // sink before sharing the session so no announce is missed at startup.
    let (node_tx, mut node_rx) = mpsc::unbounded_channel::<DiscoveredNode>();
    let mut session = session;
    session.set_announce_sink(node_tx);

    let session = Arc::new(Mutex::new(session));
    let (tx, mut rx) = mpsc::unbounded_channel::<FetchOutcome>();
    let (image_tx, mut image_rx) = mpsc::unbounded_channel::<ImageOutcome>();
    let mut inflight: Option<tokio::task::JoinHandle<()>> = None;
    let mut generation: u64 = 0;
    // Ask the terminal what it can draw once, now that raw mode is on. Every
    // failure path inside settles on half-blocks, so this cannot fail.
    let mut images = match model.no_images {
        true => ImageStore::default(),
        false => ImageStore::detect(model.no_color, model.depth, opts.image_cache_bytes),
    };

    // Continuous background discovery: lives for the whole session, independent
    // of fetches, feeding `node_rx` (see `spawn_discovery`).
    let discovery = spawn_discovery(&session);

    match initial {
        // Kick off the initial navigation, honouring a `#anchor` on the initial
        // URL (the parser folds it into the path; split it back off so the
        // fetched path is clean and the load handler can scroll to it).
        Some(target) => {
            let (target, anchor) = split_path_anchor(target);
            model.pending = Some(Pending {
                target: target.clone(),
                action: HistoryAction::Push,
            });
            model.pending_anchor = anchor;
            spawn_fetch(
                &mut inflight,
                &mut generation,
                &session,
                &tx,
                opts.timeout,
                target,
            );
        }
        // No URL to open: show the start screen and put the places panel in
        // front of it, since that is where a reader without an address finds
        // one — their bookmarks, and the nodes background discovery turns up.
        None => {
            show_start_screen(&mut model);
        }
    }

    let mut ticker = tokio::time::interval(Duration::from_millis(SPINNER_TICK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = loop {
        if let Err(err) = terminal.draw(|frame| view(&model, &images, frame)) {
            break Err(err);
        }
        if model.quit {
            break Ok(());
        }
        // Tick while a fetch spinner animates or a toast is aging towards its
        // auto-dismiss, so an idle toast still expires.
        let animate = model.needs_tick();
        tokio::select! {
            maybe_event = events.next() => match step_event(maybe_event) {
                EventStep::Apply(app) => {
                    // A resize changes how many cells a picture may occupy, so
                    // every held picture is re-encoded for the new layout.
                    let resized = matches!(app, AppEvent::Resize(_, _));
                    let mut effects = update(&mut model, app);
                    if resized {
                        for event in images.reencode_all(&model) {
                            effects.extend(update(&mut model, event));
                        }
                    }
                    run_effects(
                        effects,
                        &mut model,
                        Shell {
                            inflight: &mut inflight,
                            generation: &mut generation,
                            session: &session,
                            tx: &tx,
                            images: &mut images,
                            image_tx: &image_tx,
                            timeout: opts.timeout,
                        },
                    )
                    .await;
                }
                // A recognised-but-inert event (focus/paste, which map to
                // nothing): keep looping.
                EventStep::Ignore => {}
                // A closed stream (Ok) or a genuine IO error (Err) both leave
                // the loop; the error propagates so a dead terminal cannot
                // busy-loop.
                EventStep::End(result) => break result,
            },
            Some(outcome) = rx.recv() => {
                if outcome.generation == generation {
                    inflight = None;
                    match outcome.result {
                        LoadResult::Ok { doc, title, dest, identifying } => {
                            let effects =
                                update(&mut model, AppEvent::PageLoaded { doc, title });
                            run_effects(
                                effects,
                                &mut model,
                                Shell {
                                    inflight: &mut inflight,
                                    generation: &mut generation,
                                    session: &session,
                                    tx: &tx,
                                    images: &mut images,
                                    image_tx: &image_tx,
                                    timeout: opts.timeout,
                                },
                            )
                            .await;
                            // The session's answer for this load is
                            // authoritative: it corrects the optimistic toggle
                            // and covers a first visit to a node persisted in
                            // identify.toml by an earlier run.
                            model.note_identify(dest, identifying);
                        }
                        LoadResult::Err(msg) => {
                            update(&mut model, AppEvent::LoadFailed(msg));
                        }
                        LoadResult::Downloaded { name, bytes, dir } => {
                            update(&mut model, AppEvent::DownloadDone { name, bytes, dir });
                        }
                        LoadResult::DownloadErr(msg) => {
                            update(&mut model, AppEvent::DownloadFailed(msg));
                        }
                    }
                }
            },
            Some(outcome) = image_rx.recv() => {
                // A stale generation belongs to a page the reader has left.
                if outcome.generation == images.generation {
                    let event = match outcome.result {
                        ImageResult::Started => AppEvent::ImageLoading { link: outcome.link },
                        ImageResult::Failed(reason) => {
                            AppEvent::ImageFailed { link: outcome.link, reason }
                        }
                        ImageResult::Bytes(bytes) => {
                            let name = model
                                .image_slot(outcome.link)
                                .map(|slot| slot.name.clone())
                                .unwrap_or_else(|| "image".to_string());
                            // Cached before it is drawn, so it survives the
                            // page: the airtime is already spent either way.
                            images.cache.insert(outcome.target, bytes.clone());
                            images.accept(outcome.link, name, bytes, &model)
                        }
                    };
                    update(&mut model, event);
                }
            }
            Some(node) = node_rx.recv() => {
                update(&mut model, AppEvent::NodeDiscovered(node));
            }
            _ = ticker.tick(), if animate => {
                update(&mut model, AppEvent::Tick);
            }
        }
    };

    // Tear down: abort and drain the in-flight task and the background discovery
    // task (releasing their session clones), then best-effort close the session
    // once no task clone of it survives.
    if let Some(handle) = inflight.take() {
        handle.abort();
        let _ = handle.await;
    }
    images.reset();
    discovery.abort();
    let _ = discovery.await;
    drop(tx);
    if let Ok(mutex) = Arc::try_unwrap(session) {
        let _ = mutex.into_inner().close().await;
    }

    guard.restore_now()?;
    result
}

/// Translate a crossterm event into an [`AppEvent`], dropping the ones the UI
/// does not act on (focus, paste).
fn map_event(event: Event) -> Option<AppEvent> {
    match event {
        Event::Key(key) => Some(AppEvent::Key(key)),
        Event::Mouse(mouse) => Some(AppEvent::Mouse(mouse)),
        Event::Resize(cols, rows) => Some(AppEvent::Resize(cols, rows)),
        _ => None,
    }
}

/// What the event loop should do with one item pulled from the terminal
/// [`EventStream`].
enum EventStep {
    /// Apply this app-event to the model (a mapped, actionable event).
    Apply(AppEvent),
    /// A recognised-but-inert event that maps to nothing: do nothing and keep
    /// looping. Crossterm discards unparseable byte sequences internally (its
    /// `parse_event` contract clears the buffer on error), so those never reach
    /// us as a `Some(Err)` — the only event we ignore is a mapped-to-`None` one
    /// (focus gained/lost, paste).
    Ignore,
    /// Leave the loop with this result: `Ok(())` on a clean close (the stream
    /// ended), `Err` on a genuine IO error from the event stream (a stdin/poll
    /// failure). Ignoring the latter would busy-loop on a dead terminal.
    End(io::Result<()>),
}

/// Classify one `EventStream` item without touching the model, so the loop's
/// exit behaviour is unit-testable. A `Some(Err)` is a real IO error (stdin or
/// poll failure) — crossterm never surfaces unparseable sequences as errors, it
/// discards them internally — so it is an exit, NOT ignore-and-continue:
/// ignoring it risks a busy-loop on a dead terminal. `None` (stream closed) is a
/// clean exit.
fn step_event(item: Option<Result<Event, io::Error>>) -> EventStep {
    match item {
        Some(Ok(event)) => match map_event(event) {
            Some(app) => EventStep::Apply(app),
            None => EventStep::Ignore,
        },
        Some(Err(err)) => EventStep::End(Err(err)),
        None => EventStep::End(Ok(())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use leviculum_micron::parse;
    use ratatui::backend::TestBackend;
    use std::cell::Cell;
    use std::rc::Rc;

    const SAMPLE: &str = include_str!("../tests/fixtures/sample.mu");

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn press(model: &mut Model, code: KeyCode, mods: KeyModifiers) -> Vec<Effect> {
        update(model, AppEvent::Key(key(code, mods)))
    }

    fn type_str(model: &mut Model, s: &str) {
        for ch in s.chars() {
            press(model, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    fn tgt(n: u8) -> Target {
        Target {
            dest_hash: [n; 16],
            path: format!("/page/{n}.mu"),
            fields: Vec::new(),
            is_file: false,
        }
    }

    fn model_from_sample(width: usize, size: (u16, u16)) -> Model {
        Model::from_document(&parse(SAMPLE), width, "Sample", size)
    }

    /// The full buffer flattened to a newline-joined string.
    fn flat(buffer: &ratatui::buffer::Buffer) -> String {
        let mut s = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                s.push_str(buffer[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    /// The text of buffer row `y` across columns `0..width`.
    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        let mut s = String::new();
        for x in 0..width {
            s.push_str(buffer[(x, y)].symbol());
        }
        s
    }

    /// The plain text of a laid-out line (its cells' characters).
    fn line_text(line: &RLine) -> String {
        line.cells.iter().map(|c| c.ch).collect()
    }

    #[test]
    fn quit_key_sets_quit_flag() {
        let mut model = model_from_sample(80, (80, 24));
        assert!(!model.quit);
        let effects = press(&mut model, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(model.quit);
        assert_eq!(effects, vec![Effect::Quit]);
    }

    #[test]
    fn ctrl_c_sets_quit_flag() {
        let mut model = model_from_sample(80, (80, 24));
        let effects = press(&mut model, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(model.quit);
        assert_eq!(effects, vec![Effect::Quit]);
    }

    #[test]
    fn plain_c_does_not_quit() {
        let mut model = model_from_sample(80, (80, 24));
        press(&mut model, KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!model.quit);
    }

    #[test]
    fn resize_updates_size() {
        let mut model = model_from_sample(80, (80, 24));
        update(&mut model, AppEvent::Resize(40, 10));
        assert_eq!(model.size, (40, 10));
    }

    #[test]
    fn content_region_is_total_minus_chrome() {
        assert_eq!(content_height(24), 22);
        let [topbar, content, status] = regions(Rect::new(0, 0, 80, 24));
        assert_eq!(topbar.height, TOPBAR_ROWS);
        assert_eq!(
            topbar.y, 0,
            "content begins on the row after the one-row top bar"
        );
        assert_eq!(content.y, TOPBAR_ROWS);
        assert_eq!(status.height, STATUS_ROWS);
        assert_eq!(content.height, 24 - CHROME_ROWS);
        assert_eq!(model_from_sample(80, (80, 24)).viewport(), 22);
    }

    // --- History ---------------------------------------------------------

    #[test]
    fn history_visit_back_forward_and_truncation() {
        let mut h = History::default();
        assert!(!h.can_back());
        assert!(!h.can_forward());
        assert_eq!(h.current(), None);

        let (a, b, c) = (tgt(1), tgt(2), tgt(3));
        h.visit(a.clone());
        assert_eq!(h.current(), Some(&a));
        assert!(!h.can_back());

        h.visit(b.clone());
        assert!(h.can_back());
        assert!(!h.can_forward());
        assert_eq!(h.current(), Some(&b));

        assert_eq!(h.back(), Some(&a));
        assert_eq!(h.current(), Some(&a));
        assert!(h.can_forward());
        assert_eq!(h.forward(), Some(&b));

        // From the middle, a fresh visit truncates the forward branch.
        assert_eq!(h.back(), Some(&a));
        h.visit(c.clone());
        assert_eq!(h.current(), Some(&c));
        assert!(!h.can_forward());
        assert_eq!(h.stack.len(), 2);

        // back then forward lands on c, not the discarded b.
        assert_eq!(h.back(), Some(&a));
        assert_eq!(h.forward(), Some(&c));
    }

    #[test]
    fn history_edges_return_none() {
        let mut h = History::default();
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), None);
        h.visit(tgt(1));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), None);
        assert_eq!(h.idx, 0);
    }

    // --- update / Effect -------------------------------------------------

    #[test]
    fn colon_enters_address_mode() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        let effects = press(&mut m, KeyCode::Char(':'), KeyModifiers::NONE);
        assert!(effects.is_empty());
        assert_eq!(m.mode, Mode::Address);
    }

    #[test]
    fn typing_then_enter_yields_navigate() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        press(&mut m, KeyCode::Char(':'), KeyModifiers::NONE);
        let hash = "0123456789abcdef0123456789abcdef";
        type_str(&mut m, &format!("{hash}:/page/about.mu"));
        let effects = press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        let expected = parse_url(&format!("{hash}:/page/about.mu"), None).unwrap();
        assert_eq!(effects, vec![Effect::Navigate(expected.clone())]);
        assert_eq!(m.mode, Mode::Browse);
        assert_eq!(m.pending.as_ref().map(|p| &p.target), Some(&expected));
        assert!(m.input.value().is_empty());
    }

    #[test]
    fn bad_url_sets_error_and_no_navigate() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        press(&mut m, KeyCode::Char(':'), KeyModifiers::NONE);
        type_str(&mut m, "not-a-hash");
        let effects = press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        assert!(effects.is_empty());
        let toast = m.toast.as_ref().expect("an error toast is raised");
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(toast.text.contains("not-a-hash"), "toast: {}", toast.text);
        // Still in address mode so the user can fix the input.
        assert_eq!(m.mode, Mode::Address);
    }

    #[test]
    fn alt_left_on_history_yields_navigate_prev() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        let (a, b) = (tgt(1), tgt(2));
        m.history.visit(a.clone());
        m.history.visit(b.clone());
        m.current_dest = Some(b.dest_hash);

        let effects = press(&mut m, KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(effects, vec![Effect::Navigate(a.clone())]);
        // The cursor only moves once the page loads.
        assert_eq!(m.history.idx, 1);
        assert_eq!(
            m.pending.as_ref().map(|p| p.action.clone()),
            Some(HistoryAction::Goto(0))
        );
    }

    #[test]
    fn mouse_back_button_on_history_yields_navigate_prev() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        let (a, b) = (tgt(1), tgt(2));
        m.history.visit(a.clone());
        m.history.visit(b.clone());
        m.current_dest = Some(b.dest_hash);

        let effects = update(
            &mut m,
            AppEvent::Mouse(mouse(MouseEventKind::Down(MouseButton::Back))),
        );
        assert_eq!(effects, vec![Effect::Navigate(a.clone())]);
        // The cursor only moves once the page loads (same path as Alt-Left).
        assert_eq!(m.history.idx, 1);
        assert_eq!(
            m.pending.as_ref().map(|p| p.action.clone()),
            Some(HistoryAction::Goto(0))
        );
    }

    #[test]
    fn mouse_forward_button_on_history_yields_navigate_next() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        let (a, b) = (tgt(1), tgt(2));
        m.history.visit(a.clone());
        m.history.visit(b);
        // Step back so a forward step exists.
        m.history.goto(0);
        m.current_dest = Some(a.dest_hash);

        let effects = update(
            &mut m,
            AppEvent::Mouse(mouse(MouseEventKind::Down(MouseButton::Forward))),
        );
        assert_eq!(effects, vec![Effect::Navigate(tgt(2))]);
        assert_eq!(m.history.idx, 0);
        assert_eq!(
            m.pending.as_ref().map(|p| p.action.clone()),
            Some(HistoryAction::Goto(1))
        );
    }

    #[test]
    fn mouse_side_buttons_are_noop_without_history() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        // No prior navigation: nothing behind or ahead.
        let back = update(
            &mut m,
            AppEvent::Mouse(mouse(MouseEventKind::Down(MouseButton::Back))),
        );
        let forward = update(
            &mut m,
            AppEvent::Mouse(mouse(MouseEventKind::Down(MouseButton::Forward))),
        );
        assert!(back.is_empty());
        assert!(forward.is_empty());
        assert!(m.pending.is_none());
    }

    #[test]
    fn esc_during_loading_yields_cancel() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        m.pending = Some(Pending {
            target: tgt(1),
            action: HistoryAction::Push,
        });
        let effects = press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(effects, vec![Effect::Cancel]);
        assert!(m.pending.is_none());
        let toast = m.toast.as_ref().expect("a cancel toast is raised");
        assert_eq!(toast.kind, ToastKind::Info);
        assert_eq!(toast.text, "cancelled");
    }

    #[test]
    fn ctrl_g_during_loading_yields_cancel() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        m.pending = Some(Pending {
            target: tgt(1),
            action: HistoryAction::Push,
        });
        let effects = press(&mut m, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(effects, vec![Effect::Cancel]);
        assert!(m.pending.is_none());
    }

    #[test]
    fn page_loaded_updates_doc_pushes_history_resets_scroll() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        m.scroll = 5;
        m.pending = Some(Pending {
            target: tgt(7),
            action: HistoryAction::Push,
        });
        let effects = update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse("Fresh page body"),
                title: "Fresh".to_string(),
            },
        );
        assert!(effects.is_empty());
        assert_eq!(m.scroll, 0);
        assert_eq!(m.title, "Fresh");
        assert_eq!(m.history.stack.len(), 1);
        assert_eq!(m.history.stack[0], tgt(7));
        assert!(m.pending.is_none());
        assert_eq!(m.current_dest, Some([7; 16]));
        assert!(flat_page(&m).contains("Fresh"), "page: {:?}", flat_page(&m));
    }

    /// The current page's flattened text.
    fn flat_page(m: &Model) -> String {
        m.page.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn load_failed_keeps_page_and_sets_status() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        m.pending = Some(Pending {
            target: tgt(1),
            action: HistoryAction::Push,
        });
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse("Kept page body"),
                title: "Kept".to_string(),
            },
        );
        let lines_before = m.page.len();

        m.pending = Some(Pending {
            target: tgt(2),
            action: HistoryAction::Push,
        });
        let effects = update(&mut m, AppEvent::LoadFailed("no path".to_string()));
        assert!(effects.is_empty());
        let toast = m.toast.as_ref().expect("a failure toast is raised");
        assert_eq!(toast.kind, ToastKind::Error);
        assert_eq!(toast.text, "no path");
        assert!(m.pending.is_none());
        // The page is unchanged: still the previously loaded document.
        assert_eq!(m.title, "Kept");
        assert_eq!(m.page.len(), lines_before);
        assert_eq!(m.history.stack.len(), 1);
    }

    #[test]
    fn reload_navigates_current_without_history_change() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        m.history.visit(tgt(3));
        let effects = press(&mut m, KeyCode::Char('R'), KeyModifiers::NONE);
        assert_eq!(effects, vec![Effect::Navigate(tgt(3))]);
        assert_eq!(
            m.pending.as_ref().map(|p| p.action.clone()),
            Some(HistoryAction::Goto(0))
        );
    }

    #[test]
    fn reload_bound_to_r_ctrl_r_and_f5_but_not_bare_r() {
        // R, Ctrl-R and F5 each reload the current page (browser muscle memory);
        // bare lowercase r is deliberately left free, as a mesh reload is
        // expensive and single letters are reserved for cheap local actions.
        for (code, mods) in [
            (KeyCode::Char('R'), KeyModifiers::NONE),
            (KeyCode::Char('r'), KeyModifiers::CONTROL),
            (KeyCode::F(5), KeyModifiers::NONE),
        ] {
            let mut m = model_from_sample(content_width(80), (80, 24));
            m.history.visit(tgt(3));
            let effects = press(&mut m, code, mods);
            assert_eq!(
                effects,
                vec![Effect::Navigate(tgt(3))],
                "{code:?} + {mods:?} should reload"
            );
            assert_eq!(
                m.pending.as_ref().map(|p| p.action.clone()),
                Some(HistoryAction::Goto(0)),
                "{code:?} + {mods:?} reload must not change history"
            );
        }
        // Bare lowercase r does nothing: no navigation, no pending load.
        let mut m = model_from_sample(content_width(80), (80, 24));
        m.history.visit(tgt(3));
        let effects = press(&mut m, KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(effects.is_empty(), "bare r must be unbound: {effects:?}");
        assert!(m.pending.is_none(), "bare r must not start a load");
    }

    // --- page cache ------------------------------------------------------

    #[test]
    fn is_form_submit_flags_field_targets_only() {
        let submit = Target {
            dest_hash: [3; 16],
            path: "/page/s.mu".to_string(),
            fields: vec![("field_name".to_string(), "value".to_string())],
            is_file: false,
        };
        assert!(is_form_submit(&submit));
        // A `var_` preset is idempotent and cacheable.
        let preset = Target {
            dest_hash: [3; 16],
            path: "/page/s.mu".to_string(),
            fields: vec![("var_a".to_string(), "1".to_string())],
            is_file: false,
        };
        assert!(!is_form_submit(&preset));
        assert!(!is_form_submit(&tgt(1)));
    }

    #[test]
    fn page_loaded_inserts_into_the_cache() {
        let mut m = tall_model((80, 24));
        m.pending = Some(Pending {
            target: tgt(1),
            action: HistoryAction::Push,
        });
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: long_doc(),
                title: "A".to_string(),
            },
        );
        assert!(m.page_cache.contains(&tgt(1)), "fresh load is cached");
        assert!(!m.cached_view, "a fresh load is not a cached view");
    }

    #[test]
    fn fresh_navigation_to_cached_target_loads_from_cache_without_fetch() {
        let mut m = tall_model((80, 24));
        m.history.visit(tgt(1));
        m.current_dest = Some([1; 16]);
        m.page_cache.insert(
            tgt(2),
            CacheEntry {
                doc: long_doc(),
                title: "Two".to_string(),
                scroll: 7,
            },
        );
        let fx = begin_navigation(&mut m, tgt(2), HistoryAction::Push, None);
        assert!(fx.is_empty(), "a cache hit emits no fetch: {fx:?}");
        assert!(m.pending.is_none(), "no fetch is pending");
        assert_eq!(m.title, "Two");
        assert_eq!(m.scroll, 7, "the stored scroll is restored");
        assert!(m.cached_view, "the cached marker is set");
        // History is folded immediately (no async round-trip).
        assert_eq!(m.history.stack, vec![tgt(1), tgt(2)]);
        assert_eq!(m.history.idx, 1);
    }

    #[test]
    fn back_and_forward_hit_the_cache_and_restore_scroll() {
        // A tall, narrow viewport makes the sample document long enough that a
        // scroll offset of 12 is well within range (not clamped on restore).
        let mut m = tall_model((40, 13));
        // Load A through the fetch path so it caches.
        m.pending = Some(Pending {
            target: tgt(1),
            action: HistoryAction::Push,
        });
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: long_doc(),
                title: "A".to_string(),
            },
        );
        m.scroll = 12;

        // Navigate to B (a fresh fetch): leaving A stashes its scroll.
        let fx = begin_navigation(&mut m, tgt(2), HistoryAction::Push, None);
        assert_eq!(fx, vec![Effect::Navigate(tgt(2))]);
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: long_doc(),
                title: "B".to_string(),
            },
        );
        m.scroll = 3;

        // Back to A: instant from cache, scroll 12 restored, no fetch.
        let fx = press(&mut m, KeyCode::Left, KeyModifiers::ALT);
        assert!(fx.is_empty(), "back hit the cache: {fx:?}");
        assert!(m.pending.is_none());
        assert_eq!(m.history.idx, 0, "history moved immediately");
        assert_eq!(m.title, "A");
        assert_eq!(m.scroll, 12, "A's stashed scroll restored");
        assert!(m.cached_view);

        // Forward to B: instant, scroll 3 restored (stashed on leaving B).
        let fx = press(&mut m, KeyCode::Right, KeyModifiers::ALT);
        assert!(fx.is_empty(), "forward hit the cache: {fx:?}");
        assert_eq!(m.history.idx, 1);
        assert_eq!(m.title, "B");
        assert_eq!(m.scroll, 3);
        assert!(m.cached_view);
    }

    #[test]
    fn reload_bypasses_cache_and_overwrites_the_entry() {
        let mut m = tall_model((80, 24));
        m.history.visit(tgt(1));
        m.current_dest = Some([1; 16]);
        m.page_cache.insert(
            tgt(1),
            CacheEntry {
                doc: parse("old body"),
                title: "Old".to_string(),
                scroll: 3,
            },
        );
        // R refetches even though the page is cached.
        let fx = press(&mut m, KeyCode::Char('R'), KeyModifiers::NONE);
        assert_eq!(fx, vec![Effect::Navigate(tgt(1))]);
        assert_eq!(
            m.pending.as_ref().map(|p| p.action.clone()),
            Some(HistoryAction::Goto(0))
        );
        // On load the entry is overwritten with the fresh page.
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse("new body"),
                title: "New".to_string(),
            },
        );
        assert!(!m.cached_view, "a fresh load clears the cached marker");
        let hit = m.page_cache.get(&tgt(1)).expect("still cached");
        assert_eq!(hit.title, "New", "the entry was overwritten");
    }

    #[test]
    fn form_submit_is_never_cached_and_always_fetches() {
        let mut m = tall_model((80, 24));
        let submit = Target {
            dest_hash: [3; 16],
            path: "/page/s.mu".to_string(),
            fields: vec![("field_name".to_string(), "value".to_string())],
            is_file: false,
        };
        let fx = begin_navigation(&mut m, submit.clone(), HistoryAction::Push, None);
        assert_eq!(fx, vec![Effect::Navigate(submit.clone())], "always fetches");
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse("submitted"),
                title: "S".to_string(),
            },
        );
        assert!(
            !m.page_cache.contains(&submit),
            "a form submit result must not be cached"
        );
    }

    #[test]
    fn cache_hit_while_loading_cancels_the_inflight_fetch() {
        let mut m = tall_model((80, 24));
        m.history.visit(tgt(1));
        m.current_dest = Some([1; 16]);
        m.page_cache.insert(
            tgt(2),
            CacheEntry {
                doc: long_doc(),
                title: "Two".to_string(),
                scroll: 0,
            },
        );
        m.pending = Some(Pending {
            target: tgt(9),
            action: HistoryAction::Push,
        });
        let fx = begin_navigation(&mut m, tgt(2), HistoryAction::Push, None);
        assert_eq!(fx, vec![Effect::Cancel], "a stale fetch is cancelled");
        assert!(m.pending.is_none());
        assert_eq!(m.title, "Two");
    }

    #[test]
    fn cached_marker_shown_only_when_view_is_cached() {
        let mut m = loaded_model((80, 24));
        let fresh = flat(&render(&m, 80, 24));
        assert!(!fresh.contains(CACHED_GLYPH), "no bolt on a fresh page");
        m.cached_view = true;
        let cached = flat(&render(&m, 80, 24));
        assert!(
            cached.contains(CACHED_GLYPH),
            "cache bolt shown when cached:\n{cached}"
        );
    }

    // --- scrolling (retained from phase 2) -------------------------------

    #[test]
    fn scrolled_is_the_one_scroll_rule() {
        let vp = 10;
        let max = 90;
        // Line motions step one, clamped at both ends.
        assert_eq!(scrolled(5, ScrollCmd::LineDown, vp, max), 6);
        assert_eq!(scrolled(5, ScrollCmd::LineUp, vp, max), 4);
        assert_eq!(scrolled(0, ScrollCmd::LineUp, vp, max), 0, "clamps at 0");
        assert_eq!(
            scrolled(max, ScrollCmd::LineDown, vp, max),
            max,
            "clamps at max"
        );
        // Full page uses the viewport, half page uses viewport/2.
        assert_eq!(scrolled(0, ScrollCmd::PageDown, vp, max), 10);
        assert_eq!(scrolled(20, ScrollCmd::PageUp, vp, max), 10);
        assert_eq!(scrolled(0, ScrollCmd::HalfPageDown, vp, max), 5);
        assert_eq!(scrolled(20, ScrollCmd::HalfPageUp, vp, max), 15);
        // Page motions still clamp to the ends.
        assert_eq!(scrolled(85, ScrollCmd::PageDown, vp, max), max);
        assert_eq!(scrolled(3, ScrollCmd::PageUp, vp, max), 0);
        // Top and Bottom jump straight to the extremes.
        assert_eq!(scrolled(42, ScrollCmd::Top, vp, max), 0);
        assert_eq!(scrolled(0, ScrollCmd::Bottom, vp, max), max);
        // A zero viewport never divides by zero and still advances by one.
        assert_eq!(scrolled(0, ScrollCmd::HalfPageDown, 0, max), 1);
        // A max of 0 (content fits) pins every motion at 0.
        assert_eq!(scrolled(0, ScrollCmd::PageDown, vp, 0), 0);
        assert_eq!(scrolled(0, ScrollCmd::Bottom, vp, 0), 0);
    }

    #[test]
    fn apply_scroll_math() {
        let mut m = Model {
            page: vec![RLine::default(); 100],
            ..Model::default()
        };
        let vp = 10;
        m.apply_scroll(ScrollCmd::LineDown, vp);
        assert_eq!(m.scroll, 1, "LineDown increments");

        m.scroll = 0;
        m.apply_scroll(ScrollCmd::PageDown, vp);
        assert_eq!(m.scroll, 10, "PageDown moves by viewport");

        m.scroll = 0;
        m.apply_scroll(ScrollCmd::HalfPageDown, vp);
        assert_eq!(m.scroll, 5, "HalfPage moves by viewport/2");

        m.scroll = 42;
        m.apply_scroll(ScrollCmd::Top, vp);
        assert_eq!(m.scroll, 0, "Top is 0");

        m.apply_scroll(ScrollCmd::Bottom, vp);
        assert_eq!(m.scroll, 90, "Bottom is max = len - viewport");

        m.apply_scroll(ScrollCmd::LineDown, vp);
        assert_eq!(m.scroll, 90, "LineDown clamps at bottom");
        m.apply_scroll(ScrollCmd::PageDown, vp);
        assert_eq!(m.scroll, 90, "PageDown clamps at bottom");

        m.scroll = 0;
        m.apply_scroll(ScrollCmd::LineUp, vp);
        assert_eq!(m.scroll, 0, "LineUp clamps at top");
        m.apply_scroll(ScrollCmd::PageUp, vp);
        assert_eq!(m.scroll, 0, "PageUp clamps at top");
    }

    #[test]
    fn apply_scroll_no_overflow_when_page_shorter_than_viewport() {
        let mut m = Model {
            page: vec![RLine::default(); 3],
            ..Model::default()
        };
        let vp = 10;
        for cmd in [
            ScrollCmd::LineDown,
            ScrollCmd::PageDown,
            ScrollCmd::HalfPageDown,
            ScrollCmd::Bottom,
        ] {
            m.scroll = 0;
            m.apply_scroll(cmd, vp);
            assert_eq!(m.scroll, 0, "{cmd:?} must clamp to 0 for a short page");
        }
        let mut e = Model::default();
        e.apply_scroll(ScrollCmd::PageDown, 0);
        e.apply_scroll(ScrollCmd::Bottom, 0);
        assert_eq!(e.scroll, 0);
    }

    fn long_doc() -> leviculum_micron::MicronDocument {
        let words: Vec<String> = (0..300).map(|i| format!("word{i:03}")).collect();
        parse(&words.join(" "))
    }

    fn tall_model(size: (u16, u16)) -> Model {
        Model::from_document(&long_doc(), content_width(size.0), "Long", size)
    }

    #[test]
    fn update_scroll_keys() {
        let mut m = tall_model((40, 13)); // viewport 13 - 2 = 11
        let vp = m.viewport();
        assert_eq!(vp, 11);
        assert_eq!(m.scroll, 0);

        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(m.scroll, 1, "j scrolls down one line");

        press(&mut m, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(m.scroll, 1 + vp, "Ctrl-f pages down by the viewport");

        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        let bottom = m.max_scroll(vp);
        assert!(bottom > 0);
        assert_eq!(m.scroll, bottom, "G jumps to the bottom");

        press(&mut m, KeyCode::Char('v'), KeyModifiers::ALT);
        assert_eq!(m.scroll, bottom - vp, "Alt-v pages up by the viewport");

        let before = m.scroll;
        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(
            m.scroll,
            (before + WHEEL_STEP).min(bottom),
            "wheel down scrolls by WHEEL_STEP"
        );
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn vi_and_emacs_map_same_command() {
        let cases = [
            (
                key(KeyCode::Char('j'), KeyModifiers::NONE),
                key(KeyCode::Char('n'), KeyModifiers::CONTROL),
            ),
            (
                key(KeyCode::Char('k'), KeyModifiers::NONE),
                key(KeyCode::Char('p'), KeyModifiers::CONTROL),
            ),
            (
                key(KeyCode::Char('f'), KeyModifiers::CONTROL),
                key(KeyCode::Char(' '), KeyModifiers::NONE),
            ),
            (
                key(KeyCode::Char('b'), KeyModifiers::CONTROL),
                key(KeyCode::Char('v'), KeyModifiers::ALT),
            ),
            (
                key(KeyCode::Char('g'), KeyModifiers::NONE),
                key(KeyCode::Home, KeyModifiers::NONE),
            ),
            (
                key(KeyCode::Char('G'), KeyModifiers::NONE),
                key(KeyCode::End, KeyModifiers::NONE),
            ),
        ];
        for (vi, emacs) in cases {
            let cmd = key_to_scroll(&vi);
            assert!(cmd.is_some(), "{vi:?} should map to a scroll command");
            assert_eq!(cmd, key_to_scroll(&emacs), "{vi:?} vs {emacs:?}");
        }
    }

    #[test]
    fn resize_rewraps_and_clamps() {
        let mut m = Model::from_document(&long_doc(), content_width(100), "Long", (100, 20));
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);

        update(&mut m, AppEvent::Resize(40, 20));
        let narrow = m.page.len();
        assert!(
            m.scroll <= m.max_scroll(m.viewport()),
            "scroll must stay clamped after shrinking"
        );

        update(&mut m, AppEvent::Resize(100, 20));
        let wide = m.page.len();
        assert!(
            m.scroll <= m.max_scroll(m.viewport()),
            "scroll must stay clamped after growing"
        );
        assert!(
            narrow > wide,
            "a narrower width must re-wrap into more lines: {narrow} vs {wide}"
        );
    }

    // --- view (TestBackend) ---------------------------------------------

    /// A loaded model with a seeded history entry, ready to render.
    fn loaded_model(size: (u16, u16)) -> Model {
        let mut m =
            Model::from_document(&parse(SAMPLE), content_width(size.0), " Test Node ", size);
        m.history.visit(tgt(0xab));
        m.current_dest = Some([0xab; 16]);
        m
    }

    fn render(model: &Model, w: u16, h: u16) -> ratatui::buffer::Buffer {
        render_with_images(model, &ImageStore::default(), w, h)
    }

    /// Render with a specific image store. The default one holds no pictures
    /// and draws none, which is what every test that is not about images
    /// wants; the image tests build one that does.
    fn render_with_images(
        model: &Model,
        images: &ImageStore,
        w: u16,
        h: u16,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view(model, images, frame))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn topbar_is_one_row_title_address_and_cluster() {
        let m = loaded_model((80, 24));
        let buffer = render(&m, 80, 24);
        // Everything lives on the single identity row (row 0).
        let top = row_text(&buffer, 0, 80);
        assert!(top.contains("Test Node"), "title missing: {top:?}");
        // The address, carrying the path exactly once (never in the title too).
        assert!(top.contains(":/page/171.mu"), "address missing: {top:?}");
        assert_eq!(
            top.matches(":/page/171.mu").count(),
            1,
            "path must appear exactly once: {top:?}"
        );
        // Exactly one · separator between title and address.
        assert_eq!(top.matches('·').count(), 1, "want one separator: {top:?}");
        // The status cluster shows a hollow star (this page is not bookmarked).
        assert!(top.contains(STAR_OFF), "bookmark star missing: {top:?}");
    }

    #[test]
    fn status_bar_renders_hints() {
        let m = loaded_model((80, 24));
        let buffer = render(&m, 80, 24);
        let status = row_text(&buffer, 23, 80);
        assert!(status.contains("quit"), "hints missing: {status:?}");
    }

    #[test]
    fn loading_state_renders_spinner_and_text() {
        let mut m = loaded_model((80, 24));
        m.pending = Some(Pending {
            target: tgt(9),
            action: HistoryAction::Push,
        });
        let buffer = render(&m, 80, 24);
        // The loading state now floats in the bottom-left chip on the content's
        // last row (row 22), not the footer row.
        let chip = row_text(&buffer, 22, 80);
        assert!(chip.contains("loading"), "loading missing: {chip:?}");
    }

    #[test]
    fn hsv_to_rgb_known_conversions() {
        // The primary and secondary hues at full saturation and value.
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), (255, 0, 0), "h=0 -> red");
        assert_eq!(hsv_to_rgb(60.0, 1.0, 1.0), (255, 255, 0), "h=60 -> yellow");
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), (0, 255, 0), "h=120 -> green");
        assert_eq!(hsv_to_rgb(180.0, 1.0, 1.0), (0, 255, 255), "h=180 -> cyan");
        assert_eq!(hsv_to_rgb(240.0, 1.0, 1.0), (0, 0, 255), "h=240 -> blue");
        assert_eq!(
            hsv_to_rgb(300.0, 1.0, 1.0),
            (255, 0, 255),
            "h=300 -> magenta"
        );
        // Hue wraps: 360 == 0, and negatives fold back into range.
        assert_eq!(
            hsv_to_rgb(360.0, 1.0, 1.0),
            (255, 0, 0),
            "h=360 wraps to red"
        );
        assert_eq!(
            hsv_to_rgb(-60.0, 1.0, 1.0),
            (255, 0, 255),
            "h=-60 -> magenta"
        );
        // s=1, v=1 always yields a fully saturated, full-brightness colour: one
        // channel at 255 and at least one at 0.
        for deg in (0..360).step_by(7) {
            let (r, g, b) = hsv_to_rgb(deg as f32, 1.0, 1.0);
            assert_eq!(r.max(g).max(b), 255, "not full brightness at h={deg}");
            assert_eq!(r.min(g).min(b), 0, "not fully saturated at h={deg}");
        }
    }

    #[test]
    fn spinner_span_glides_smoothly_through_the_spectrum() {
        // The glyph steps through the six braille frames as `spin` advances.
        for spin in 0..12 {
            assert_eq!(
                &*spinner_span(spin).content,
                SPIN_FRAMES[spin % SPIN_FRAMES.len()],
                "glyph out of cycle at spin={spin}"
            );
        }
        // Every frame is bold and carries the HSV colour for its hue.
        let cycle = (360.0 / HUE_STEP).ceil() as usize;
        for spin in 0..cycle {
            let style = spinner_span(spin).style;
            assert!(
                style.add_modifier.contains(Modifier::BOLD),
                "spinner not bold at spin={spin}"
            );
            let hue = (spin as f32 * HUE_STEP).rem_euclid(360.0);
            let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
            assert_eq!(
                style.fg,
                Some(Color::Rgb(r, g, b)),
                "wrong hsv fg at spin={spin}"
            );
        }
        // The colour glides smoothly: consecutive ticks shift the hue by only the
        // small HUE_STEP, so each RGB channel moves by a bounded amount (no hard
        // jumps). A single HUE_STEP spans at most one 60-degree sextant, over which
        // a channel changes by at most 255.
        let max_delta = (HUE_STEP / 60.0 * 255.0).ceil() as i32 + 1;
        for spin in 0..cycle {
            let a = spinner_span(spin).style.fg;
            let b = spinner_span(spin + 1).style.fg;
            let (Some(Color::Rgb(ar, ag, ab)), Some(Color::Rgb(br, bg, bb))) = (a, b) else {
                panic!("spinner fg not an Rgb colour at spin={spin}");
            };
            assert_ne!(a, b, "hue did not change between spin={spin} and next");
            for (x, y, ch) in [(ar, br, "r"), (ag, bg, "g"), (ab, bb, "b")] {
                let d = (x as i32 - y as i32).abs();
                assert!(
                    d <= max_delta,
                    "channel {ch} jumped by {d} (> {max_delta}) at spin={spin}"
                );
            }
        }
        // A full cycle spans the whole spectrum: the hue reaches both ends.
        let hues: Vec<f32> = (0..cycle)
            .map(|s| (s as f32 * HUE_STEP).rem_euclid(360.0))
            .collect();
        let min = hues.iter().cloned().fold(f32::MAX, f32::min);
        let max = hues.iter().cloned().fold(f32::MIN, f32::max);
        assert!(min < 30.0, "cycle does not start near hue 0 (min={min})");
        assert!(max > 330.0, "cycle does not reach the far end (max={max})");
    }

    #[test]
    fn loading_status_bar_has_rainbow_bold_spinner_cell() {
        let mut m = loaded_model((80, 24));
        m.pending = Some(Pending {
            target: tgt(9),
            action: HistoryAction::Push,
        });
        m.spin = 3;
        let hue = (m.spin as f32 * HUE_STEP).rem_euclid(360.0);
        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
        let buffer = render(&m, 80, 24);
        // The spinner now lives in the bottom-left chip on the content's last row.
        let mut found = false;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, 22)];
            if cell.modifier.contains(Modifier::BOLD) && cell.fg == Color::Rgb(r, g, b) {
                found = true;
            }
        }
        assert!(found, "no bold rainbow spinner cell in the loading chip");
    }

    #[test]
    fn error_toast_renders_over_content_and_status_keeps_hints() {
        let mut m = loaded_model((80, 24));
        m.set_toast(ToastKind::Error, "no path to destination");
        let buffer = render(&m, 80, 24);

        // The toast text and its warning glyph render somewhere in the content.
        let all = flat(&buffer);
        assert!(all.contains("no path"), "toast text missing:\n{all}");
        assert!(all.contains('⚠'), "error glyph missing:\n{all}");

        // The attention red is painted on the toast cells.
        let mut found_error_style = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if buffer[(x, y)].fg == Color::Rgb(255, 95, 95) {
                    found_error_style = true;
                }
            }
        }
        assert!(found_error_style, "error style not painted");

        // The status bar (row 23) still shows the key-hints, not the message.
        let status = row_text(&buffer, 23, 80);
        assert!(status.contains("quit"), "hints missing: {status:?}");
        assert!(
            !status.contains("no path"),
            "message leaked into status bar"
        );
    }

    #[test]
    fn set_toast_stores_kind_and_text() {
        let mut m = Model::default();
        m.set_toast(ToastKind::Info, "copied aa11:/page/x.mu");
        let toast = m.toast.as_ref().expect("toast stored");
        assert_eq!(toast.kind, ToastKind::Info);
        assert_eq!(toast.text, "copied aa11:/page/x.mu");
        assert_eq!(toast.shown_at, m.now_tick);
    }

    #[test]
    fn toast_expires_after_the_timeout() {
        let mut m = Model::default();
        m.set_toast(ToastKind::Info, "copied");
        // Just short of the lifetime it is still up.
        for _ in 0..TOAST_TICKS - 1 {
            update(&mut m, AppEvent::Tick);
        }
        assert!(m.toast.is_some(), "toast dismissed too early");
        // One more tick crosses the lifetime and clears it.
        update(&mut m, AppEvent::Tick);
        assert!(m.toast.is_none(), "toast should have expired");
    }

    #[test]
    fn any_key_dismisses_the_toast() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        m.set_toast(ToastKind::Info, "bookmarked");
        // A plain scroll key is a navigation event: it clears the toast.
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(m.toast.is_none(), "a key event should dismiss the toast");
    }

    #[test]
    fn needs_tick_tracks_loading_and_toast() {
        let mut m = Model::default();
        assert!(!m.needs_tick(), "idle model needs no tick");
        m.set_toast(ToastKind::Info, "up");
        assert!(m.needs_tick(), "an active toast must drive the tick");
    }

    #[test]
    fn bad_link_follow_sets_error_toast_with_url_not_status_bar() {
        // A same-destination link with no current destination fails to resolve.
        let mut m = loaded_model((80, 24));
        m.current_dest = None;
        m.links = vec![RenderedLink {
            index: 1,
            label: "Broken".to_string(),
            target: ":/page/broken.mu".to_string(),
            ..RenderedLink::default()
        }];
        let effects = follow_link(&mut m, 1);
        assert!(effects.is_empty(), "a broken link must not navigate");
        let toast = m.toast.as_ref().expect("a bad-link toast is raised");
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(
            toast.text.contains(":/page/broken.mu"),
            "toast must carry the offending url: {}",
            toast.text
        );

        // The offending url shows in the toast overlay, never in the status bar.
        let buffer = render(&m, 80, 24);
        let status = row_text(&buffer, 23, 80);
        assert!(
            !status.contains(":/page/broken.mu"),
            "the url leaked into the status bar: {status:?}"
        );
        assert!(
            status.contains("quit"),
            "status bar lost its hints: {status:?}"
        );
    }

    #[test]
    fn view_renders_heading_and_underlined_link_label() {
        let model = model_from_sample(content_width(80), (80, 24));
        let buffer = render(&model, 80, 24);
        let text = flat(&buffer);
        assert!(text.contains("Sample Page"), "heading missing:\n{text}");
        assert!(text.contains("Alpha"), "link label missing:\n{text}");

        let link_fg = Color::Rgb(0, 175, 255);
        let mut found_styled_link = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "A"
                    && cell.fg == link_fg
                    && cell.modifier.contains(Modifier::UNDERLINED)
                {
                    found_styled_link = true;
                }
            }
        }
        assert!(
            found_styled_link,
            "no underlined LINK_FG 'A' cell found:\n{text}"
        );
    }

    /// Find the fg colour of the underlined link-label 'A' cell in a rendered
    /// buffer, if any.
    fn link_label_fg(buffer: &ratatui::buffer::Buffer) -> Option<Color> {
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "A" && cell.modifier.contains(Modifier::UNDERLINED) {
                    return Some(cell.fg);
                }
            }
        }
        None
    }

    #[test]
    fn ansi256_depth_downgrades_link_colour_to_indexed() {
        let mut model = model_from_sample(content_width(80), (80, 24));
        model.depth = ColorDepth::Ansi256;
        let buffer = render(&model, 80, 24);
        // The link label keeps its colour, but as the nearest palette index, not
        // a 24-bit Rgb triple.
        let expect = Color::Indexed(rgb_to_ansi256(0, 175, 255));
        assert_eq!(
            link_label_fg(&buffer),
            Some(expect),
            "Ansi256 render should downgrade the link fg to an indexed colour"
        );
    }

    #[test]
    fn truecolor_depth_keeps_link_colour_as_rgb() {
        let mut model = model_from_sample(content_width(80), (80, 24));
        model.depth = ColorDepth::Truecolor;
        let buffer = render(&model, 80, 24);
        assert_eq!(
            link_label_fg(&buffer),
            Some(Color::Rgb(0, 175, 255)),
            "Truecolor render should keep the link fg as a 24-bit Rgb triple"
        );
    }

    #[test]
    fn no_color_suppresses_the_ansi256_downgrade() {
        // NO_COLOR still wins: even with an Ansi256 depth, the downgrade post-pass
        // is skipped so the reverse-video chrome path is untouched.
        let mut model = model_from_sample(content_width(80), (80, 24));
        model.no_color = true;
        model.depth = ColorDepth::Ansi256;
        let buffer = render(&model, 80, 24);
        // No cell carries an Indexed colour (the downgrade never ran).
        let has_indexed = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                let cell = &buffer[(x, y)];
                matches!(cell.fg, Color::Indexed(_)) || matches!(cell.bg, Color::Indexed(_))
            })
        });
        assert!(!has_indexed, "no_color must suppress the Ansi256 downgrade");
    }

    #[test]
    fn light_theme_view_uses_light_link_and_heading_band() {
        let mut model = model_from_sample(content_width(80), (80, 24));
        model.toggle_theme();
        assert_eq!(model.theme, Theme::Light);
        let buffer = render(&model, 80, 24);

        // The link label 'A' now carries the light (deep-blue) link colour.
        let light_link = Color::Rgb(0, 90, 170);
        let mut found_light_link = false;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "A"
                    && cell.fg == light_link
                    && cell.modifier.contains(Modifier::UNDERLINED)
                {
                    found_light_link = true;
                }
            }
        }
        assert!(found_light_link, "no light-blue underlined link cell found");

        // The depth-1 "Sample Page" heading carries the light band bg #777777
        // (the dark theme would band it #bbbbbb instead).
        let light_band_bg = Color::Rgb(0x77, 0x77, 0x77);
        let has_light_band = (0..buffer.area.height)
            .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].bg == light_band_bg));
        assert!(has_light_band, "light heading band not found");
        // The dark depth-1 band must not appear under the light theme.
        let dark_band_bg = Color::Rgb(0xbb, 0xbb, 0xbb);
        let has_dark_band = (0..buffer.area.height)
            .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].bg == dark_band_bg));
        assert!(!has_dark_band, "dark heading band leaked into light theme");
    }

    #[test]
    fn t_toggles_theme_and_view_reflects_it() {
        let mut m = loaded_model((80, 24));
        assert_eq!(m.theme, Theme::Dark);

        // `t` flips the model to the light theme...
        let effects = press(&mut m, KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(effects.is_empty());
        assert_eq!(m.theme, Theme::Light);
        let light_bg = rgb(Theme::Light.chrome_bg());
        let buffer = render(&m, 80, 24);
        assert_eq!(
            buffer[(0, 0)].bg,
            light_bg,
            "view must reflect the light chrome after toggling"
        );

        // ...and `t` again flips back to dark.
        press(&mut m, KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(m.theme, Theme::Dark);
        let dark_bg = rgb(Theme::Dark.chrome_bg());
        let buffer = render(&m, 80, 24);
        assert_eq!(
            buffer[(0, 0)].bg,
            dark_bg,
            "view must return to dark chrome"
        );
    }

    #[test]
    fn view_scrolls_slice_below_topbar_and_draws_scrollbar() {
        let (w, h) = (40u16, 13u16);
        let mut m = Model::from_document(&long_doc(), content_width(w), "Long", (w, h));
        assert!(
            m.page.len() > m.viewport(),
            "fixture must exceed the viewport"
        );
        let content_top = TOPBAR_ROWS; // content begins after the two-row top-bar

        let buffer = render(&m, w, h);
        let top0 = row_text(&buffer, content_top, w - SCROLLBAR_COLS);
        assert_eq!(top0.trim_end(), line_text(&m.page[0]).trim_end());

        let vp = m.viewport();
        press(&mut m, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(m.scroll, vp);
        let buffer = render(&m, w, h);
        let top1 = row_text(&buffer, content_top, w - SCROLLBAR_COLS);
        assert_eq!(top1.trim_end(), line_text(&m.page[vp]).trim_end());

        // The scrollbar occupies the reserved right-hand column of the content.
        let mut scrollbar_cell = false;
        for y in content_top..h {
            if buffer[(w - SCROLLBAR_COLS, y)].symbol() != " " {
                scrollbar_cell = true;
            }
        }
        assert!(scrollbar_cell, "scrollbar column should not be empty");
    }

    #[test]
    fn scrollbar_thumb_reaches_top_and_bottom() {
        // A page much taller than the viewport, so the thumb is a small handle
        // that must travel the whole track as `scroll` runs `0..=max_scroll`.
        let (w, h) = (40u16, 13u16);
        let mut m = Model::from_document(&long_doc(), content_width(w), "Long", (w, h));
        assert!(
            m.page.len() > m.viewport() * 2,
            "fixture must be much taller than the viewport"
        );

        let col = w - SCROLLBAR_COLS; // rightmost column carries the scrollbar
        let content_top = TOPBAR_ROWS; // first content row (below the two-row top-bar)
        let content_bottom = h - STATUS_ROWS - 1; // last content row (above the status bar)

        // The rows in the scrollbar column occupied by the thumb glyph (block).
        let thumb_rows = |buf: &ratatui::buffer::Buffer| -> Vec<u16> {
            (content_top..=content_bottom)
                .filter(|&y| buf[(col, y)].symbol() == "█")
                .collect()
        };

        // scroll = 0: the thumb sits at the TOP of the track, not the bottom.
        m.scroll = 0;
        let buf = render(&m, w, h);
        let top = thumb_rows(&buf);
        assert!(!top.is_empty(), "thumb should be visible at scroll 0");
        assert_eq!(
            *top.first().unwrap(),
            content_top,
            "thumb top must hit the track top at scroll 0"
        );
        assert!(
            *top.last().unwrap() < content_bottom,
            "thumb must not fill to the bottom at scroll 0"
        );

        // scroll = max_scroll: the thumb BOTTOM reaches the bottom row of the track.
        let vp = m.viewport();
        m.scroll = m.max_scroll(vp);
        assert!(m.scroll > 0, "max_scroll must be positive for a tall page");
        let buf = render(&m, w, h);
        let bottom = thumb_rows(&buf);
        assert!(!bottom.is_empty(), "thumb should be visible at max scroll");
        assert_eq!(
            *bottom.last().unwrap(),
            content_bottom,
            "thumb bottom must reach the track bottom at max scroll"
        );
        assert!(
            *bottom.first().unwrap() > content_top,
            "thumb must have left the top at max scroll"
        );

        // A mid value puts the thumb strictly between the two extremes.
        m.scroll = m.max_scroll(vp) / 2;
        let buf = render(&m, w, h);
        let mid = thumb_rows(&buf);
        assert!(!mid.is_empty(), "thumb should be visible mid-scroll");
        assert!(
            *mid.first().unwrap() > content_top && *mid.last().unwrap() < content_bottom,
            "mid-scroll thumb must sit between the track ends: {mid:?}"
        );
    }

    #[test]
    fn chrome_bars_carry_background_full_width() {
        let m = loaded_model((80, 24));
        assert!(!m.no_color, "colour is on by default");
        assert_eq!(m.theme, Theme::Dark, "dark is the default theme");
        let dark_bg = rgb(Theme::Dark.chrome_bg());
        let buffer = render(&m, 80, 24);
        // The one-row top bar (0) and the status row (23) carry the chrome
        // background across the full width, including the rightmost column.
        for &y in &[0u16, 23] {
            for &x in &[0u16, 40, 79] {
                assert_eq!(
                    buffer[(x, y)].bg,
                    dark_bg,
                    "cell ({x},{y}) is missing the chrome background"
                );
            }
        }
    }

    #[test]
    fn chrome_bars_carry_light_background_under_light_theme() {
        let mut m = loaded_model((80, 24));
        m.toggle_theme();
        assert_eq!(m.theme, Theme::Light);
        let light_bg = rgb(Theme::Light.chrome_bg());
        let buffer = render(&m, 80, 24);
        for &y in &[0u16, 23] {
            for &x in &[0u16, 40, 79] {
                assert_eq!(
                    buffer[(x, y)].bg,
                    light_bg,
                    "cell ({x},{y}) is missing the light chrome background"
                );
            }
        }
    }

    #[test]
    fn chrome_bars_use_reverse_under_no_color() {
        let dark_bg = rgb(Theme::Dark.chrome_bg());
        for theme in [Theme::Dark, Theme::Light] {
            let mut m = loaded_model((80, 24));
            m.no_color = true;
            m.theme = theme;
            m.relayout(m.width);
            let buffer = render(&m, 80, 24);
            for &y in &[0u16, 23] {
                for &x in &[0u16, 40, 79] {
                    let cell = &buffer[(x, y)];
                    assert!(
                        cell.modifier.contains(Modifier::REVERSED),
                        "cell ({x},{y}) must be reversed under no_color ({theme:?})"
                    );
                    assert_ne!(
                        cell.bg, dark_bg,
                        "no_color must not paint the colour background ({theme:?})"
                    );
                }
            }
        }
    }

    /// The laid-out footer slot whose button carries `action`.
    fn footer_slot(m: &Model, action: FooterAction) -> FooterSlot {
        let footer = regions(m.frame_area())[2];
        footer_layout(m, footer)
            .into_iter()
            .find(|s| s.item.action == action)
            .expect("footer slot")
    }

    #[test]
    fn footer_key_is_bright_bold_and_label_is_muted() {
        // The grouping is carried by contrast, not spacing: the key cell renders
        // in the bright chrome fg and BOLD (never DIM); the label cell in the
        // muted chrome fg; and the two clearly differ. Checked under both themes.
        for theme in [Theme::Dark, Theme::Light] {
            let mut m = loaded_model((80, 24));
            if theme == Theme::Light {
                m.toggle_theme();
            }
            assert_eq!(m.theme, theme);
            // The `q quit` button: its key is available, so it is the active case.
            let slot = footer_slot(&m, FooterAction::Quit);
            let buffer = render(&m, 80, 24);
            let key = &buffer[(slot.rect.x, 23u16)];
            assert_eq!(
                key.fg,
                rgb(theme.chrome_fg()),
                "footer key must use the bright chrome fg ({theme:?})"
            );
            assert!(
                key.modifier.contains(Modifier::BOLD),
                "footer key must be BOLD ({theme:?})"
            );
            assert!(
                !key.modifier.contains(Modifier::DIM),
                "footer key must not be DIM ({theme:?})"
            );
            // The label ("quit") starts two columns past the key ("q" + a space).
            let label = &buffer[(slot.rect.x + 2, 23u16)];
            assert_eq!(
                label.fg,
                rgb(theme.chrome_muted_fg()),
                "footer label must use the muted chrome fg ({theme:?})"
            );
            assert_ne!(key.fg, label.fg, "key and label must differ ({theme:?})");
        }
    }

    #[test]
    fn step_event_exits_on_stream_error_and_close() {
        // A Some(Err) from the event stream is a genuine IO error (stdin/poll
        // failure); crossterm discards unparseable sequences internally and
        // never surfaces them as errors. So it EXITS the loop, carrying the
        // error, rather than being ignored (which would busy-loop a dead tty).
        let err = io::Error::other("poll failure");
        assert!(matches!(step_event(Some(Err(err))), EventStep::End(Err(_))));
        // A closed stream (None) is a clean exit.
        assert!(matches!(step_event(None), EventStep::End(Ok(()))));
        // A real key still maps to an applied app-event.
        let ev = Event::Key(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(
            step_event(Some(Ok(ev))),
            EventStep::Apply(AppEvent::Key(_))
        ));
    }

    #[test]
    fn help_overlay_renders_keybindings() {
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(m.show_help);
        let buffer = render(&m, 120, 40);
        let text = flat(&buffer);
        assert!(text.contains("help"), "help title missing:\n{text}");
        assert!(text.contains("quit"), "help body missing:\n{text}");
        // Esc dismisses it.
        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!m.show_help);
    }

    #[test]
    fn help_overlay_widens_to_the_longest_line() {
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let buffer = render(&m, 120, 40);
        let text = flat(&buffer);
        // A wide row renders contiguously rather than wrapping onto two lines.
        assert!(
            text.lines()
                .any(|row| row.contains("hint — type a label or the link text")),
            "widest help line wrapped:\n{text}"
        );
        // The overlay is at least the widest line plus its two borders wide.
        let inner = help_lines()
            .iter()
            .map(|l| UnicodeWidthStr::width(l.as_str()))
            .max()
            .unwrap();
        let border = text
            .lines()
            .find(|row| row.contains('┌'))
            .expect("top border row");
        let chars: Vec<char> = border.chars().collect();
        let start = chars.iter().position(|&c| c == '┌').unwrap();
        let end = chars.iter().position(|&c| c == '┐').unwrap();
        let width = end - start + 1;
        assert!(
            width >= inner + 2,
            "overlay width {width} < longest line {inner} + 2",
        );
    }

    #[test]
    fn help_lists_every_scroll_binding() {
        // Every keys-string in the scroll keymap appears verbatim in the rendered
        // overlay: the table is the single source, so the help cannot omit a
        // binding the key handler honours.
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let text = flat(&render(&m, 120, 40));
        for row in SCROLL_KEYS {
            assert!(
                text.lines().any(|l| l.contains(row.keys)),
                "scroll binding {:?} missing from help:\n{text}",
                row.keys
            );
        }
    }

    #[test]
    fn help_descriptions_all_align() {
        // Every description starts at the same display column: the keys column is
        // computed, never hand-padded, so alignment is structural.
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let text = flat(&render(&m, 120, 40));
        let rows: Vec<&str> = text.lines().collect();
        let mut cols = Vec::new();
        for group in help_groups() {
            for e in &group.entries {
                let row = rows
                    .iter()
                    .find(|l| l.contains(e.keys) && l.contains(e.desc))
                    .unwrap_or_else(|| panic!("help row for {:?} missing:\n{text}", e.keys));
                // Char count, not byte offset: the border and arrows are multibyte
                // but each occupies one display column.
                let byte = row.find(e.desc).expect("desc column");
                cols.push(row[..byte].chars().count());
            }
        }
        let first = cols[0];
        assert!(
            cols.iter().all(|&c| c == first),
            "descriptions misaligned: columns {cols:?}"
        );
    }

    #[test]
    fn help_lines_never_exceed_the_overlay_width() {
        // The overlay sizes itself to its widest line, so no content line wraps:
        // every built line renders intact on a single row.
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let text = flat(&render(&m, 120, 40));
        for line in help_lines().iter().filter(|l| !l.is_empty()) {
            assert!(
                text.lines().any(|row| row.contains(line.as_str())),
                "help line wrapped or missing: {line:?}\n{text}"
            );
        }
    }

    #[test]
    fn address_mode_renders_editor() {
        let mut m = loaded_model((80, 24));
        press(&mut m, KeyCode::Char(':'), KeyModifiers::NONE);
        type_str(&mut m, "abc");
        let buffer = render(&m, 80, 24);
        let text = flat(&buffer);
        assert!(text.contains("abc"), "editor content missing:\n{text}");
    }

    // --- phase 4: visible_links, hints, focus, mouse --------------------

    /// A left-button mouse-down event at `(col, row)`.
    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// A mouse-move event at `(col, row)`.
    fn moved(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The laid-out link with 1-based `index`.
    fn link_of(m: &Model, index: usize) -> &RenderedLink {
        m.links.iter().find(|l| l.index == index).expect("link")
    }

    #[test]
    fn visible_links_map_content_lines_to_screen_rects() {
        let m = model_from_sample(content_width(80), (80, 24));
        let vls = visible_links(&m);
        assert_eq!(vls.len(), 2, "sample has two links");
        for vl in &vls {
            let link = link_of(&m, vl.index);
            // row = top-bar rows + (content line - scroll); scroll is 0 here.
            assert_eq!(vl.row, TOPBAR_ROWS + link.line as u16);
            assert_eq!(vl.col_start, link.col_start as u16);
            assert_eq!(vl.col_end, link.col_end as u16);
        }
    }

    #[test]
    fn visible_links_exclude_off_viewport() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        // Scroll past both links (they sit near the top of the page).
        m.scroll = link_of(&m, 2).line + 1;
        assert!(
            visible_links(&m).is_empty(),
            "links above the scroll must be excluded"
        );
    }

    #[test]
    fn visible_links_partial_scroll_keeps_only_lower_link() {
        let mut m = model_from_sample(content_width(80), (80, 24));
        let beta_line = link_of(&m, 2).line;
        m.scroll = beta_line;
        let vls = visible_links(&m);
        assert_eq!(vls.len(), 1, "only the lower link stays visible");
        assert_eq!(vls[0].index, 2);
        // The now-topmost visible link sits at the content top row.
        assert_eq!(vls[0].row, TOPBAR_ROWS);
    }

    #[test]
    fn hint_labels_are_single_char_when_few() {
        let labels = hint_labels(3);
        assert_eq!(labels, vec!["a", "s", "d"]);
        // All nine of the alphabet fit as single characters.
        assert!(hint_labels(9).iter().all(|l| l.len() == 1));
        let nine = hint_labels(9);
        assert_eq!(nine.len(), 9);
        assert_eq!(
            nine.iter().collect::<std::collections::HashSet<_>>().len(),
            9
        );
    }

    #[test]
    fn hint_labels_are_two_char_when_many() {
        let labels = hint_labels(10);
        assert_eq!(labels.len(), 10);
        assert!(
            labels.iter().all(|l| l.chars().count() == 2),
            "got: {labels:?}"
        );
        assert_eq!(labels[0], "aa");
        // Unique across the whole set.
        let uniq: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(uniq.len(), 10);
    }

    #[test]
    fn hints_cover_visible_links_then_fields_then_controls_with_unique_labels() {
        let m = loaded_model((80, 24));
        let hs = hints(&m);
        // Two links + one form field + two top-bar controls (the address slot
        // and the star).
        assert_eq!(hs.len(), 5);
        assert!(matches!(hs[0].target, HintTarget::Link(1)));
        assert!(matches!(hs[1].target, HintTarget::Link(2)));
        assert!(matches!(hs[2].target, HintTarget::Field(1)));
        assert!(matches!(
            hs[3].target,
            HintTarget::Control(Control::Address)
        ));
        assert!(matches!(
            hs[4].target,
            HintTarget::Control(Control::Bookmark)
        ));
        let labels: std::collections::HashSet<_> = hs.iter().map(|h| &h.label).collect();
        assert_eq!(labels.len(), 5, "labels must be unique");
    }

    #[test]
    fn f_enters_hint_mode() {
        let mut m = loaded_model((80, 24));
        let effects = press(&mut m, KeyCode::Char('f'), KeyModifiers::NONE);
        assert!(effects.is_empty());
        assert_eq!(m.mode, Mode::Hint);
    }

    #[test]
    fn typing_a_unique_label_follows_that_link() {
        let mut m = loaded_model((80, 24));
        press(&mut m, KeyCode::Char('f'), KeyModifiers::NONE);
        // The first link's label is 'a'.
        let effects = press(&mut m, KeyCode::Char('a'), KeyModifiers::NONE);
        let expected = browser::resolve_link(link_of(&m, 1), m.current_dest)
            .unwrap()
            .0;
        assert_eq!(effects, vec![Effect::Navigate(expected.clone())]);
        assert_eq!(m.mode, Mode::Browse, "hint mode exits after a match");
        assert_eq!(m.pending.as_ref().map(|p| &p.target), Some(&expected));
    }

    #[test]
    fn typing_link_text_follows_that_link() {
        let mut m = loaded_model((80, 24));
        press(&mut m, KeyCode::Char('f'), KeyModifiers::NONE);
        // 'b' matches no label but uniquely matches the "• Beta" link text.
        let effects = press(&mut m, KeyCode::Char('b'), KeyModifiers::NONE);
        let expected = browser::resolve_link(link_of(&m, 2), m.current_dest)
            .unwrap()
            .0;
        assert_eq!(effects, vec![Effect::Navigate(expected)]);
        assert_eq!(m.mode, Mode::Browse);
    }

    #[test]
    fn non_matching_hint_key_is_ignored() {
        let mut m = loaded_model((80, 24));
        press(&mut m, KeyCode::Char('f'), KeyModifiers::NONE);
        // 'z' is neither a label prefix nor in any link's text.
        let effects = press(&mut m, KeyCode::Char('z'), KeyModifiers::NONE);
        assert!(effects.is_empty());
        assert_eq!(m.mode, Mode::Hint, "still hinting");
        assert!(m.hint_input.is_empty(), "invalid key not accepted");
    }

    #[test]
    fn esc_exits_hint_mode() {
        let mut m = loaded_model((80, 24));
        press(&mut m, KeyCode::Char('f'), KeyModifiers::NONE);
        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(m.mode, Mode::Browse);
    }

    #[test]
    fn tab_cycles_focus_and_wraps() {
        // The sample page has two links then one text field, in document order.
        let mut m = loaded_model((80, 24));
        assert_eq!(m.focus, None);
        assert_eq!(m.field_focus, None);
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.focus, Some(1));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.focus, Some(2));
        // The third element is the text field: link focus clears, field focus
        // takes over, and the mode switches to field editing.
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.focus, None);
        assert_eq!(m.field_focus, Some(1));
        assert_eq!(m.mode, Mode::Field);
        // Wrap back to the first link (and back to browse mode).
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.focus, Some(1));
        assert_eq!(m.field_focus, None);
        assert_eq!(m.mode, Mode::Browse);
        // Shift-Tab goes backward from the first link, wrapping to the field.
        press(&mut m, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(m.field_focus, Some(1));
        assert_eq!(m.mode, Mode::Field);
    }

    #[test]
    fn tab_sets_status_target() {
        let m = loaded_model((80, 24));
        let mut m2 = m.clone();
        press(&mut m2, KeyCode::Tab, KeyModifiers::NONE);
        let target = focused_link_target(&m2).expect("focused target");
        assert!(
            target.contains(&link_of(&m, 1).target),
            "status target missing the link path: {target:?}"
        );
    }

    #[test]
    fn enter_follows_focused_link() {
        let mut m = loaded_model((80, 24));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        let effects = press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        let expected = browser::resolve_link(link_of(&m, 1), m.current_dest)
            .unwrap()
            .0;
        assert_eq!(effects, vec![Effect::Navigate(expected)]);
    }

    #[test]
    fn following_external_https_link_opens_externally() {
        let mut m = Model {
            links: vec![RenderedLink {
                index: 1,
                label: "Site".to_string(),
                target: "https://example.org/x".to_string(),
                ..RenderedLink::default()
            }],
            ..Model::default()
        };
        let effects = follow_link(&mut m, 1);
        assert_eq!(
            effects,
            vec![Effect::OpenExternal("https://example.org/x".to_string())]
        );
        assert!(
            m.pending.is_none(),
            "an external link must not start a fetch"
        );
    }

    #[test]
    fn following_unsafe_scheme_link_refuses_with_toast() {
        let mut m = Model {
            links: vec![RenderedLink {
                index: 1,
                label: "Bad".to_string(),
                target: "file:///etc/passwd".to_string(),
                ..RenderedLink::default()
            }],
            ..Model::default()
        };
        let effects = follow_link(&mut m, 1);
        assert!(effects.is_empty(), "an unsafe link must yield no effect");
        assert!(m.pending.is_none(), "an unsafe link must not navigate");
        let toast = m.toast.as_ref().expect("a refusal toast is raised");
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(
            toast.text.contains("file"),
            "toast names the refused scheme: {}",
            toast.text
        );
    }

    #[test]
    fn following_lxmf_link_copies_the_address_instead_of_failing() {
        // Before this was handled, the target reached parse_url and produced
        // "bad link: ... (malformed URL)", which reads as a broken page
        // rather than an action this browser does not perform.
        let hash = "0ec84236630cea839d80a71c39fb41ce";
        let mut m = Model {
            links: vec![RenderedLink {
                index: 1,
                label: "Lew Palm".to_string(),
                target: format!("lxmf@{hash}"),
                ..RenderedLink::default()
            }],
            ..Model::default()
        };
        let effects = follow_link(&mut m, 1);
        assert_eq!(effects, vec![Effect::Copy(hash.to_string())]);
        assert!(m.pending.is_none(), "an address must not start a fetch");
        let toast = m.toast.as_ref().expect("a toast reports what happened");
        assert_eq!(toast.kind, ToastKind::Info, "this is not an error");
        assert!(
            toast.text.contains(hash),
            "the address is shown, not just copied: {}",
            toast.text
        );
    }

    /// A page that links to its own node with a bare `/page/x.mu` dropped the
    /// `:` a local URL keeps. The link is dead here exactly as it is in the
    /// reference (see `url::tests::reference_rejects_the_same_urls_we_do`), so
    /// the toast's job is to hand over the string that would have worked rather
    /// than to restate the grammar.
    #[test]
    fn link_missing_the_local_colon_is_told_which_colon() {
        let mut m = loaded_model((80, 24));
        m.links = vec![RenderedLink {
            index: 1,
            label: "About".to_string(),
            target: "/page/about.md".to_string(),
            ..RenderedLink::default()
        }];

        let effects = follow_link(&mut m, 1);
        assert!(effects.is_empty(), "a dead link must not navigate");
        assert!(m.pending.is_none());
        let toast = m.toast.as_ref().expect("an error toast is raised");
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(
            toast.text.contains(":/page/about.md"),
            "the toast must show the URL that works: {}",
            toast.text
        );
        // The suggestion must survive the single truncated toast line at 80
        // columns: the whole note fits, and in any case leads with the fix.
        assert!(
            UnicodeWidthStr::width(toast.text.as_str()) <= 74,
            "toast too long to read at 80 columns: {}",
            toast.text
        );
        let suggestion = toast.text.find(":/page/about.md").expect("suggestion");
        assert!(
            suggestion < toast.text.find("keeps").expect("explanation"),
            "the fix must come before the explanation: {}",
            toast.text
        );
    }

    /// A local URL typed with nothing open — reachable from the start screen,
    /// where the URL editor is one keypress away and no page is loaded.
    #[test]
    fn local_url_with_no_open_page_says_so() {
        let mut m = Model {
            size: (80, 24),
            ..Model::default()
        };
        m.relayout(content_width(80));
        show_start_screen(&mut m);
        m.show_places = false;

        press(&mut m, KeyCode::Char(':'), KeyModifiers::NONE);
        type_str(&mut m, ":/page/x.mu");
        press(&mut m, KeyCode::Enter, KeyModifiers::NONE);

        let toast = m.toast.as_ref().expect("an error toast is raised");
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(
            toast.text.contains("open page"),
            "must say what is missing: {}",
            toast.text
        );
    }

    #[test]
    fn following_rns_link_still_navigates() {
        let mut m = loaded_model((80, 24));
        let effects = follow_link(&mut m, 1);
        let expected = browser::resolve_link(link_of(&m, 1), m.current_dest)
            .unwrap()
            .0;
        assert_eq!(effects, vec![Effect::Navigate(expected)]);
    }

    #[test]
    fn following_file_link_downloads_instead_of_navigating() {
        let mut m = loaded_model((80, 24));
        m.links = vec![RenderedLink {
            index: 1,
            label: "Manual".to_string(),
            target: ":/file/docs/manual.pdf".to_string(),
            ..RenderedLink::default()
        }];
        let history_before = m.history.clone();
        let effects = follow_link(&mut m, 1);
        let expected = Target {
            dest_hash: [0xab; 16],
            path: "/file/docs/manual.pdf".to_string(),
            fields: Vec::new(),
            is_file: true,
        };
        assert_eq!(effects, vec![Effect::Download(expected)]);
        assert!(
            m.pending.is_none(),
            "a download must not start a page fetch"
        );
        assert_eq!(
            m.history, history_before,
            "a download must not change history"
        );
        assert_eq!(m.download.as_deref(), Some("manual.pdf"));
        assert!(m.is_loading(), "a download shows the loading indicator");
    }

    #[test]
    fn download_done_clears_the_flight_and_raises_info_toast() {
        let mut m = loaded_model((80, 24));
        m.download = Some("manual.pdf".to_string());
        let effects = update(
            &mut m,
            AppEvent::DownloadDone {
                name: "manual.pdf".to_string(),
                bytes: 1234,
                dir: "/home/x/Downloads".to_string(),
            },
        );
        assert!(effects.is_empty());
        assert!(m.download.is_none(), "the flight must be cleared");
        let toast = m.toast.as_ref().expect("a completion toast is raised");
        assert_eq!(toast.kind, ToastKind::Info);
        assert_eq!(
            toast.text,
            "downloaded manual.pdf (1234 bytes) to /home/x/Downloads"
        );
    }

    #[test]
    fn download_failed_raises_error_toast() {
        let mut m = loaded_model((80, 24));
        m.download = Some("manual.pdf".to_string());
        let effects = update(
            &mut m,
            AppEvent::DownloadFailed("request timed out".to_string()),
        );
        assert!(effects.is_empty());
        assert!(m.download.is_none(), "the flight must be cleared");
        let toast = m.toast.as_ref().expect("a failure toast is raised");
        assert_eq!(toast.kind, ToastKind::Error);
        assert_eq!(toast.text, "request timed out");
    }

    #[test]
    fn esc_cancels_an_inflight_download() {
        let mut m = loaded_model((80, 24));
        m.download = Some("manual.pdf".to_string());
        let effects = press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(effects, vec![Effect::Cancel]);
        assert!(m.download.is_none(), "Esc must clear the download flight");
    }

    #[test]
    fn navigation_supersedes_an_inflight_download() {
        let mut m = loaded_model((80, 24));
        m.download = Some("manual.pdf".to_string());
        let effects = begin_navigation(&mut m, tgt(2), HistoryAction::Push, None);
        assert_eq!(effects, vec![Effect::Navigate(tgt(2))]);
        assert!(
            m.download.is_none(),
            "a fresh navigation supersedes the download flight"
        );
    }

    #[test]
    fn focus_auto_scrolls_to_off_screen_link() {
        // A link on content line 50 with a 21-row viewport must scroll into view.
        let mut m = Model {
            page: vec![RLine::default(); 60],
            links: vec![RenderedLink {
                index: 1,
                label: "Deep".to_string(),
                target: ":/page/deep.mu".to_string(),
                line: 50,
                col_start: 0,
                col_end: 4,
                ..RenderedLink::default()
            }],
            size: (80, 24),
            ..Model::default()
        };
        assert_eq!(m.viewport(), 22);
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.focus, Some(1));
        // scroll = line + 1 - viewport = 50 + 1 - 22 = 29.
        assert_eq!(m.scroll, 29);
    }

    #[test]
    fn mouse_click_on_link_navigates() {
        let mut m = loaded_model((80, 24));
        let vl = visible_links(&m)
            .into_iter()
            .find(|v| v.index == 1)
            .expect("alpha visible");
        let effects = update(&mut m, AppEvent::Mouse(click(vl.col_start, vl.row)));
        let expected = browser::resolve_link(link_of(&m, 1), m.current_dest)
            .unwrap()
            .0;
        assert_eq!(effects, vec![Effect::Navigate(expected)]);
    }

    #[test]
    fn mouse_click_off_a_link_does_nothing() {
        let mut m = loaded_model((80, 24));
        // Row well below any link, in the content body.
        let effects = update(&mut m, AppEvent::Mouse(click(0, 20)));
        assert!(effects.is_empty());
        assert!(m.pending.is_none());
    }

    #[test]
    fn mouse_click_on_address_enters_address_mode() {
        let mut m = loaded_model((80, 24));
        let cr = top_bar_controls(&m, regions(m.frame_area())[0])
            .into_iter()
            .find(|c| c.control == Control::Address)
            .expect("address control");
        let effects = update(&mut m, AppEvent::Mouse(click(cr.rect.x, cr.rect.y)));
        assert!(effects.is_empty());
        assert_eq!(m.mode, Mode::Address);
    }

    #[test]
    fn mouse_click_on_star_toggles_bookmark() {
        let mut m = loaded_model((80, 24));
        let url = current_url(&m).expect("a page is loaded");
        assert!(!m.bookmarks.contains(&url), "not bookmarked to start");
        let cr = top_bar_controls(&m, regions(m.frame_area())[0])
            .into_iter()
            .find(|c| c.control == Control::Bookmark)
            .expect("bookmark star");
        let effects = update(&mut m, AppEvent::Mouse(click(cr.rect.x, cr.rect.y)));
        assert_eq!(effects, vec![Effect::SaveBookmarks]);
        assert!(m.bookmarks.contains(&url), "star click added the bookmark");
        // A second click removes it again.
        let cr = top_bar_controls(&m, regions(m.frame_area())[0])
            .into_iter()
            .find(|c| c.control == Control::Bookmark)
            .expect("bookmark star");
        update(&mut m, AppEvent::Mouse(click(cr.rect.x, cr.rect.y)));
        assert!(
            !m.bookmarks.contains(&url),
            "star click removed the bookmark"
        );
    }

    #[test]
    fn star_glyph_reflects_bookmark_state() {
        let mut m = loaded_model((80, 24));
        assert_eq!(star_glyph(&m), Some(STAR_OFF));
        toggle_bookmark_current(&mut m);
        assert_eq!(star_glyph(&m), Some(STAR_ON));
    }

    #[test]
    fn hops_cluster_reads_registry_or_unknown() {
        let mut m = loaded_model((80, 24));
        // Nothing announced for this dest yet: unknown.
        assert_eq!(hops_text(&m).as_deref(), Some("unknown"));
        // Once the dest is a known announced node with a hop count, N hops.
        let mut node = disc_node(0xab, "Node-AB");
        node.hops = Some(3);
        m.node_registry.upsert_node(&node);
        assert_eq!(hops_text(&m).as_deref(), Some("3 hops"));
        let top = row_text(&render(&m, 80, 24), 0, 80);
        assert!(top.contains("3 hops"), "hop count not rendered: {top:?}");
    }

    #[test]
    fn narrow_topbar_ellipsizes_address_without_overflow() {
        let m = loaded_model((30, 24));
        let buffer = render(&m, 30, 24);
        let top = row_text(&buffer, 0, 30);
        // The row is at most the terminal width, never more.
        assert!(UnicodeWidthStr::width(top.trim_end()) <= 30);
        // The address had to ellipsize at this width.
        assert!(
            top.contains('…'),
            "narrow address should ellipsize: {top:?}"
        );
    }

    #[test]
    fn rendered_buffer_has_no_marker_or_legend() {
        // A link-bearing page with no "Links:" content of its own.
        let m = Model::from_document(
            &parse("Intro paragraph\n\n`[Alpha`:/page/alpha.mu]"),
            content_width(80),
            "T",
            (80, 24),
        );
        let buffer = render(&m, 80, 24);
        let text = flat(&buffer);
        assert!(text.contains("Alpha"), "link label missing:\n{text}");
        assert!(!text.contains("[1]"), "visible [N] marker leaked:\n{text}");
        assert!(!text.contains("Links:"), "legend leaked:\n{text}");
    }

    #[test]
    fn hint_badges_render_over_links() {
        let mut m = loaded_model((80, 24));
        let vl = visible_links(&m)
            .into_iter()
            .find(|v| v.index == 1)
            .expect("alpha visible");
        press(&mut m, KeyCode::Char('f'), KeyModifiers::NONE);
        let buffer = render(&m, 80, 24);
        // The first link wears the 'a' badge over its first cell.
        let cell = &buffer[(vl.col_start, vl.row)];
        assert_eq!(cell.symbol(), "a", "hint badge not drawn over the link");
        assert_eq!(cell.bg, Color::Rgb(255, 215, 0), "badge background missing");
    }

    #[test]
    fn mouse_move_over_link_sets_and_clears_hover() {
        let mut m = loaded_model((80, 24));
        let vl = visible_links(&m)
            .into_iter()
            .find(|v| v.index == 1)
            .expect("alpha visible");
        update(&mut m, AppEvent::Mouse(moved(vl.col_start, vl.row)));
        assert_eq!(m.hover, Some(1), "moving over a link sets the hover");
        // A move off any link clears it.
        update(&mut m, AppEvent::Mouse(moved(vl.col_start, vl.row + 3)));
        assert_eq!(m.hover, None, "moving off a link clears the hover");
    }

    #[test]
    fn hovered_link_renders_highlighted_and_others_do_not() {
        let mut m = loaded_model((80, 24));
        let hovered = visible_links(&m)
            .into_iter()
            .find(|v| v.index == 1)
            .expect("alpha visible");
        let other = visible_links(&m)
            .into_iter()
            .find(|v| v.index == 2)
            .expect("beta visible");
        update(
            &mut m,
            AppEvent::Mouse(moved(hovered.col_start, hovered.row)),
        );
        let buffer = render(&m, 80, 24);
        let cell = &buffer[(hovered.col_start, hovered.row)];
        assert!(
            cell.modifier.contains(Modifier::BOLD) && cell.modifier.contains(Modifier::UNDERLINED),
            "hovered link not highlighted: {:?}",
            cell.modifier
        );
        // The non-hovered link keeps its plain link styling (no hover bold).
        let plain = &buffer[(other.col_start, other.row)];
        assert!(
            !plain.modifier.contains(Modifier::BOLD),
            "non-hovered link wrongly bolded: {:?}",
            plain.modifier
        );
    }

    #[test]
    fn focused_link_renders_highlighted() {
        let mut m = loaded_model((80, 24));
        let vl = visible_links(&m)
            .into_iter()
            .find(|v| v.index == 1)
            .expect("alpha visible");
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        let buffer = render(&m, 80, 24);
        let cell = &buffer[(vl.col_start, vl.row)];
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "focused link not highlighted"
        );
    }

    #[test]
    fn focused_link_shows_chip_and_keeps_footer_buttons() {
        let mut m = loaded_model((80, 24));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        let buffer = render(&m, 80, 24);
        // The chip renders the focused link's address on the content's last row
        // (row 22), above the footer (row 23).
        let chip = row_text(&buffer, 22, 80);
        assert!(
            chip.contains(&link_of(&m, 1).target),
            "chip missing the focused target on the content's last row: {chip:?}"
        );
        assert!(
            chip.contains('→'),
            "chip missing its arrow marker: {chip:?}"
        );
        // The regression this batch prevents: the footer still draws its buttons.
        let status = row_text(&buffer, 23, 80);
        assert!(
            status.contains("back") && status.contains("quit"),
            "footer buttons covered by the focused link: {status:?}"
        );
    }

    #[test]
    fn hovered_link_shows_chip_and_keeps_footer_buttons() {
        let mut m = loaded_model((80, 24));
        let vl = visible_links(&m)[0];
        update_hover(&mut m, vl.col_start, vl.row);
        let buffer = render(&m, 80, 24);
        let chip = row_text(&buffer, 22, 80);
        assert!(
            chip.contains(&link_of(&m, vl.index).target),
            "chip missing the hovered target: {chip:?}"
        );
        let status = row_text(&buffer, 23, 80);
        assert!(
            status.contains("back") && status.contains("quit"),
            "footer buttons covered by the hovered link: {status:?}"
        );
    }

    #[test]
    fn loading_chip_shows_spinner_and_path_and_keeps_footer_buttons() {
        let mut m = loaded_model((80, 24));
        m.pending = Some(Pending {
            target: tgt(7),
            action: HistoryAction::Push,
        });
        let buffer = render(&m, 80, 24);
        let chip = row_text(&buffer, 22, 80);
        assert!(
            chip.contains("loading") && chip.contains("/page/7.mu"),
            "loading chip missing spinner label / path: {chip:?}"
        );
        let spins: Vec<&str> = SPIN_FRAMES.to_vec();
        assert!(
            spins.iter().any(|g| chip.contains(g)),
            "loading chip missing the spinner glyph: {chip:?}"
        );
        let status = row_text(&buffer, 23, 80);
        assert!(
            status.contains("back") && status.contains("quit"),
            "footer buttons covered while loading: {status:?}"
        );
    }

    #[test]
    fn idle_draws_no_chip_and_leaves_the_content_row_untouched() {
        let m = loaded_model((80, 24));
        // No loading, no focus, no hover.
        assert!(m.pending.is_none() && m.focus.is_none() && m.hover.is_none());
        let buffer = render(&m, 80, 24);
        let chip = row_text(&buffer, 22, 80);
        assert!(!chip.contains('→'), "idle chip drew an arrow: {chip:?}");
        assert!(
            !chip.contains("loading"),
            "idle chip drew a loading label: {chip:?}"
        );
    }

    #[test]
    fn loading_wins_over_a_hovered_link() {
        let mut m = loaded_model((80, 24));
        let vl = visible_links(&m)[0];
        update_hover(&mut m, vl.col_start, vl.row);
        m.pending = Some(Pending {
            target: tgt(7),
            action: HistoryAction::Push,
        });
        let buffer = render(&m, 80, 24);
        let chip = row_text(&buffer, 22, 80);
        assert!(
            chip.contains("loading") && chip.contains("/page/7.mu"),
            "loading did not win over the hovered link: {chip:?}"
        );
        assert!(
            !chip.contains('→'),
            "the hovered link's arrow leaked while loading: {chip:?}"
        );
    }

    #[test]
    fn a_long_address_is_truncated_and_the_chip_never_exceeds_content_width() {
        // A narrow terminal forces truncation of a focused link's long target.
        let cols = 20u16;
        let mut m = loaded_model((cols, 24));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        let target = focused_link_target(&m).expect("focused target");
        assert!(
            UnicodeWidthStr::width(target.as_str()) + 2 > cols as usize,
            "test needs a target wider than the terminal"
        );
        let buffer = render(&m, cols, 24);
        let chip = row_text(&buffer, 22, cols);
        // The whole row fits the terminal, so the field never spilled over.
        assert_eq!(chip.chars().count(), cols as usize);
        // The arrow and the node prefix survive; the truncated tail is gone.
        assert!(
            chip.contains('→'),
            "chip missing its arrow marker: {chip:?}"
        );
        assert!(
            !chip.contains("alpha.mu"),
            "long address was not truncated: {chip:?}"
        );
        assert!(chip.contains('…'), "no ellipsis after truncation: {chip:?}");
    }

    // --- phase 5: in-page search + #anchor scroll-to --------------------

    /// One laid-out line from plain text, every cell in the default style.
    fn rline(text: &str) -> RLine {
        RLine {
            cells: text
                .chars()
                .map(|ch| crate::render::StyledChar {
                    ch,
                    st: RStyle::default(),
                    link: None,
                    field: None,
                })
                .collect(),
        }
    }

    #[test]
    fn find_matches_multiple_across_lines_and_case_insensitive() {
        let page = vec![rline("Foo bar foo"), rline("nothing here"), rline("FOObar")];
        let ms = find_matches(&page, "foo");
        assert_eq!(
            ms,
            vec![
                Match {
                    line_idx: 0,
                    col_start: 0,
                    col_end: 3
                },
                Match {
                    line_idx: 0,
                    col_start: 8,
                    col_end: 11
                },
                Match {
                    line_idx: 2,
                    col_start: 0,
                    col_end: 3
                },
            ]
        );
    }

    #[test]
    fn find_matches_empty_query_and_no_match_are_empty() {
        let page = vec![rline("abcdef")];
        assert!(
            find_matches(&page, "").is_empty(),
            "empty query -> no matches"
        );
        assert!(
            find_matches(&page, "xyz").is_empty(),
            "no hit -> no matches"
        );
        // A query longer than the line never matches.
        assert!(find_matches(&page, "abcdefg").is_empty());
    }

    #[test]
    fn find_matches_adjacent_hits_are_non_overlapping() {
        // "aa" over "aaaa" yields two adjacent, non-overlapping matches (not the
        // three an overlapping scan would report).
        let ms = find_matches(&[rline("aaaa")], "aa");
        assert_eq!(
            ms,
            vec![
                Match {
                    line_idx: 0,
                    col_start: 0,
                    col_end: 2
                },
                Match {
                    line_idx: 0,
                    col_start: 2,
                    col_end: 4
                },
            ]
        );
    }

    #[test]
    fn find_matches_positions_are_display_columns_after_wide_chars() {
        // Two width-2 emojis (4 display columns) precede "term", which starts at
        // char index 2 but display column 4; col_end is 4 + width("term") = 8.
        let ms = find_matches(&[rline("😀😀term")], "term");
        assert_eq!(
            ms,
            vec![Match {
                line_idx: 0,
                col_start: 4,
                col_end: 8,
            }]
        );
    }

    #[test]
    fn search_highlight_lands_on_the_term_after_wide_chars() {
        // "😀😀 " is 5 display columns; the term occupies the next six.
        let mut m = Model::from_document(&parse("😀😀 needle"), content_width(80), "S", (80, 24));
        commit_search(&mut m, "needle");
        let mch = m.matches.first().copied().expect("one match");
        assert_eq!((mch.col_start, mch.col_end), (5, 11));

        let buffer = render(&m, 80, 24);
        let row = TOPBAR_ROWS + (mch.line_idx - m.scroll) as u16;
        // The highlighted cells spell the term, not a wide-char-shifted slice.
        let mut got = String::new();
        for x in mch.col_start as u16..mch.col_end as u16 {
            got.push_str(buffer[(x, row)].symbol());
        }
        assert_eq!(got, "needle");
        assert!(
            buffer[(mch.col_start as u16, row)]
                .modifier
                .contains(Modifier::REVERSED),
            "the current match should be highlighted",
        );
    }

    #[test]
    fn link_after_wide_chars_hit_tests_at_its_display_column() {
        let doc = "😀😀 `[Alpha`:/page/alpha.mu]";
        let mut m = Model::from_document(&parse(doc), content_width(80), "S", (80, 24));
        // A current destination so the relative link resolves on activation.
        m.current_dest = Some([0x11; 16]);
        let vl = visible_links(&m)
            .into_iter()
            .find(|v| v.index == 1)
            .expect("alpha visible");
        // The label starts at display column 5 (after 😀😀 + space), not the char
        // index 3 the old cell-index positioning reported.
        assert_eq!(vl.col_start, 5);

        // A click on the label's display column follows it ...
        let effects = update(&mut m, AppEvent::Mouse(click(vl.col_start, vl.row)));
        assert!(matches!(effects.as_slice(), [Effect::Navigate(_)]));

        // ... while the old char-index column (3) sits inside the second emoji and
        // hits nothing.
        let mut m2 = Model::from_document(&parse(doc), content_width(80), "S", (80, 24));
        m2.current_dest = Some([0x11; 16]);
        let effects = update(&mut m2, AppEvent::Mouse(click(3, vl.row)));
        assert!(
            effects.is_empty(),
            "char-index column must not hit the link"
        );
    }

    /// A controlled document with a known anchor deep in a tall page: 30 intro
    /// paragraphs, then `` `:deep `` before a heading, then 30 trailing ones, so
    /// the anchor sits well below the first viewport and above the page bottom.
    fn long_anchor_doc() -> leviculum_micron::MicronDocument {
        let mut s = String::new();
        for i in 0..30 {
            s.push_str(&format!("intro{i}\n\n"));
        }
        s.push_str("`:deep\n>Deep Heading\n\n");
        for i in 0..30 {
            s.push_str(&format!("tail{i}\n\n"));
        }
        parse(&s)
    }

    #[test]
    fn anchor_line_resolves_known_and_rejects_unknown() {
        // `:target binds to its own (blank) block; the heading slug binds to the
        // heading block. Both resolve to a page line; an unknown anchor does not.
        let m = Model::from_document(
            &parse("Intro para\n`:target\n>Marked Section\nbody text"),
            content_width(80),
            "T",
            (80, 24),
        );
        let target_line = anchor_line(&m, "target").expect("known anchor resolves");
        let heading_line = anchor_line(&m, "marked-section").expect("heading slug resolves");
        assert!(
            line_text(&m.page[heading_line]).contains("Marked Section"),
            "heading anchor must land on the heading line: {:?}",
            line_text(&m.page[heading_line])
        );
        assert!(
            target_line < heading_line,
            "the anchor precedes the heading"
        );
        assert_eq!(anchor_line(&m, "nope"), None);
    }

    #[test]
    fn navigation_with_anchor_scrolls_to_anchor_line() {
        let mut m = model_from_sample(content_width(80), (80, 10));
        m.pending = Some(Pending {
            target: tgt(7),
            action: HistoryAction::Push,
        });
        m.pending_anchor = Some("deep".to_string());
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: long_anchor_doc(),
                title: "Deep".to_string(),
            },
        );
        let vp = m.viewport();
        let line = anchor_line(&m, "deep").expect("anchor resolves on the loaded page");
        assert!(
            line > vp,
            "anchor must sit below the first viewport to test a jump"
        );
        assert_eq!(m.scroll, line.min(m.max_scroll(vp)));
        assert!(m.scroll > 0, "scroll must have moved to the anchor");
        assert!(m.pending_anchor.is_none(), "anchor consumed on load");
    }

    #[test]
    fn navigation_with_unknown_anchor_stays_top_and_notes() {
        let mut m = model_from_sample(content_width(80), (80, 10));
        m.pending = Some(Pending {
            target: tgt(7),
            action: HistoryAction::Push,
        });
        m.pending_anchor = Some("missing".to_string());
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: long_anchor_doc(),
                title: "Deep".to_string(),
            },
        );
        assert_eq!(m.scroll, 0, "unknown anchor falls back to the top");
        let toast = m.toast.as_ref().expect("unknown anchor raises a toast");
        assert!(
            toast.text.contains("not found"),
            "unknown anchor should note it: {}",
            toast.text
        );
    }

    #[test]
    fn split_path_anchor_strips_the_fragment() {
        let (base, anchor) = split_path_anchor(Target {
            dest_hash: [1; 16],
            path: "/page/x.mu#sec".to_string(),
            fields: Vec::new(),
            is_file: false,
        });
        assert_eq!(base.path, "/page/x.mu");
        assert_eq!(anchor.as_deref(), Some("sec"));
        // No fragment -> no anchor, path untouched.
        let (base2, anchor2) = split_path_anchor(Target {
            dest_hash: [1; 16],
            path: "/page/y.mu".to_string(),
            fields: Vec::new(),
            is_file: false,
        });
        assert_eq!(base2.path, "/page/y.mu");
        assert!(anchor2.is_none());
    }

    /// A tall page whose only "needle" sits below the first viewport, so a search
    /// commit must scroll to reveal it.
    fn deep_match_model(size: (u16, u16)) -> Model {
        let mut s = String::new();
        for i in 0..40 {
            s.push_str(&format!("filler{i}\n\n"));
        }
        s.push_str("needle deep here\n");
        Model::from_document(&parse(&s), content_width(size.0), "S", size)
    }

    /// A short page with three "needle" occurrences on distinct lines, all inside
    /// a full-height viewport, for cycling tests.
    fn triple_match_model() -> Model {
        let s = "needle one\n\nfiller\n\nneedle two\n\nneedle three";
        Model::from_document(&parse(s), content_width(80), "S", (80, 24))
    }

    #[test]
    fn slash_enters_search_and_enter_commits_matches_and_scrolls() {
        let mut m = deep_match_model((80, 10));
        let eff = press(&mut m, KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(eff.is_empty());
        assert_eq!(m.mode, Mode::Search);
        type_str(&mut m, "needle");
        let eff = press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        assert!(eff.is_empty());
        assert_eq!(m.mode, Mode::Browse, "Enter commits and leaves search");
        assert!(!m.matches.is_empty(), "commit populates matches");
        assert_eq!(m.current_match, Some(0));
        // The (single, deep) match is scrolled into the viewport.
        let first = m.matches[0];
        let vp = m.viewport();
        assert!(
            first.line_idx >= vp,
            "match must start below the first viewport"
        );
        assert!(
            first.line_idx >= m.scroll && first.line_idx < m.scroll + vp,
            "current match must be visible after commit"
        );
    }

    #[test]
    fn n_and_shift_n_cycle_matches_and_wrap() {
        let mut m = triple_match_model();
        press(&mut m, KeyCode::Char('/'), KeyModifiers::NONE);
        type_str(&mut m, "needle");
        press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(m.matches.len(), 3, "three needle lines");
        assert_eq!(m.current_match, Some(0));
        press(&mut m, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(m.current_match, Some(1));
        press(&mut m, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(m.current_match, Some(2));
        // n wraps past the end back to the first.
        press(&mut m, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(m.current_match, Some(0));
        // N wraps before the start to the last.
        press(&mut m, KeyCode::Char('N'), KeyModifiers::NONE);
        assert_eq!(m.current_match, Some(2));
    }

    #[test]
    fn n_with_no_matches_is_a_noop() {
        let mut m = triple_match_model();
        assert!(m.matches.is_empty());
        press(&mut m, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(m.current_match, None);
        assert_eq!(m.scroll, 0);
    }

    #[test]
    fn reentering_search_and_esc_clears_highlights() {
        let mut m = triple_match_model();
        press(&mut m, KeyCode::Char('/'), KeyModifiers::NONE);
        type_str(&mut m, "needle");
        press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!m.matches.is_empty(), "committed matches exist");
        // Re-entering search drops the prior highlights immediately.
        press(&mut m, KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(
            m.matches.is_empty(),
            "entering search clears old highlights"
        );
        // Esc returns to browse with nothing highlighted.
        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(m.mode, Mode::Browse);
        assert!(m.matches.is_empty());
        assert!(m.current_match.is_none());
    }

    #[test]
    fn commit_with_no_match_notes_it() {
        let mut m = triple_match_model();
        press(&mut m, KeyCode::Char('/'), KeyModifiers::NONE);
        type_str(&mut m, "zzz");
        press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        assert!(m.matches.is_empty());
        assert_eq!(m.current_match, None);
        let toast = m.toast.as_ref().expect("a fruitless search raises a toast");
        assert!(
            toast.text.contains("no matches"),
            "a fruitless search should say so: {}",
            toast.text
        );
    }

    #[test]
    fn search_highlights_matches_and_current_is_stronger() {
        let mut m = triple_match_model();
        press(&mut m, KeyCode::Char('/'), KeyModifiers::NONE);
        type_str(&mut m, "needle");
        press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        let buffer = render(&m, 80, 24);
        // The current (first) match is drawn reversed.
        let cur = m.matches[0];
        let cur_row = TOPBAR_ROWS + cur.line_idx as u16;
        let cur_cell = &buffer[(cur.col_start as u16, cur_row)];
        assert!(
            cur_cell.modifier.contains(Modifier::REVERSED),
            "current match must use the stronger reversed highlight"
        );
        // A non-current match carries the theme's match tint, not the reverse.
        let other = m.matches[1];
        let other_row = TOPBAR_ROWS + other.line_idx as u16;
        let other_cell = &buffer[(other.col_start as u16, other_row)];
        assert_eq!(
            other_cell.bg,
            rgb(Theme::Dark.search_match_bg()),
            "non-current match must carry the tint background"
        );
        assert!(
            !other_cell.modifier.contains(Modifier::REVERSED),
            "non-current match must not use the stronger highlight"
        );
    }

    #[test]
    fn search_commit_scrolls_current_match_into_the_rendered_viewport() {
        let mut m = deep_match_model((80, 10));
        press(&mut m, KeyCode::Char('/'), KeyModifiers::NONE);
        type_str(&mut m, "needle");
        press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        let mline = m.matches[0].line_idx;
        let vp = m.viewport();
        assert!(
            mline >= m.scroll && mline < m.scroll + vp,
            "match in viewport"
        );
        let buffer = render(&m, 80, 10);
        let row = TOPBAR_ROWS + (mline - m.scroll) as u16;
        let cell = &buffer[(m.matches[0].col_start as u16, row)];
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "the current match must render (reversed) inside the viewport"
        );
    }

    #[test]
    fn anchor_jump_renders_the_anchor_line_at_the_content_top() {
        let mut m = model_from_sample(content_width(80), (80, 10));
        m.pending = Some(Pending {
            target: tgt(7),
            action: HistoryAction::Push,
        });
        m.pending_anchor = Some("deep".to_string());
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: long_anchor_doc(),
                title: "Deep".to_string(),
            },
        );
        let buffer = render(&m, 80, 10);
        let top = row_text(&buffer, TOPBAR_ROWS, 80 - SCROLLBAR_COLS);
        assert_eq!(
            top.trim_end(),
            line_text(&m.page[m.scroll]).trim_end(),
            "the anchor's line must sit at the top of the content viewport"
        );
    }

    #[test]
    fn search_bar_renders_query_line() {
        let mut m = triple_match_model();
        press(&mut m, KeyCode::Char('/'), KeyModifiers::NONE);
        type_str(&mut m, "abc");
        let buffer = render(&m, 80, 24);
        let status = row_text(&buffer, 23, 80);
        assert!(status.contains('/'), "search prompt missing: {status:?}");
        assert!(status.contains("abc"), "query text missing: {status:?}");
    }

    // --- terminal guard (retained from phase 2) --------------------------

    struct MockOps {
        entered: Rc<Cell<bool>>,
        restored: Rc<Cell<bool>>,
    }

    impl TerminalOps for MockOps {
        fn enter(&mut self) -> io::Result<()> {
            self.entered.set(true);
            Ok(())
        }
        fn restore(&mut self) -> io::Result<()> {
            self.restored.set(true);
            Ok(())
        }
    }

    #[test]
    fn guard_restores_on_drop() {
        let entered = Rc::new(Cell::new(false));
        let restored = Rc::new(Cell::new(false));
        {
            let guard = TerminalGuard::new(MockOps {
                entered: entered.clone(),
                restored: restored.clone(),
            })
            .expect("enter");
            assert!(entered.get(), "enter should have run");
            assert!(!restored.get(), "restore must not run before drop");
            drop(guard);
        }
        assert!(restored.get(), "restore must run on drop");
    }

    #[test]
    fn guard_restore_now_is_idempotent() {
        let restored = Rc::new(Cell::new(false));
        let mut guard = TerminalGuard::new(MockOps {
            entered: Rc::new(Cell::new(false)),
            restored: restored.clone(),
        })
        .expect("enter");
        guard.restore_now().expect("restore");
        assert!(restored.get());
        restored.set(false);
        guard.restore_now().expect("restore idempotent");
        assert!(!restored.get(), "restore ran twice");
    }

    // --- phase 6: bookmarks, copy, places panel -------------------------

    fn disc_node(hash: u8, name: &str) -> DiscoveredNode {
        DiscoveredNode {
            dest_hash: [hash; 16],
            name: Some(name.to_string()),
            first_seen: 0,
            last_seen: 0,
            hops: Some(1),
        }
    }

    #[test]
    fn osc52_wraps_base64_payload() {
        // ESC ] 52 ; c ; base64("hi") ST, with ST = ESC backslash.
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x1b\\");
    }

    /// The set of footer actions a browse footer keeps at width `w`.
    fn browse_actions(w: u16) -> Vec<FooterAction> {
        let m = loaded_model((80, 24));
        footer_layout(&m, Rect::new(0, 23, w, 1))
            .into_iter()
            .map(|s| s.item.action)
            .collect()
    }

    #[test]
    fn footer_shows_every_button_at_a_wide_width() {
        // Given enough room, the whole browse footer renders with labels and no
        // collapse: nav trio + 8 browse actions = 11 buttons, in order.
        let m = loaded_model((120, 24));
        let slots = footer_layout(&m, Rect::new(0, 23, 120, 1));
        assert_eq!(slots.len(), 11, "all browse buttons should fit wide");
        assert!(slots.iter().all(|s| !s.collapsed), "no collapse when wide");
        assert!(matches!(slots[0].item.action, FooterAction::Back));
        assert!(matches!(slots[1].item.action, FooterAction::Forward));
        assert!(matches!(slots[2].item.action, FooterAction::Reload));
        assert!(matches!(slots[3].item.action, FooterAction::Identify));
        // The stronger divider sits before the first non-nav button.
        assert!(slots[3].strong_divider, "nav/mode boundary divider missing");
        assert!(!slots[1].strong_divider, "nav items share the dim divider");
        let right = slots.last().map(|s| s.rect.right()).unwrap_or(0);
        assert!(right <= 120, "footer overflows: {right}");
    }

    #[test]
    fn footer_nav_trio_is_first_and_click_navigates_back() {
        let mut m = loaded_model((80, 24));
        // Give history a back step so back is available and does something.
        m.history.visit(tgt(0xcd));
        m.current_dest = Some([0xcd; 16]);
        assert!(m.history.can_back());
        let slot = footer_slot(&m, FooterAction::Back);
        let effects = update(&mut m, AppEvent::Mouse(click(slot.rect.x, slot.rect.y)));
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0], Effect::Navigate(_)));
    }

    #[test]
    fn footer_nav_trio_renders_keys_not_glyphs() {
        // The nav trio reads as ordinary key + label items like the rest of the
        // strip: `Alt-← back`, `Alt-→ forward`, `R reload`, no glyphs anywhere.
        let m = loaded_model((120, 24));
        let slots = footer_layout(&m, Rect::new(0, 23, 120, 1));
        let trio: Vec<(&str, &str)> = slots[..3]
            .iter()
            .map(|s| (s.item.key, s.item.label))
            .collect();
        assert_eq!(
            trio,
            vec![("Alt-←", "back"), ("Alt-→", "forward"), ("R", "reload")]
        );
        let buffer = render(&m, 120, 24);
        let row = row_text(&buffer, 23, 120);
        for g in ['‹', '›', '⟳'] {
            assert!(!row.contains(g), "glyph {g:?} still in footer: {row:?}");
        }
        assert!(row.contains("Alt-← back"), "back item text: {row:?}");
        assert!(row.contains("Alt-→ forward"), "forward item text: {row:?}");
        assert!(row.contains("R reload"), "reload item text: {row:?}");
    }

    #[test]
    fn footer_reload_key_is_bright_bold_and_label_is_muted() {
        // The reload item wears exactly the same chrome as every other footer
        // item: KEY in the bright chrome fg + BOLD, LABEL in the muted chrome fg.
        let m = loaded_model((80, 24));
        let slot = footer_slot(&m, FooterAction::Reload);
        let buffer = render(&m, 80, 24);
        let key = &buffer[(slot.rect.x, 23u16)];
        assert_eq!(
            key.fg,
            rgb(m.theme.chrome_fg()),
            "reload key must be bright"
        );
        assert!(
            key.modifier.contains(Modifier::BOLD),
            "reload key must be BOLD"
        );
        // The label ("reload") starts two columns past the key ("R" + a space).
        let label = &buffer[(slot.rect.x + 2, 23u16)];
        assert_eq!(
            label.fg,
            rgb(m.theme.chrome_muted_fg()),
            "reload label must be muted"
        );
    }

    #[test]
    fn footer_click_on_reload_reloads_the_page() {
        let mut m = loaded_model((80, 24));
        let slot = footer_slot(&m, FooterAction::Reload);
        let effects = update(&mut m, AppEvent::Mouse(click(slot.rect.x, slot.rect.y)));
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0], Effect::Navigate(_)));
    }

    #[test]
    fn help_overlay_lists_all_bindings_and_affordances() {
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let text = flat(&render(&m, 120, 40));
        // The bindings the old hand-written list silently dropped.
        for s in [
            "Ctrl-n", "Ctrl-p", "Ctrl-v", "PageDown", "PageUp", "wheel", "Ctrl-R", "F5", "Alt-←",
            "Alt-→",
        ] {
            assert!(text.contains(s), "help missing binding {s:?}:\n{text}");
        }
        // The mouse affordances: the clickable URL, the star, the footer
        // buttons, and the side buttons.
        for s in [
            "click the URL",
            "click star",
            "footer button",
            "mouse side buttons",
        ] {
            assert!(text.contains(s), "help missing affordance {s:?}:\n{text}");
        }
    }

    #[test]
    fn help_overlay_has_no_heading_and_a_closing_border_title() {
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let text = flat(&render(&m, 120, 40));
        assert!(
            !text.contains("lnomad — keys"),
            "stale heading line present:\n{text}"
        );
        assert!(
            text.contains("click to close"),
            "border title must mention closing:\n{text}"
        );
    }

    #[test]
    fn help_mentions_back_forward_exactly_once() {
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let text = flat(&render(&m, 120, 40));
        let count = text
            .lines()
            .filter(|l| l.contains("back / forward"))
            .count();
        assert_eq!(count, 1, "back / forward must appear once:\n{text}");
    }

    /// The overlay scrollbar's thumb (`█`) appears in a column strictly left of
    /// the content's own scrollbar (the rightmost column), so this detects the
    /// overlay's scrollbar without picking up the always-present content one.
    fn overlay_scrollbar_shown(buffer: &ratatui::buffer::Buffer) -> bool {
        let w = buffer.area.width;
        for y in 0..buffer.area.height {
            for x in 0..w.saturating_sub(1) {
                if buffer[(x, y)].symbol() == "█" {
                    return true;
                }
            }
        }
        false
    }

    /// Whether the places panel's selected entry currently sits inside the
    /// overlay's viewport — the invariant the view-follow logic must keep.
    fn selected_visible(m: &Model) -> bool {
        let layout = places_layout(m);
        let vp = places_viewport(m, layout.total);
        let sel_line = layout.entry_line[m.places_sel];
        sel_line >= m.places_scroll && sel_line < m.places_scroll + vp
    }

    #[test]
    fn help_overlay_scrolls_when_taller_than_the_terminal() {
        // A terminal too short to show the whole help.
        let rows = 8u16;
        let mut m = loaded_model((80, rows));
        let total = help_lines().len();
        assert!(
            total + 2 > rows as usize,
            "the help must overflow an {rows}-row terminal for this test"
        );
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        assert_eq!(m.help_scroll, 0, "the help opens at the top");
        let page_before = m.scroll;
        let (viewport, max) = help_scroll_geometry(&m);
        assert!(max > 0, "there must be room to scroll");

        // Down motions: j, Ctrl-n, PageDown, Ctrl-f.
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(m.help_scroll, 1, "j scrolls down one");
        press(&mut m, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(m.help_scroll, 2, "Ctrl-n scrolls down one");
        let before = m.help_scroll;
        press(&mut m, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(
            m.help_scroll,
            (before + viewport).min(max),
            "PageDown moves by a viewport"
        );
        m.help_scroll = 0;
        press(&mut m, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(m.help_scroll, viewport.min(max), "Ctrl-f pages down");

        // G / End reach the last line; g / Home return to the top.
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(m.help_scroll, max, "G reaches the bottom");
        press(&mut m, KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(m.help_scroll, 0, "g returns to the top");
        press(&mut m, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(m.help_scroll, max, "End also reaches the bottom");
        press(&mut m, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(m.help_scroll, 0, "Home returns to the top");

        // The wheel scrolls the overlay, both ways, clamped.
        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(
            m.help_scroll,
            WHEEL_STEP.min(max),
            "wheel down scrolls the help"
        );
        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollUp)));
        assert_eq!(m.help_scroll, 0, "wheel up scrolls back to the top");

        // The offset clamps at both ends.
        press(&mut m, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(m.help_scroll, 0, "clamps at the top");
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(m.help_scroll, max, "clamps at the bottom");

        // Scrolling the help never moves the page underneath.
        assert_eq!(m.scroll, page_before, "the page offset is untouched");
    }

    #[test]
    fn help_scroll_reveals_the_last_line_and_draws_a_scrollbar() {
        // Wide but short: the overlay is narrower than the terminal, so its
        // right-border scrollbar sits strictly left of the content scrollbar.
        let (w, rows) = (120u16, 8u16);
        let mut m = loaded_model((w, rows));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        // The last help entry's keys are unique to the help body.
        let marker = "q / Ctrl-c";
        let top = flat(&render(&m, w, rows));
        assert!(
            !top.contains(marker),
            "the last line must be hidden at the top:\n{top}"
        );
        assert!(
            overlay_scrollbar_shown(&render(&m, w, rows)),
            "the scrollbar must render while the help overflows"
        );
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        let bottom = flat(&render(&m, w, rows));
        assert!(
            bottom.contains(marker),
            "G must reveal the last help line:\n{bottom}"
        );
    }

    #[test]
    fn help_that_fits_shows_no_scrollbar_and_ignores_scroll_keys() {
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let (_, max) = help_scroll_geometry(&m);
        assert_eq!(max, 0, "the help fits, so there is nothing to scroll");
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(
            m.help_scroll, 0,
            "scroll keys are pinned when the help fits"
        );
        assert!(
            !overlay_scrollbar_shown(&render(&m, 120, 40)),
            "no scrollbar when the help fits the terminal"
        );
    }

    #[test]
    fn click_closes_help_without_falling_through() {
        let mut m = loaded_model((120, 40));
        // A footer button that WOULD act on a click were the help closed.
        let slot = footer_slot(&m, FooterAction::Reload);
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(m.show_help);
        let effects = update(&mut m, AppEvent::Mouse(click(slot.rect.x, slot.rect.y)));
        assert!(!m.show_help, "a click must close the help");
        assert!(
            effects.is_empty(),
            "the click must not fall through to the footer button: {effects:?}"
        );
        // Esc and ? still close it.
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(m.show_help);
        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!m.show_help, "Esc must still close the help");
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(m.show_help);
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(!m.show_help, "? must still close the help");
    }

    #[test]
    fn help_titles_dim_keys_bold_bright_desc_muted() {
        // The first cell of `needle`'s run in the rendered buffer.
        fn find_cell(buffer: &ratatui::buffer::Buffer, needle: &str) -> (u16, u16) {
            let chars: Vec<char> = needle.chars().collect();
            let area = buffer.area();
            for y in 0..area.height {
                for x in 0..area.width {
                    if (0..chars.len() as u16).all(|i| {
                        let cx = x + i;
                        cx < area.width && buffer[(cx, y)].symbol().starts_with(chars[i as usize])
                    }) {
                        return (x, y);
                    }
                }
            }
            panic!("cell for {needle:?} not found");
        }

        for theme in [Theme::Dark, Theme::Light] {
            let mut m = loaded_model((120, 40));
            if theme == Theme::Light {
                m.toggle_theme();
            }
            press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
            let buffer = render(&m, 120, 40);

            // A group title renders dim, in the muted chrome fg.
            let title = &buffer[find_cell(&buffer, "Move")];
            assert!(
                title.modifier.contains(Modifier::DIM),
                "group title must be dim ({theme:?})"
            );
            assert_eq!(
                title.fg,
                rgb(theme.chrome_muted_fg()),
                "group title must use the muted chrome fg ({theme:?})"
            );
            // A key cell is BOLD in the bright chrome fg, never dim.
            let key = &buffer[find_cell(&buffer, "wheel")];
            assert!(
                key.modifier.contains(Modifier::BOLD),
                "key cell must be bold ({theme:?})"
            );
            assert!(
                !key.modifier.contains(Modifier::DIM),
                "key cell must not be dim ({theme:?})"
            );
            assert_eq!(
                key.fg,
                rgb(theme.chrome_fg()),
                "key cell must use the bright chrome fg ({theme:?})"
            );
            // A description cell is muted, not bold.
            let desc = &buffer[find_cell(&buffer, "scroll")];
            assert_eq!(
                desc.fg,
                rgb(theme.chrome_muted_fg()),
                "description must use the muted chrome fg ({theme:?})"
            );
            assert!(
                !desc.modifier.contains(Modifier::BOLD),
                "description must not be bold ({theme:?})"
            );
            assert_ne!(
                key.fg, desc.fg,
                "key and description must differ ({theme:?})"
            );
        }
    }

    #[test]
    fn footer_unavailable_nav_reads_muted_not_dim() {
        // loaded_model has a single history entry: back is unavailable and must
        // render in the readable muted chrome fg, never DIM.
        for theme in [Theme::Dark, Theme::Light] {
            let mut m = loaded_model((80, 24));
            if theme == Theme::Light {
                m.toggle_theme();
            }
            assert!(!m.history.can_back(), "back must be unavailable here");
            let slot = footer_slot(&m, FooterAction::Back);
            let buffer = render(&m, 80, 24);
            let cell = &buffer[(slot.rect.x, 23u16)];
            assert_eq!(
                cell.fg,
                rgb(theme.chrome_muted_fg()),
                "unavailable nav must use the muted chrome fg ({theme:?})"
            );
            assert!(
                !cell.modifier.contains(Modifier::DIM),
                "unavailable nav must not be DIM ({theme:?})"
            );
        }
    }

    #[test]
    fn footer_click_on_places_opens_the_panel() {
        // Wide enough that `d places` survives the priority truncation (the
        // spelled-out nav keys make the full strip a touch wider than before).
        let mut m = loaded_model((120, 24));
        assert!(!m.show_places);
        let slot = footer_slot(&m, FooterAction::Places);
        let effects = update(&mut m, AppEvent::Mouse(click(slot.rect.x, slot.rect.y)));
        assert!(effects.is_empty());
        assert!(m.show_places, "clicking `d places` opens the places panel");
    }

    #[test]
    fn footer_priority_truncation_drops_copy_first() {
        // At a width too tight for every label, `y copy` drops first while the nav
        // trio, `? help` and `q quit` always remain, and labels stay (no collapse).
        let acts = browse_actions(90);
        assert!(
            !acts.contains(&FooterAction::Copy),
            "y copy should drop first: {acts:?}"
        );
        for keep in [
            FooterAction::Back,
            FooterAction::Forward,
            FooterAction::Reload,
            FooterAction::Help,
            FooterAction::Quit,
        ] {
            assert!(acts.contains(&keep), "{keep:?} must survive: {acts:?}");
        }
        let m = loaded_model((80, 24));
        assert!(
            footer_layout(&m, Rect::new(0, 23, 90, 1))
                .iter()
                .all(|s| !s.collapsed),
            "labels should stay at a merely-narrow width"
        );
    }

    #[test]
    fn footer_collapses_labels_to_keys_when_very_narrow() {
        let m = loaded_model((30, 24));
        let slots = footer_layout(&m, Rect::new(0, 23, 30, 1));
        assert!(
            slots.iter().all(|s| s.collapsed),
            "a very narrow footer must collapse to keys"
        );
        // Rendered: the keys survive, the words do not.
        let buffer = render(&m, 30, 24);
        let row = row_text(&buffer, 23, 30);
        assert!(row.contains("Alt-←"), "back key missing: {row:?}");
        assert!(row.contains('q'), "quit key missing: {row:?}");
        assert!(!row.contains("quit"), "labels should be gone: {row:?}");
        assert!(!row.contains("back"), "labels should be gone: {row:?}");
    }

    #[test]
    fn footer_field_mode_shows_field_actions() {
        let mut m = loaded_model((80, 24));
        // Tab past the two links onto the sample page's text field.
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.mode, Mode::Field);
        // At a comfortable width the full field-mode set is shown.
        let labels: Vec<&str> = footer_layout(&m, Rect::new(0, 23, 100, 1))
            .into_iter()
            .map(|s| s.item.label)
            .collect();
        assert!(labels.contains(&"done"), "Esc done missing: {labels:?}");
        assert!(
            labels.contains(&"next field"),
            "Tab next field missing: {labels:?}"
        );
        assert!(
            labels.contains(&"toggle"),
            "Space toggle missing: {labels:?}"
        );
    }

    #[test]
    fn footer_hover_highlights_then_clears() {
        let mut m = loaded_model((80, 24));
        let slot = footer_slot(&m, FooterAction::Quit);
        update(&mut m, AppEvent::Mouse(moved(slot.rect.x, slot.rect.y)));
        assert_eq!(m.footer_hover, Some(slot.index), "move sets footer hover");
        let buffer = render(&m, 80, 24);
        let cell = &buffer[(slot.rect.x, 23u16)];
        assert!(
            cell.modifier.contains(Modifier::UNDERLINED),
            "hovered footer button not highlighted: {:?}",
            cell.modifier
        );
        // A move off the footer clears it.
        update(&mut m, AppEvent::Mouse(moved(slot.rect.x, 10)));
        assert_eq!(m.footer_hover, None, "moving off clears footer hover");
    }

    #[test]
    fn bookmark_toggle_adds_then_removes_and_persists() {
        let mut m = loaded_model((80, 24));
        let url = current_url(&m).expect("current url");
        // First `m` bookmarks the page and asks the shell to persist.
        let fx = press(&mut m, KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(m.bookmarks.contains(&url));
        assert_eq!(fx, vec![Effect::SaveBookmarks]);
        // Second `m` un-bookmarks it, again persisting.
        let fx = press(&mut m, KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(!m.bookmarks.contains(&url));
        assert_eq!(fx, vec![Effect::SaveBookmarks]);
    }

    #[test]
    fn yank_copies_current_page_url() {
        let mut m = loaded_model((80, 24));
        let url = current_url(&m).expect("current url");
        let fx = press(&mut m, KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(fx, vec![Effect::Copy(url)]);
        let toast = m.toast.as_ref().expect("a copy toast is raised");
        assert_eq!(toast.kind, ToastKind::Info);
        assert!(toast.text.contains("copied"), "toast: {}", toast.text);
    }

    // --- identify: the `i` toggle, top-bar marker, footer button, help ----

    /// A loaded model with a fingerprint set, as the IO shell would build it.
    fn identify_model(size: (u16, u16)) -> Model {
        let mut m = loaded_model(size);
        m.fingerprint = "aabbccddeeff00112233445566778899".to_string();
        m
    }

    #[test]
    fn i_toggles_identify_emits_setidentify_and_reload() {
        let mut m = identify_model((80, 24));
        assert!(!m.identifying, "anonymous by default");

        let fx = press(&mut m, KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(
            fx.first(),
            Some(&Effect::SetIdentify {
                dest: [0xab; 16],
                on: true
            }),
            "the decision is persisted before the reload: {fx:?}"
        );
        assert!(
            matches!(fx.get(1), Some(Effect::Navigate(t)) if t.dest_hash == [0xab; 16]),
            "the toggle reloads the current page: {fx:?}"
        );
        assert!(m.identifying, "the flag flips optimistically");
        let toast = m.toast.as_ref().expect("a toggle toast is raised");
        assert_eq!(toast.kind, ToastKind::Info);
        assert!(
            toast.text.contains("Identifying") && toast.text.contains("aabbccdd"),
            "toast names the fingerprint: {}",
            toast.text
        );

        let fx = press(&mut m, KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(
            fx.first(),
            Some(&Effect::SetIdentify {
                dest: [0xab; 16],
                on: false
            }),
            "the second toggle turns it off: {fx:?}"
        );
        assert!(
            matches!(fx.get(1), Some(Effect::Navigate(_))),
            "the second toggle reloads too: {fx:?}"
        );
        assert!(!m.identifying, "back to anonymous");
        let toast = m.toast.as_ref().expect("a toggle toast is raised");
        assert!(
            toast.text.contains("anonymous"),
            "toast notes anonymity: {}",
            toast.text
        );
    }

    #[test]
    fn i_without_current_dest_is_noop() {
        let mut m = model_from_sample(80, (80, 24));
        assert!(m.current_dest.is_none());
        let fx = press(&mut m, KeyCode::Char('i'), KeyModifiers::NONE);
        assert!(fx.is_empty(), "no page, no effects: {fx:?}");
        assert!(!m.identifying);
    }

    #[test]
    fn topbar_shows_key_marker_only_when_identifying() {
        let mut m = identify_model((80, 24));
        let anon = row_text(&render(&m, 80, 24), 0, 80);
        assert!(
            !anon.contains(IDENTIFY_GLYPH),
            "anonymous shows no marker: {anon:?}"
        );
        m.identifying = true;
        let ident = row_text(&render(&m, 80, 24), 0, 80);
        assert!(
            ident.contains(IDENTIFY_GLYPH),
            "key marker missing: {ident:?}"
        );
        assert!(
            ident.contains("aabbccdd"),
            "fingerprint prefix missing: {ident:?}"
        );
        assert!(
            !ident.contains("eeff0011"),
            "only the first 8 hex chars show: {ident:?}"
        );
    }

    #[test]
    fn topbar_identity_marker_survives_hops_and_bolt_dropping() {
        // At a width too narrow for the whole cluster the hop count drops
        // first, then the cache bolt; the identity marker (and the star)
        // survive. current_dest is set, so hops would read "unknown".
        let mut m = identify_model((14, 24));
        m.identifying = true;
        m.cached_view = true;
        let top = row_text(&render(&m, 14, 24), 0, 14);
        assert!(
            top.contains(IDENTIFY_GLYPH) && top.contains("aabbccdd"),
            "identity must survive truncation: {top:?}"
        );
        assert!(!top.contains("unknown"), "hops drop first: {top:?}");
        assert!(!top.contains(CACHED_GLYPH), "the bolt drops next: {top:?}");
    }

    #[test]
    fn footer_identify_label_flips() {
        let mut m = loaded_model((80, 24));
        let label_of = |m: &Model| {
            footer_items(m)
                .into_iter()
                .find(|i| i.action == FooterAction::Identify)
                .map(|i| i.label)
        };
        assert_eq!(label_of(&m), Some("identify"), "anonymous state");
        m.identifying = true;
        assert_eq!(label_of(&m), Some("anonymous"), "identifying state");
        m.current_dest = None;
        assert_eq!(label_of(&m), None, "no page, no identify button");
    }

    #[test]
    fn click_footer_identify_toggles() {
        let mut m = identify_model((80, 24));
        let slot = footer_slot(&m, FooterAction::Identify);
        let fx = update(&mut m, AppEvent::Mouse(click(slot.rect.x, slot.rect.y)));
        assert_eq!(
            fx.first(),
            Some(&Effect::SetIdentify {
                dest: [0xab; 16],
                on: true
            }),
            "the click persists like the key: {fx:?}"
        );
        assert!(
            matches!(fx.get(1), Some(Effect::Navigate(_))),
            "the click reloads like the key: {fx:?}"
        );
        assert!(m.identifying, "the click flips the flag like the key");
    }

    #[test]
    fn help_lists_identify_binding() {
        let mut m = loaded_model((120, 40));
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let text = flat(&render(&m, 120, 40));
        assert!(
            text.contains("identify / go anonymous to this node"),
            "help missing the i binding:\n{text}"
        );
        assert!(
            text.contains("default anonymous"),
            "help missing the anonymity-model line:\n{text}"
        );
    }

    #[test]
    fn cached_revisit_restores_identify_state_per_node() {
        let mut m = tall_model((80, 24));
        // Fresh-load node 1 and learn (as the IO shell does after a load) that
        // the session identifies to it.
        begin_navigation(&mut m, tgt(1), HistoryAction::Push, None);
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse("one"),
                title: "One".to_string(),
            },
        );
        m.note_identify([1; 16], true);
        assert!(m.identifying);
        // Fresh-load node 2, anonymous.
        begin_navigation(&mut m, tgt(2), HistoryAction::Push, None);
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse("two"),
                title: "Two".to_string(),
            },
        );
        m.note_identify([2; 16], false);
        assert!(!m.identifying, "node 2 is anonymous");
        // Back to node 1 is a cache hit: no fetch, yet the flag is restored.
        let fx = go_back(&mut m);
        assert!(fx.is_empty(), "cache hit fetches nothing: {fx:?}");
        assert!(m.identifying, "cached revisit restores the per-node state");
        // Forward to node 2, also cached: anonymous again.
        go_forward(&mut m);
        assert!(!m.identifying, "cached forward restores anonymity");
    }

    #[test]
    /// Started without a URL, the browser shows its start screen with the places
    /// panel already in front of it: nothing is fetched, nothing enters the
    /// history, and no destination is current, so the top bar carries no address
    /// and back/forward/reload stay unavailable.
    fn start_screen_opens_the_places_panel_over_a_local_page() {
        let mut m = Model {
            size: (80, 24),
            ..Model::default()
        };
        m.relayout(content_width(80));
        show_start_screen(&mut m);

        assert!(m.show_places, "the places panel must open by itself");
        assert_eq!(m.title, START_TITLE);
        assert!(!m.page.is_empty(), "the start page must be laid out");
        assert!(m.history.current().is_none(), "no history entry");
        assert!(!m.history.can_back() && !m.history.can_forward());
        assert_eq!(m.current_dest, None, "no node is current");
        assert_eq!(current_url(&m), None, "no address to show");
        assert!(!m.is_loading(), "the start screen must not fetch anything");
        assert!(m.page_cache.get(&tgt(0xab)).is_none(), "nothing cached");
    }

    #[test]
    /// The start screen names the keys that lead somewhere, and closing the
    /// panel leaves it in view rather than an empty frame.
    fn start_screen_survives_closing_the_panel() {
        let mut m = Model {
            size: (80, 24),
            ..Model::default()
        };
        m.relayout(content_width(80));
        show_start_screen(&mut m);

        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!m.show_places, "Esc closes the panel");

        let text = flat(&render(&m, 80, 24));
        assert!(text.contains("lnomad"), "start screen gone: {text}");
        assert!(text.contains("places"), "no hint at the panel: {text}");
        assert!(text.contains("URL"), "no hint at the URL entry: {text}");
    }

    #[test]
    /// An empty discovered-nodes section says announces are still awaited, not
    /// that there are none: on a first run the panel opens by itself with both
    /// sections empty, and "(none)" would read as a broken feature.
    fn empty_places_panel_says_it_is_still_listening() {
        let mut m = Model {
            size: (80, 24),
            ..Model::default()
        };
        m.relayout(content_width(80));
        show_start_screen(&mut m);

        let text = flat(&render(&m, 80, 24));
        assert!(
            text.contains("listening for announces"),
            "no listening note: {text}"
        );
    }

    #[test]
    fn places_lists_bookmarks_then_discovered_nodes() {
        let mut m = loaded_model((80, 24));
        m.bookmarks.add(Bookmark {
            url: "aa11:/page/index.mu".to_string(),
            title: "Home".to_string(),
        });
        m.node_registry.upsert_node(&disc_node(0xcd, "Node-C"));
        let ps = places(&m);
        assert_eq!(ps.len(), 2, "one bookmark then one node");
        assert!(matches!(ps[0], Place::Bookmark { .. }));
        assert!(matches!(ps[1], Place::Node { .. }));
    }

    #[test]
    fn places_panel_opens_navigates_and_activates() {
        let mut m = loaded_model((80, 24));
        m.bookmarks.add(Bookmark {
            url: "aa11:/page/index.mu".to_string(),
            title: "Home".to_string(),
        });
        m.node_registry.upsert_node(&disc_node(0xcd, "Node-C"));

        // `d` opens the panel; it then owns keys.
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(m.show_places);
        assert_eq!(m.places_sel, 0);

        // `j` moves onto the discovered node; Enter opens its default page.
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, 1);
        let fx = press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!m.show_places, "activating closes the panel");
        assert_eq!(
            fx,
            vec![Effect::Navigate(Target {
                dest_hash: [0xcd; 16],
                path: DEFAULT_PATH.to_string(),
                fields: Vec::new(),
                is_file: false,
            })]
        );
    }

    #[test]
    fn places_panel_shares_the_content_scroll_keymap() {
        let mut m = loaded_model((80, 24));
        for i in 0..5u8 {
            m.bookmarks.add(Bookmark {
                url: format!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa{i:02}:/page/index.mu"),
                title: format!("B{i}"),
            });
        }
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(m.show_places);
        assert_eq!(m.places_sel, 0);

        // Line down: j, Ctrl-n, Down all step one entry.
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, 1);
        press(&mut m, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(m.places_sel, 2);
        press(&mut m, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(m.places_sel, 3);

        // Line up: k, Ctrl-p, Up step back one entry.
        press(&mut m, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, 2);
        press(&mut m, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(m.places_sel, 1);
        press(&mut m, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(m.places_sel, 0);

        // g / G / Home / End jump to the first / last entry.
        let last = places(&m).len() - 1;
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, last);
        press(&mut m, KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, 0);
        press(&mut m, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(m.places_sel, last);
        press(&mut m, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(m.places_sel, 0);

        // Ctrl-d is a half-page motion here, not the `d` that closes the panel.
        press(&mut m, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(m.show_places, "Ctrl-d must not close the panel");
        assert_eq!(
            m.places_sel, last,
            "half-page jump clamps to the last entry"
        );

        // Plain `d` still closes the panel.
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(!m.show_places);
    }

    /// Fill a model with `n` discovered nodes so the panel overflows a short
    /// terminal.
    fn with_nodes(size: (u16, u16), n: u8) -> Model {
        let mut m = loaded_model(size);
        for i in 0..n {
            update(
                &mut m,
                AppEvent::NodeDiscovered(disc_node(0x40 + i, &format!("Node-{i}"))),
            );
        }
        m
    }

    #[test]
    fn places_panel_scrolls_to_follow_the_selection() {
        let mut m = with_nodes((80, 8), 20);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(m.show_places);
        assert_eq!(m.places_sel, 0);
        assert_eq!(m.places_scroll, 0, "the panel opens at the top");

        let layout = places_layout(&m);
        let vp = places_viewport(&m, layout.total);
        assert!(layout.total > vp, "the list must overflow this terminal");

        // Stepping down one entry at a time keeps the selected row visible, and
        // once the selection reaches the viewport bottom the view scrolls by one
        // (all entries are nodes here, so a step is exactly one line).
        let len = places(&m).len();
        let mut prev = m.places_scroll;
        let mut ever_scrolled = false;
        for _ in 0..len - 1 {
            press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
            assert!(selected_visible(&m), "the selected row must stay visible");
            if m.places_scroll != prev {
                assert_eq!(m.places_scroll, prev + 1, "the view scrolls by one line");
                ever_scrolled = true;
            }
            prev = m.places_scroll;
        }
        assert!(
            ever_scrolled,
            "stepping past the bottom must scroll the view"
        );

        // G selects the last node AND shows it; g returns to the top.
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, len - 1, "G selects the last node");
        assert!(selected_visible(&m), "the last node's row is visible");
        press(&mut m, KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, 0, "g selects the first entry");
        // Minimum-needed scrolling: the first entry is brought into view without
        // scrolling any further than required to reveal it.
        assert!(selected_visible(&m), "the first entry is visible after g");
    }

    #[test]
    fn places_selection_stays_visible_across_the_section_boundary() {
        let mut m = with_nodes((80, 8), 10);
        for i in 0..4u8 {
            m.bookmarks.add(Bookmark {
                url: format!("aa{i:02}:/page/index.mu"),
                title: format!("B{i}"),
            });
        }
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let len = places(&m).len();
        // Walk the whole list; the selected row is visible at every step, even
        // as it crosses the blank spacer and the "Discovered nodes" title.
        for _ in 0..len + 2 {
            press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
            assert!(
                selected_visible(&m),
                "selection off-screen at sel={}",
                m.places_sel
            );
        }
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        assert!(selected_visible(&m), "G keeps the last row visible");
    }

    #[test]
    fn places_page_motion_moves_by_the_visible_entry_count() {
        let mut m = with_nodes((80, 8), 20);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let vp = places_viewport(&m, places_layout(&m).total);
        let max = places(&m).len() - 1;
        press(&mut m, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(
            m.places_sel,
            vp.min(max),
            "PageDown moves the selection by the viewport entry count, not a constant"
        );
        assert!(selected_visible(&m));
    }

    #[test]
    fn places_wheel_scrolls_the_view_and_never_moves_the_selection() {
        let mut m = with_nodes((80, 8), 20);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let layout = places_layout(&m);
        let vp = places_viewport(&m, layout.total);
        let max = layout.total.saturating_sub(vp);
        assert!(max > WHEEL_STEP, "the list must overflow enough to wheel");

        // A notch down moves the VIEW by WHEEL_STEP lines and leaves the
        // selection where it was.
        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(m.places_scroll, WHEEL_STEP, "wheel down scrolls the view");
        assert_eq!(m.places_sel, 0, "wheel down leaves the selection alone");

        // A notch up scrolls the view back and again leaves the selection alone.
        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollUp)));
        assert_eq!(m.places_scroll, 0, "wheel up scrolls the view back");
        assert_eq!(m.places_sel, 0, "wheel up leaves the selection alone");
    }

    #[test]
    fn places_wheel_clamps_at_both_ends() {
        let mut m = with_nodes((80, 8), 20);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let layout = places_layout(&m);
        let vp = places_viewport(&m, layout.total);
        let max = layout.total.saturating_sub(vp);

        // Wheeling up at the top never underflows.
        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollUp)));
        assert_eq!(m.places_scroll, 0, "the view never scrolls below 0");

        // Wheeling down past the bottom clamps to max, never beyond.
        for _ in 0..layout.total {
            update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        }
        assert_eq!(m.places_scroll, max, "the view never scrolls past max");
        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(m.places_scroll, max, "an extra notch stays clamped");
        assert_eq!(m.places_sel, 0, "wheeling never moved the selection");
    }

    #[test]
    fn places_wheel_leaves_every_entry_reachable() {
        // The regression: one notch used to jump the selection by WHEEL_STEP and
        // skip entries. Now a notch does not move the selection at all, so the
        // very next `j` steps by exactly one and no entry is skipped.
        let mut m = with_nodes((80, 8), 20);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let sel = m.places_sel;
        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(
            m.places_sel,
            sel + 1,
            "after a wheel notch, j moves the selection by one, not by WHEEL_STEP"
        );
    }

    #[test]
    fn places_key_after_wheeling_off_screen_snaps_the_view_back() {
        let mut m = with_nodes((80, 8), 20);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let sel_before = m.places_sel;

        // Wheel the view down until the selected row is off-screen.
        for _ in 0..layout_max_notches(&m) {
            update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        }
        assert!(!selected_visible(&m), "the selection is now off-screen");
        assert_eq!(m.places_sel, sel_before, "wheeling did not move it");

        // One `j` moves the selection by exactly one and the view snaps back so
        // the selected row is inside the viewport again.
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, sel_before + 1, "j steps by exactly one");
        assert!(
            selected_visible(&m),
            "the view scrolled back to the selection"
        );
    }

    #[test]
    fn places_wheel_is_a_no_op_when_the_panel_fits() {
        // A tall terminal shows the whole (short) list, so there is nothing to
        // scroll and the wheel changes neither the view nor the selection.
        let mut m = with_nodes((80, 40), 3);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let layout = places_layout(&m);
        let vp = places_viewport(&m, layout.total);
        assert!(layout.total <= vp, "the whole list must fit for this test");

        update(&mut m, AppEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(m.places_scroll, 0, "a fitting panel ignores the wheel");
        assert_eq!(m.places_sel, 0, "and does not move the selection");
    }

    /// How many wheel notches it takes to reach the bottom of the places view.
    fn layout_max_notches(m: &Model) -> usize {
        let layout = places_layout(m);
        let vp = places_viewport(m, layout.total);
        let max = layout.total.saturating_sub(vp);
        max.div_ceil(WHEEL_STEP)
    }

    /// The screen row the `idx`-th places entry is drawn on, through the same
    /// overlay geometry and clamped scroll offset the renderer and hit-test use.
    fn place_row(m: &Model, idx: usize) -> u16 {
        let overlay = places_overlay_rect(m, m.frame_area());
        let layout = places_layout(m);
        let vp = places_viewport(m, layout.total);
        let max = layout.total.saturating_sub(vp);
        let offset = m.places_scroll.min(max);
        overlay.y + 1 + (layout.entry_line[idx] - offset) as u16
    }

    /// A column safely inside the panel's inner region.
    fn place_col(m: &Model) -> u16 {
        places_overlay_rect(m, m.frame_area()).x + 2
    }

    /// A model with one bookmark then one discovered node, panel open. The
    /// bookmark carries a full 32-hex destination hash so activating it parses to
    /// a real navigation target.
    fn places_model() -> Model {
        let mut m = loaded_model((80, 24));
        m.bookmarks.add(Bookmark {
            url: format!("{}:/page/index.mu", "bb".repeat(16)),
            title: "Home".to_string(),
        });
        m.node_registry.upsert_node(&disc_node(0xcd, "Node-C"));
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(m.show_places);
        m
    }

    /// A model with three titled bookmarks (indices 0..=2) then one discovered
    /// node (index 3), panel open on the first row.
    fn bookmarks_model() -> Model {
        let mut m = loaded_model((80, 24));
        for (i, title) in ["Alpha", "Beta", "Gamma"].iter().enumerate() {
            m.bookmarks.add(Bookmark {
                url: format!("{}:/page/{i}.mu", "aa".repeat(16)),
                title: title.to_string(),
            });
        }
        m.node_registry.upsert_node(&disc_node(0xcd, "Node-C"));
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(m.show_places);
        assert_eq!(m.places_sel, 0);
        m
    }

    /// The titles of `m`'s bookmarks, in order.
    fn bookmark_titles(m: &Model) -> Vec<String> {
        m.bookmarks.list().iter().map(|b| b.title.clone()).collect()
    }

    #[test]
    fn places_click_on_a_bookmark_row_activates_it() {
        let mut m = places_model();
        // A single left click on the bookmark row does exactly what Enter on that
        // selection does, and selects it first.
        let mut via_key = m.clone();
        let expected = press(&mut via_key, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!expected.is_empty(), "a bookmark row navigates");

        let (col, row) = (place_col(&m), place_row(&m, 0));
        let fx = update(&mut m, AppEvent::Mouse(click(col, row)));
        assert_eq!(m.places_sel, 0, "the click selects the bookmark row");
        assert_eq!(fx, expected, "the click activates exactly like Enter");
        assert!(!m.show_places, "activating closes the panel");
    }

    #[test]
    fn places_click_on_a_discovered_node_row_activates_it() {
        let mut m = places_model();
        let node_idx = 1;
        let mut via_key = m.clone();
        via_key.places_sel = node_idx;
        let expected = press(&mut via_key, KeyCode::Enter, KeyModifiers::NONE);

        let (col, row) = (place_col(&m), place_row(&m, node_idx));
        let fx = update(&mut m, AppEvent::Mouse(click(col, row)));
        assert_eq!(m.places_sel, node_idx, "the click selects the node row");
        assert_eq!(
            fx,
            vec![Effect::Navigate(Target {
                dest_hash: [0xcd; 16],
                path: DEFAULT_PATH.to_string(),
                fields: Vec::new(),
                is_file: false,
            })],
            "the click opens the node's default page"
        );
        assert_eq!(fx, expected, "the click activates exactly like Enter");
        assert!(!m.show_places, "activating closes the panel");
    }

    #[test]
    fn places_click_honours_the_scroll_offset() {
        // A tall list in a short terminal, scrolled to the bottom so the first
        // visible row is well down the list. A click on that top row must select
        // the entry actually drawn there, not the entry the row would hold at
        // scroll 0.
        let mut m = with_nodes((80, 8), 20);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        assert!(m.places_scroll > 0, "the list must be scrolled");

        let layout = places_layout(&m);
        let vp = places_viewport(&m, layout.total);
        let offset = m.places_scroll.min(layout.total.saturating_sub(vp));
        let top_idx = layout
            .entry_line
            .iter()
            .position(|&l| l == offset)
            .expect("an entry sits on the first visible line");
        assert!(top_idx > 0, "the scrolled-in entry is not the first entry");

        let Place::Node { dest_hash, .. } = places(&m)[top_idx].clone() else {
            panic!("the scrolled-in entry is a discovered node");
        };
        let overlay = places_overlay_rect(&m, m.frame_area());
        let fx = update(&mut m, AppEvent::Mouse(click(overlay.x + 2, overlay.y + 1)));
        assert_eq!(
            m.places_sel, top_idx,
            "the hit-test maps the top screen row through the scroll offset"
        );
        assert_eq!(
            fx,
            vec![Effect::Navigate(Target {
                dest_hash,
                path: DEFAULT_PATH.to_string(),
                fields: Vec::new(),
                is_file: false,
            })],
            "it activates the entry actually drawn on that row"
        );
    }

    #[test]
    fn places_click_on_a_title_or_blank_does_nothing() {
        let mut m = places_model();
        let overlay = places_overlay_rect(&m, m.frame_area());
        let sel_before = m.places_sel;
        // Row 0 inside the panel is the "Bookmarks" section title, never an entry.
        let fx = update(&mut m, AppEvent::Mouse(click(overlay.x + 2, overlay.y + 1)));
        assert!(fx.is_empty(), "a click on a section title does nothing");
        assert!(m.show_places, "the panel stays open");
        assert_eq!(m.places_sel, sel_before, "the selection is unchanged");
        assert!(place_at(&m, overlay.x + 2, overlay.y + 1).is_none());

        // The blank spacer line between the two sections is likewise inert.
        let blank = places_layout(&m).entry_line[0] + 1; // line after the sole bookmark
        let fx = update(
            &mut m,
            AppEvent::Mouse(click(overlay.x + 2, overlay.y + 1 + blank as u16)),
        );
        assert!(fx.is_empty(), "a click on a blank line does nothing");
        assert!(m.show_places);
    }

    #[test]
    fn places_click_outside_the_panel_closes_it() {
        let mut m = places_model();
        let overlay = places_overlay_rect(&m, m.frame_area());
        // A point above and left of the panel is outside it (and would otherwise
        // reach the top bar underneath).
        assert!(overlay.y > 0, "there is room above the panel");
        let fx = update(&mut m, AppEvent::Mouse(click(0, 0)));
        assert!(fx.is_empty(), "closing the panel follows no link");
        assert!(!m.show_places, "a click outside closes the panel");
    }

    #[test]
    fn places_hover_highlights_an_entry_and_clears_off_it() {
        let mut m = places_model();
        let (col, node_row) = (place_col(&m), place_row(&m, 1));
        update(&mut m, AppEvent::Mouse(moved(col, node_row)));
        assert_eq!(m.places_hover, Some(1), "moving over an entry hovers it");

        // The hovered (non-selected) row is bold + underlined, distinct from the
        // selected row's reverse video.
        let buffer = render(&m, 80, 24);
        let hovered = &buffer[(col, node_row)];
        assert!(
            hovered.modifier.contains(Modifier::UNDERLINED)
                && hovered.modifier.contains(Modifier::BOLD),
            "the hovered row is bold + underlined: {:?}",
            hovered.modifier
        );
        assert!(
            !hovered.modifier.contains(Modifier::REVERSED),
            "the hover highlight is distinct from the selection's reverse video"
        );
        let sel_row = place_row(&m, 0);
        let selected = &buffer[(col, sel_row)];
        assert!(
            selected.modifier.contains(Modifier::REVERSED),
            "the selected row keeps its reverse-video highlight"
        );

        // Moving onto the section title (not an entry) clears the hover.
        let overlay = places_overlay_rect(&m, m.frame_area());
        update(&mut m, AppEvent::Mouse(moved(overlay.x + 2, overlay.y + 1)));
        assert_eq!(m.places_hover, None, "moving off an entry clears the hover");
    }

    #[test]
    fn places_scrollbar_shows_only_when_the_list_overflows() {
        // Wide + short: the panel overflows, so the scrollbar renders.
        let mut over = with_nodes((120, 8), 20);
        press(&mut over, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            overlay_scrollbar_shown(&render(&over, 120, 8)),
            "a scrollbar renders while the list overflows"
        );
        // Tall: the whole list fits, so there is no scrollbar.
        let mut fits = with_nodes((120, 40), 3);
        press(&mut fits, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            !overlay_scrollbar_shown(&render(&fits, 120, 40)),
            "no scrollbar when the list fits the terminal"
        );
    }

    #[test]
    fn overlays_never_draw_outside_their_rect_on_a_tiny_terminal() {
        // A 4x3 terminal is far smaller than either overlay's natural size;
        // rendering must clamp to the rect and never panic.
        let mut m = with_nodes((4, 3), 10);
        press(&mut m, KeyCode::Char('?'), KeyModifiers::NONE);
        let _ = render(&m, 4, 3);
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        let _ = render(&m, 4, 3);
        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let _ = render(&m, 4, 3);
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        let _ = render(&m, 4, 3);
    }

    #[test]
    fn places_panel_deletes_selected_bookmark_and_persists() {
        let mut m = loaded_model((80, 24));
        m.bookmarks.add(Bookmark {
            url: "aa11:/page/index.mu".to_string(),
            title: "Home".to_string(),
        });
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        // `x` removes the selected bookmark and asks to persist.
        let fx = press(&mut m, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(m.bookmarks.is_empty());
        assert_eq!(fx, vec![Effect::SaveBookmarks]);
    }

    #[test]
    fn x_deletes_selected_bookmark_with_title_toast() {
        let mut m = bookmarks_model();
        let fx = press(&mut m, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(fx, vec![Effect::SaveBookmarks]);
        assert_eq!(bookmark_titles(&m), ["Beta", "Gamma"]);
        // The toast names the TITLE (not the URL) and carries the undo hint.
        let toast = m.toast.clone().expect("a toast is shown");
        assert_eq!(toast.text, "removed Alpha — u to undo");

        // An untitled bookmark falls back to its URL in the toast.
        let mut m = loaded_model((80, 24));
        m.bookmarks.add(Bookmark {
            url: "aa11:/page/index.mu".to_string(),
            title: String::new(),
        });
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        press(&mut m, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            m.toast.clone().expect("a toast is shown").text,
            "removed aa11:/page/index.mu — u to undo"
        );
    }

    #[test]
    fn delete_key_deletes_like_x() {
        let mut via_x = bookmarks_model();
        let mut via_del = bookmarks_model();
        press(&mut via_x, KeyCode::Char('j'), KeyModifiers::NONE);
        press(&mut via_del, KeyCode::Char('j'), KeyModifiers::NONE);
        let fx_x = press(&mut via_x, KeyCode::Char('x'), KeyModifiers::NONE);
        let fx_del = press(&mut via_del, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(fx_del, fx_x, "Delete emits the same effects as x");
        assert_eq!(fx_del, vec![Effect::SaveBookmarks]);
        assert_eq!(bookmark_titles(&via_del), bookmark_titles(&via_x));
        assert_eq!(bookmark_titles(&via_del), ["Alpha", "Gamma"]);
        assert_eq!(via_del.toast, via_x.toast, "same toast either way");
    }

    #[test]
    fn u_restores_the_last_deleted_bookmark() {
        let mut m = bookmarks_model();
        // Delete the middle bookmark ...
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        press(&mut m, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(bookmark_titles(&m), ["Alpha", "Gamma"]);
        // ... and `u` puts it back exactly where it was.
        let fx = press(&mut m, KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(fx, vec![Effect::SaveBookmarks]);
        assert_eq!(
            bookmark_titles(&m),
            ["Alpha", "Beta", "Gamma"],
            "restored in place"
        );
        assert_eq!(m.places_sel, 1, "the restored row is selected");
        assert_eq!(
            m.toast.clone().expect("a toast is shown").text,
            "restored Beta"
        );

        // A second `u` has nothing left to undo: no save, list unchanged.
        let fx = press(&mut m, KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(fx.is_empty(), "an empty undo slot saves nothing");
        assert_eq!(bookmark_titles(&m), ["Alpha", "Beta", "Gamma"]);

        // A new deletion replaces the one slot: after deleting Alpha and then
        // Gamma, `u` restores only Gamma (the newer); Alpha stays gone.
        press(&mut m, KeyCode::Char('g'), KeyModifiers::NONE);
        press(&mut m, KeyCode::Char('x'), KeyModifiers::NONE);
        press(&mut m, KeyCode::Char('j'), KeyModifiers::NONE);
        press(&mut m, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(bookmark_titles(&m), ["Beta"]);
        press(&mut m, KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(
            bookmark_titles(&m),
            ["Beta", "Gamma"],
            "u restores the newer deletion only"
        );
    }

    #[test]
    fn x_marker_shows_only_on_selected_or_hovered_bookmark_row() {
        let mut m = bookmarks_model();
        let x_col = place_x_screen_col(&m).expect("the panel reserves an × cell");
        // Hover the third bookmark row while the first stays selected.
        let (col, row) = (place_col(&m), place_row(&m, 2));
        update(&mut m, AppEvent::Mouse(moved(col, row)));
        assert_eq!(m.places_hover, Some(2));
        let buffer = render(&m, 80, 24);
        let sym = |idx: usize| buffer[(x_col, place_row(&m, idx))].symbol().to_string();
        assert_eq!(sym(0), "×", "the selected bookmark row shows the marker");
        assert_eq!(sym(2), "×", "a hovered bookmark row shows the marker");
        assert_eq!(sym(1), " ", "an unmarked bookmark row shows none");
        assert_eq!(sym(3), " ", "a node row shows none");

        // Even a selected AND hovered node row shows no marker.
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        let (col, row) = (place_col(&m), place_row(&m, 3));
        update(&mut m, AppEvent::Mouse(moved(col, row)));
        assert_eq!((m.places_sel, m.places_hover), (3, Some(3)));
        let buffer = render(&m, 80, 24);
        assert_eq!(
            buffer[(x_col, place_row(&m, 3))].symbol(),
            " ",
            "a selected + hovered node row shows no ×"
        );
    }

    #[test]
    fn click_on_x_deletes_that_bookmark() {
        let mut m = bookmarks_model();
        let x_col = place_x_screen_col(&m).expect("the panel reserves an × cell");
        // Hover then click the × on the second bookmark row: THAT bookmark goes
        // (not the selected one), the panel stays open, nothing navigates.
        let row = place_row(&m, 1);
        update(&mut m, AppEvent::Mouse(moved(x_col, row)));
        assert_eq!(m.places_hover, Some(1));
        let fx = update(&mut m, AppEvent::Mouse(click(x_col, row)));
        assert_eq!(fx, vec![Effect::SaveBookmarks]);
        assert_eq!(bookmark_titles(&m), ["Alpha", "Gamma"]);
        assert!(m.show_places, "deleting keeps the panel open");

        // A click elsewhere on the row keeps the select + activate behaviour.
        let mut m = places_model();
        let (col, row) = (place_col(&m), place_row(&m, 0));
        let fx = update(&mut m, AppEvent::Mouse(click(col, row)));
        assert!(
            matches!(fx.as_slice(), [Effect::Navigate(_)]),
            "a non-× click on a bookmark row still activates it: {fx:?}"
        );
        assert!(!m.show_places, "activating closes the panel");
        assert_eq!(m.bookmarks.len(), 1, "nothing was deleted");
    }

    #[test]
    fn node_row_has_no_x_and_is_not_deletable() {
        let mut m = bookmarks_model();
        let node_idx = 3;
        // Select the node row; x and Delete are no-ops: nothing removed, no
        // save, no toast, no undo slot.
        press(&mut m, KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, node_idx);
        assert!(press(&mut m, KeyCode::Char('x'), KeyModifiers::NONE).is_empty());
        assert!(press(&mut m, KeyCode::Delete, KeyModifiers::NONE).is_empty());
        assert_eq!(bookmark_titles(&m), ["Alpha", "Beta", "Gamma"]);
        assert_eq!(places(&m).len(), 4, "the node is still listed");
        assert!(m.toast.is_none(), "a node-row delete shows no toast");
        assert!(m.last_deleted_bookmark.is_none(), "no undo slot is written");

        // A click on the node row's would-be × cell (selected AND hovered) is
        // inert: no delete, no activation, the panel stays open.
        let x_col = place_x_screen_col(&m).expect("the panel reserves an × cell");
        let row = place_row(&m, node_idx);
        update(&mut m, AppEvent::Mouse(moved(x_col, row)));
        assert_eq!(m.places_hover, Some(node_idx));
        let fx = update(&mut m, AppEvent::Mouse(click(x_col, row)));
        assert!(fx.is_empty(), "the × column of a node row does nothing");
        assert!(m.show_places, "the panel stays open");
        assert_eq!(places(&m).len(), 4, "nothing was deleted");
    }

    #[test]
    fn discovery_event_feeds_the_places_panel() {
        let mut m = loaded_model((80, 24));
        update(&mut m, AppEvent::NodeDiscovered(disc_node(0xcd, "Node-C")));
        let ps = places(&m);
        assert_eq!(ps.len(), 1);
        assert!(matches!(ps[0], Place::Node { .. }));
    }

    #[test]
    fn discovery_channel_node_reaches_the_model() {
        // The background discovery task forwards each node onto a channel whose
        // receiver is the loop's fourth select arm. Drive that channel directly
        // (no real RNS): a node sent on it is received and folded into the model,
        // and the places panel then lists it — exactly what the arm does.
        let mut m = loaded_model((80, 24));
        let (node_tx, mut node_rx) = mpsc::unbounded_channel::<DiscoveredNode>();
        node_tx.send(disc_node(0xab, "Node-A")).expect("send node");
        let node = node_rx.try_recv().expect("node delivered on channel");
        update(&mut m, AppEvent::NodeDiscovered(node));
        assert!(
            m.node_registry.get_by_hash(&[0xab; 16]).is_some(),
            "node folded into the model registry"
        );
        let ps = places(&m);
        assert_eq!(ps.len(), 1);
        assert!(matches!(ps[0], Place::Node { .. }));
    }

    #[test]
    fn fit_width_counts_vs16_presentation_as_wide() {
        assert_eq!(fit_width(""), 0);
        assert_eq!(fit_width("a"), 1);
        // Ordinary wide emoji: unicode-width already says 2.
        assert_eq!(fit_width("🌐"), 2);
        assert_eq!(fit_width("📰"), 2);
        // Emoji-presentation sequences (base + U+FE0F): unicode-width scores
        // these as 1, but a terminal paints them 2 wide.
        assert_eq!(fit_width("☯️"), 2);
        assert_eq!(fit_width("☸️"), 2);
        // The bare base char without VS16 keeps its text-default single column.
        assert_eq!(fit_width("☯"), 1);
    }

    #[test]
    fn truncate_to_cols_is_width_aware() {
        assert_eq!(truncate_to_cols("hello", 10), "hello");
        assert_eq!(truncate_to_cols("hello", 5), "hello");
        // Cut to a budget: the ellipsis takes the last column.
        assert_eq!(truncate_to_cols("hello world", 5), "hell…");
        assert_eq!(
            UnicodeWidthStr::width(truncate_to_cols("hello world", 5).as_str()),
            5
        );
        // A wide char that would straddle the budget is dropped whole, so the
        // result never exceeds the requested width.
        let cut = truncate_to_cols("🚀ab", 2);
        assert!(
            UnicodeWidthStr::width(cut.as_str()) <= 2,
            "over budget: {cut:?}"
        );
        assert_eq!(truncate_to_cols("anything", 0), "");
    }

    /// Assert the places panel's right border on `row` is intact and no
    /// reversed selected-bar cell reaches or passes it (the overflow that
    /// truncation fixes). Panel width 72 centred in 80 cols → x=4, border at 75.
    fn assert_places_border_intact(buffer: &ratatui::buffer::Buffer, row: u16) {
        let width = 72u16;
        let x = (80 - width) / 2;
        let right_border = x + width - 1;
        assert_eq!(
            buffer[(right_border, row)].symbol(),
            "│",
            "right border overwritten"
        );
        for col in right_border..buffer.area.width {
            assert!(
                !buffer[(col, row)].modifier.contains(Modifier::REVERSED),
                "selected bar spilled to column {col} on row {row}"
            );
        }
    }

    /// The row carrying the truncation ellipsis, or panics.
    fn ellipsis_row(buffer: &ratatui::buffer::Buffer) -> u16 {
        let mut row = None;
        for y in 0..buffer.area.height {
            if row_text(buffer, y, 80).contains('…') {
                row = Some(y);
            }
        }
        row.expect("a truncated bookmark row with an ellipsis")
    }

    #[test]
    fn places_row_truncates_and_keeps_border_intact() {
        let mut m = loaded_model((80, 24));
        // A bookmark whose label (title + url + emoji) far exceeds the panel's
        // inner width, so the selected row must be truncated with an ellipsis.
        // The title carries a VS16 emoji-presentation sequence (☯️ = U+262F +
        // U+FE0F), which unicode-width under-counts as one column: with the old
        // plain-width truncation the row spilled past the border, so this test
        // would have caught the bug.
        m.bookmarks.add(Bookmark {
            url: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:/page/a-very-long-page-path.mu".to_string(),
            title: "☯️ an extremely long bookmark title that cannot fit inside the panel"
                .to_string(),
        });
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, 0, "the long bookmark is the selected row");
        let buffer = render(&m, 80, 24);
        assert_places_border_intact(&buffer, ellipsis_row(&buffer));
    }

    #[test]
    fn places_row_real_vs16_title_stays_inside_border() {
        let mut m = loaded_model((80, 24));
        // The exact real-world bookmark that overflowed: two emoji-presentation
        // sequences (☯️ and ☸️, each base + U+FE0F). unicode-width scores each
        // as one column, the terminal paints two, so the row shifted two
        // columns right, past the panel's right border.
        m.bookmarks.add(Bookmark {
            url: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:/page/index.mu".to_string(),
            title: "☯️  Zen of Reticulum ☸️".to_string(),
        });
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(m.places_sel, 0, "the long bookmark is the selected row");
        let buffer = render(&m, 80, 24);
        assert_places_border_intact(&buffer, ellipsis_row(&buffer));
    }

    #[test]
    fn places_row_ordinary_wide_emoji_still_truncates() {
        // Regression guard: rows with ordinary wide emoji (🌐, 📰) that
        // unicode-width already scores as two columns keep truncating cleanly.
        for title in [
            "🌐 a very long bookmark title that overflows the places panel border",
            "📰 a very long bookmark title that overflows the places panel border",
        ] {
            let mut m = loaded_model((80, 24));
            m.bookmarks.add(Bookmark {
                url: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:/page/a-very-long-page-path.mu".to_string(),
                title: title.to_string(),
            });
            press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
            let buffer = render(&m, 80, 24);
            assert_places_border_intact(&buffer, ellipsis_row(&buffer));
        }
    }

    #[test]
    fn places_panel_renders_both_sections() {
        let mut m = loaded_model((80, 24));
        m.bookmarks.add(Bookmark {
            url: "aa11:/page/index.mu".to_string(),
            title: "Home".to_string(),
        });
        m.node_registry.upsert_node(&disc_node(0xcd, "Node-C"));
        press(&mut m, KeyCode::Char('d'), KeyModifiers::NONE);
        let buffer = render(&m, 80, 24);
        let text = flat(&buffer);
        assert!(text.contains("Bookmarks"), "missing header:\n{text}");
        assert!(text.contains("Discovered nodes"), "missing header:\n{text}");
        assert!(text.contains("Home"), "missing bookmark:\n{text}");
        assert!(text.contains("Node-C"), "missing node:\n{text}");
    }

    // --- Phase 7: editable form fields + submit -------------------------

    const HASH_HEX: &str = "0123456789abcdef0123456789abcdef";

    /// A loaded model from a micron source with a current destination set, so
    /// same-destination links and field submission resolve.
    fn field_model(src: &str, size: (u16, u16)) -> Model {
        let mut m = Model::from_document(&parse(src), content_width(size.0), "T", size);
        m.history.visit(tgt(0xab));
        m.current_dest = Some([0xab; 16]);
        m
    }

    #[test]
    fn field_store_inits_from_prefill() {
        let src = "Name: `<name`bob>\nAgree: `<?|agree`Agree>";
        let m = field_model(src, (80, 24));
        assert_eq!(m.field_state.len(), 2);
        assert_eq!(m.field_state[0].name, "name");
        assert_eq!(m.field_state[0].kind, FieldKind::Text);
        assert_eq!(m.field_state[0].input.value(), "bob");
        assert_eq!(m.field_state[1].name, "agree");
        assert_eq!(m.field_state[1].kind, FieldKind::Checkbox);
        assert!(!m.field_state[1].checked, "checkbox starts unchecked");
    }

    #[test]
    fn typing_edits_focused_text_field() {
        let src = "Name: `<name`bo>";
        let mut m = field_model(src, (80, 24));
        // Tab focuses the only interactive element, the text field.
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.field_focus, Some(1));
        assert_eq!(m.mode, Mode::Field);
        // The editor starts with the cursor at the end of "bo"; typing appends.
        type_str(&mut m, "b");
        assert_eq!(m.field_state[0].input.value(), "bob");
        // The re-layout reflects the new value in the laid-out widget.
        let field_line: String = m.page[m.field_defs[0].line]
            .cells
            .iter()
            .map(|c| c.ch)
            .collect();
        assert!(
            field_line.contains("bob"),
            "widget not updated: {field_line:?}"
        );
        // The widget keeps its declared width while editing; typing fills it in
        // place rather than growing the box.
        let vf = visible_fields(&m)[0].clone();
        assert_eq!((vf.col_end - vf.col_start) as usize, 24);
    }

    #[test]
    fn long_text_scrolls_inside_the_field_box() {
        // An 8-wide field typed full of 12 characters: nothing may be lost, and
        // the box must slide so the caret — and the tail being typed — stay in
        // view. This is what makes long chat messages enterable.
        let src = "`<8|msg`>";
        let mut m = field_model(src, (80, 24));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.field_focus, Some(1));
        type_str(&mut m, "abcdefghijkl");
        assert_eq!(
            m.field_state[0].input.value(),
            "abcdefghijkl",
            "the editor must keep the whole text, not just what fits"
        );

        let vf = visible_fields(&m)[0].clone();
        assert_eq!(
            (vf.col_end - vf.col_start) as usize,
            8,
            "box keeps its width"
        );

        let line: String = m.page[m.field_defs[0].line]
            .cells
            .iter()
            .map(|c| c.ch)
            .collect();
        assert!(
            line.contains("fghijkl"),
            "box did not scroll to the caret: {line:?}"
        );
        assert!(
            !line.contains("abcde"),
            "box still shows the head, so the caret is off-screen: {line:?}"
        );
    }

    #[test]
    fn space_toggles_focused_checkbox() {
        let src = "Agree: `<?|agree`Agree>";
        let mut m = field_model(src, (80, 24));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.field_focus, Some(1));
        assert!(!m.field_state[0].checked);
        press(&mut m, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(m.field_state[0].checked, "Space should check the box");
        press(&mut m, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!m.field_state[0].checked, "Space should uncheck it again");
    }

    #[test]
    fn space_selects_radio_and_deselects_group_siblings() {
        // Two radios sharing the name "col": selecting one deselects the other.
        let src = "`<^|col`Red>`<^|col`Blue>";
        let mut m = field_model(src, (80, 24));
        assert_eq!(m.field_state.len(), 2);
        // Focus + select the first radio.
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut m, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(m.field_state[0].checked);
        assert!(!m.field_state[1].checked);
        // Move to the second and select it: the first must clear.
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut m, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!m.field_state[0].checked, "sibling radio not cleared");
        assert!(m.field_state[1].checked);
    }

    #[test]
    fn click_toggles_checkbox() {
        let src = "Agree: `<?|agree`Agree>";
        let mut m = field_model(src, (80, 24));
        let vf = visible_fields(&m)[0].clone();
        update(&mut m, AppEvent::Mouse(click(vf.col_start, vf.row)));
        assert!(m.field_state[0].checked, "click should check the box");
        let vf = visible_fields(&m)[0].clone();
        update(&mut m, AppEvent::Mouse(click(vf.col_start, vf.row)));
        assert!(!m.field_state[0].checked, "second click should uncheck it");
    }

    #[test]
    fn click_selects_radio() {
        // Two radios sharing the name "col": clicking one selects it and clears
        // its sibling; clicking the selected one keeps it selected (a radio can
        // only be unselected by selecting another in its group).
        let src = "`<^|col`Red>`<^|col`Blue>";
        let mut m = field_model(src, (80, 24));
        let vf = visible_fields(&m)[0].clone();
        update(&mut m, AppEvent::Mouse(click(vf.col_start, vf.row)));
        assert!(m.field_state[0].checked, "click should select the radio");
        assert!(!m.field_state[1].checked);
        let vf = visible_fields(&m)[1].clone();
        update(&mut m, AppEvent::Mouse(click(vf.col_start, vf.row)));
        assert!(!m.field_state[0].checked, "sibling radio not cleared");
        assert!(m.field_state[1].checked);
        let vf = visible_fields(&m)[1].clone();
        update(&mut m, AppEvent::Mouse(click(vf.col_start, vf.row)));
        assert!(
            m.field_state[1].checked,
            "clicking the selected radio must keep it selected"
        );
    }

    #[test]
    fn click_on_text_field_only_focuses() {
        let src = "Name: `<name`bob>";
        let mut m = field_model(src, (80, 24));
        let vf = visible_fields(&m)[0].clone();
        update(&mut m, AppEvent::Mouse(click(vf.col_start, vf.row)));
        assert_eq!(m.mode, Mode::Field);
        assert_eq!(m.field_focus, Some(1));
        assert_eq!(
            m.field_state[0].input.value(),
            "bob",
            "a click must not edit the text"
        );
    }

    /// Enter hint mode and type the label of field `fi`'s hint badge.
    fn activate_field_hint(m: &mut Model, fi: usize) {
        press(m, KeyCode::Char('f'), KeyModifiers::NONE);
        assert_eq!(m.mode, Mode::Hint);
        let label = hints(m)
            .into_iter()
            .find(|h| h.target == HintTarget::Field(fi))
            .expect("field hint present")
            .label;
        for c in label.chars() {
            press(m, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn hints_include_visible_fields() {
        let src = "`[Alpha`:/page/a.mu]\nAgree: `<?|agree`Agree>\nName: `<name`bob>";
        let m = field_model(src, (80, 24));
        let fields = visible_fields(&m);
        assert_eq!(fields.len(), 2);
        let hs = hints(&m);
        for vf in fields {
            let hint = hs
                .iter()
                .find(|h| h.target == HintTarget::Field(vf.index))
                .expect("every visible field gets a hint");
            assert_eq!((hint.row, hint.col), (vf.row, vf.col_start));
        }
    }

    #[test]
    fn hint_labels_cover_links_fields_and_controls() {
        let src = "`[Alpha`:/page/a.mu]\nAgree: `<?|agree`Agree>\nName: `<name`bob>";
        let m = field_model(src, (80, 24));
        assert_eq!(visible_links(&m).len(), 1);
        assert_eq!(visible_fields(&m).len(), 2);
        let [topbar, _content, _status] = regions(m.frame_area());
        let expected =
            visible_links(&m).len() + visible_fields(&m).len() + top_bar_controls(&m, topbar).len();
        let hs = hints(&m);
        assert_eq!(hs.len(), expected, "no target dropped or double-counted");
        let labels: std::collections::HashSet<_> = hs.iter().map(|h| &h.label).collect();
        assert_eq!(labels.len(), hs.len(), "labels must be unique");
    }

    #[test]
    fn hint_activate_checkbox_toggles() {
        let src = "Agree: `<?|agree`Agree>";
        let mut m = field_model(src, (80, 24));
        activate_field_hint(&mut m, 1);
        assert!(m.field_state[0].checked, "hint should check the box");
        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        activate_field_hint(&mut m, 1);
        assert!(!m.field_state[0].checked, "second hint should uncheck it");
    }

    #[test]
    fn hint_activate_radio_selects() {
        // Two radios sharing the name "col": hinting one selects it and clears
        // its group sibling, same as a click.
        let src = "`<^|col`Red>`<^|col`Blue>";
        let mut m = field_model(src, (80, 24));
        activate_field_hint(&mut m, 1);
        assert!(m.field_state[0].checked, "hint should select the radio");
        assert!(!m.field_state[1].checked);
        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        activate_field_hint(&mut m, 2);
        assert!(!m.field_state[0].checked, "sibling radio not cleared");
        assert!(m.field_state[1].checked);
    }

    #[test]
    fn hint_activate_text_field_focuses() {
        let src = "Name: `<name`bob>";
        let mut m = field_model(src, (80, 24));
        activate_field_hint(&mut m, 1);
        assert_eq!(m.mode, Mode::Field, "text field hint enters field editing");
        assert_eq!(m.field_focus, Some(1));
        assert_eq!(
            m.field_state[0].input.value(),
            "bob",
            "a hint must not edit the text"
        );
    }

    #[test]
    fn esc_leaves_field_editing() {
        let src = "Name: `<name`>";
        let mut m = field_model(src, (80, 24));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(m.mode, Mode::Field);
        press(&mut m, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(m.mode, Mode::Browse);
        assert_eq!(m.field_focus, None);
    }

    #[test]
    fn visible_fields_positions_after_wide_char() {
        // A width-2 emoji then a space, then the field: the widget starts at
        // display column 3 (2 + 1), not character index 2.
        let src = "\u{1f389} `<name`>";
        let m = field_model(src, (80, 24));
        let vf = visible_fields(&m);
        assert_eq!(vf.len(), 1);
        assert_eq!(vf[0].row, TOPBAR_ROWS);
        assert_eq!(vf[0].col_start, 3, "field mispositioned after wide char");
        // An empty text field still spans its declared micron width (default 24),
        // like NomadNet's full-width input bar — not the width of its content.
        assert_eq!(vf[0].col_end, 3 + 24);
    }

    #[test]
    fn text_field_box_spans_declared_width_not_content() {
        // Default width (24) with a prefill, an explicit width, and an empty
        // field: the widget is always exactly `width` columns wide.
        for (src, width) in [
            ("`<name`Guest>", 24usize),
            ("`<12|nick`Guest>", 12),
            ("`<40|msg`>", 40),
        ] {
            let m = field_model(src, (120, 24));
            let vf = visible_fields(&m);
            assert_eq!(vf.len(), 1, "one field in {src:?}");
            assert_eq!(
                (vf[0].col_end - vf[0].col_start) as usize,
                width,
                "field box in {src:?} must span its declared width {width}"
            );
        }
    }

    #[test]
    fn submit_link_carries_collected_field_values() {
        // A page with a text field "name" (prefill "bob") and a submit link that
        // references it plus a `k=v` preset.
        let src = format!("`<name`bob>\n`[Go`{HASH_HEX}:/page/s.mu`name|a=1]");
        let mut m = field_model(&src, (80, 24));
        let effects = follow_link(&mut m, 1);
        let target = match effects.as_slice() {
            [Effect::Navigate(t)] => t.clone(),
            other => panic!("expected a single Navigate, got {other:?}"),
        };
        assert_eq!(target.path, "/page/s.mu");
        // The preset lands as var_a; the collected field value as field_name.
        assert!(
            target
                .fields
                .contains(&("var_a".to_string(), "1".to_string())),
            "preset missing: {:?}",
            target.fields
        );
        assert!(
            target
                .fields
                .contains(&("field_name".to_string(), "bob".to_string())),
            "collected field missing: {:?}",
            target.fields
        );
    }

    #[test]
    fn submit_link_reflects_edited_value_and_ignores_unreferenced_fields() {
        // Two text fields; the link references only "q". Editing "q" must flow
        // through, and the unreferenced "other" field must not be submitted.
        let src = format!("`<q`hi>`<other`x>\n`[Go`{HASH_HEX}:/page/s.mu`q]");
        let mut m = field_model(&src, (80, 24));
        // Focus the first field (q) and append "!".
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        type_str(&mut m, "!");
        assert_eq!(m.field_state[0].input.value(), "hi!");
        let link = m.links.iter().find(|l| l.index == 1).cloned().unwrap();
        let collected = collect_submit_fields(&m, &link);
        assert_eq!(
            collected,
            vec![("field_q".to_string(), "hi!".to_string())],
            "only the referenced, edited field should be collected"
        );
    }

    #[test]
    fn submit_link_star_collects_all_fields() {
        let src = format!("`<a`1>`<b`2>\n`[Go`{HASH_HEX}:/page/s.mu`*]");
        let m = field_model(&src, (80, 24));
        let link = m.links.iter().find(|l| l.index == 1).cloned().unwrap();
        let collected = collect_submit_fields(&m, &link);
        assert_eq!(
            collected,
            vec![
                ("field_a".to_string(), "1".to_string()),
                ("field_b".to_string(), "2".to_string()),
            ],
            "`*` should collect every field"
        );
    }

    #[test]
    fn unchecked_checkbox_is_not_submitted() {
        let src = format!("`<?|agree`Agree>\n`[Go`{HASH_HEX}:/page/s.mu`agree]");
        let mut m = field_model(&src, (80, 24));
        let link = m.links.iter().find(|l| l.index == 1).cloned().unwrap();
        // Unchecked: nothing collected.
        assert!(collect_submit_fields(&m, &link).is_empty());
        // Check it (focus the checkbox, Space), then it submits its value.
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut m, KeyCode::Char(' '), KeyModifiers::NONE);
        let collected = collect_submit_fields(&m, &link);
        assert_eq!(
            collected,
            vec![("field_agree".to_string(), "Agree".to_string())],
            "a checked box submits its value (the label when none given)"
        );
    }

    #[test]
    fn mouse_click_on_field_focuses_it() {
        let src = "Name: `<name`>";
        let mut m = field_model(src, (80, 24));
        let vf = visible_fields(&m)[0].clone();
        let effects = update(&mut m, AppEvent::Mouse(click(vf.col_start, vf.row)));
        assert!(effects.is_empty(), "focusing a field yields no effect");
        assert_eq!(m.field_focus, Some(1));
        assert_eq!(m.mode, Mode::Field);
    }

    #[test]
    fn field_state_survives_resize() {
        let src = "Name: `<name`>";
        let mut m = field_model(src, (80, 24));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        type_str(&mut m, "kept");
        assert_eq!(m.field_state[0].input.value(), "kept");
        update(&mut m, AppEvent::Resize(60, 20));
        assert_eq!(
            m.field_state[0].input.value(),
            "kept",
            "a resize must not reset the field store"
        );
    }

    // --- Phase 7: TestBackend rendering ---------------------------------

    #[test]
    fn input_field_renders_its_value_in_a_box() {
        let src = "Name: `<name`bob>";
        let m = field_model(src, (80, 24));
        let buffer = render(&m, 80, 24);
        assert!(flat(&buffer).contains("bob"), "field value not drawn");
        let vf = visible_fields(&m)[0].clone();
        // The box spans the field's DECLARED width (default 24), not "bob" (3).
        assert_eq!((vf.col_end - vf.col_start) as usize, 24);
        // Every cell of the box — including the padding past the text — carries
        // the input-box background, so the field's full extent is visible, the
        // way NomadNet draws an input bar.
        let want = Color::Rgb(
            Theme::Dark.chrome_bg().0,
            Theme::Dark.chrome_bg().1,
            Theme::Dark.chrome_bg().2,
        );
        for x in vf.col_start..vf.col_end {
            assert_eq!(
                buffer[(x, vf.row)].bg,
                want,
                "input box not filled at column {x}"
            );
        }
    }

    #[test]
    fn focused_field_is_highlighted() {
        let src = "Name: `<name`bob>";
        let mut m = field_model(src, (80, 24));
        press(&mut m, KeyCode::Tab, KeyModifiers::NONE);
        let vf = visible_fields(&m)[0].clone();
        let buffer = render(&m, 80, 24);
        let cell = &buffer[(vf.col_start, vf.row)];
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "a focused field should be highlighted"
        );
    }

    #[test]
    fn checkbox_renders_its_state() {
        // Prechecked checkbox renders "[x]"; an unchecked one "[ ]".
        let checked = field_model("`<?|a|Yes|*`On>", (80, 24));
        assert!(flat(&render(&checked, 80, 24)).contains("[x] On"));
        let unchecked = field_model("`<?|a`Off>", (80, 24));
        assert!(flat(&render(&unchecked, 80, 24)).contains("[ ] Off"));
    }

    // --- inline images ----------------------------------------------------

    /// The micron a blog page with one picture looks like: a standalone link
    /// to a file in the node's file area, which is the only form micron has.
    const IMAGE_PAGE: &str = "\
>>Antennenbau

Der Mast steht.

`[Antenne`:/file/antenne.png]

Text unter dem Bild.
";

    /// A model showing IMAGE_PAGE, loaded through the ordinary load path so the
    /// image slots are planned exactly as they are in production.
    fn image_model() -> (Model, Vec<Effect>) {
        let mut m = Model {
            size: (80, 24),
            current_dest: Some([0xab; 16]),
            ..Model::default()
        };
        m.relayout(content_width(80));
        let effects = update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse(IMAGE_PAGE),
                title: "Blog".to_string(),
            },
        );
        (m, effects)
    }

    /// The probe a fetched picture would have produced.
    fn probe() -> Probe {
        Probe {
            format: "PNG",
            width: 640,
            height: 320,
            len: 12_400,
        }
    }

    #[test]
    fn a_standalone_image_link_is_planned_and_fetched() {
        let (m, effects) = image_model();
        assert_eq!(m.images.len(), 1, "the picture must get a slot");
        assert_eq!(m.images[0].name, "antenne.png");
        assert_eq!(m.images[0].state, SlotState::Queued);

        let [Effect::FetchImages(requests)] = &effects[..] else {
            panic!("expected one FetchImages effect, got {effects:?}");
        };
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].link, m.images[0].link);
        assert_eq!(requests[0].target.path, "/file/antenne.png");
        assert!(requests[0].target.is_file);
        assert_eq!(
            requests[0].target.dest_hash, [0xab; 16],
            "a `:`-relative link resolves against the page's own node"
        );
    }

    #[test]
    fn a_page_without_pictures_asks_for_nothing() {
        let mut m = model_from_sample(78, (80, 24));
        let effects = update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse(SAMPLE),
                title: "Sample".to_string(),
            },
        );
        assert!(m.images.is_empty());
        assert!(!effects.iter().any(|e| matches!(e, Effect::FetchImages(_))));
    }

    #[test]
    fn images_off_plans_nothing_at_all() {
        let mut m = Model {
            size: (80, 24),
            no_images: true,
            current_dest: Some([0xab; 16]),
            ..Model::default()
        };
        let effects = update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse(IMAGE_PAGE),
                title: "Blog".to_string(),
            },
        );
        assert!(m.images.is_empty(), "--images off must plan no pictures");
        assert!(effects.is_empty(), "and must ask for no transfers");
        // The link is still there and still followable.
        assert_eq!(m.links.len(), 1);
    }

    #[test]
    fn a_page_beyond_the_ceiling_fetches_only_the_first_ones() {
        let mut page = String::new();
        for i in 0..(MAX_INLINE_IMAGES + 3) {
            page.push_str(&format!("`[Bild {i}`:/file/bild{i}.png]\n\n"));
        }
        let mut m = Model {
            size: (80, 40),
            current_dest: Some([0xab; 16]),
            ..Model::default()
        };
        let effects = update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse(&page),
                title: "Viele".to_string(),
            },
        );

        assert_eq!(
            m.images.len(),
            MAX_INLINE_IMAGES + 3,
            "every one gets a slot"
        );
        let fetched = m
            .images
            .iter()
            .filter(|s| s.state == SlotState::Queued)
            .count();
        assert_eq!(fetched, MAX_INLINE_IMAGES);
        let skipped = m
            .images
            .iter()
            .filter(|s| s.state == SlotState::TooMany)
            .count();
        assert_eq!(skipped, 3, "a hostile page cannot start fifty transfers");

        let [Effect::FetchImages(requests)] = &effects[..] else {
            panic!("expected one FetchImages effect, got {effects:?}");
        };
        assert_eq!(requests.len(), MAX_INLINE_IMAGES);
    }

    #[test]
    fn a_ready_picture_reserves_its_rows_and_pushes_the_text_down() {
        let (mut m, _) = image_model();
        let link_line = m.links[0].line;
        let below: Vec<String> = m.page[link_line + 2..]
            .iter()
            .map(|l| l.cells.iter().map(|c| c.ch).collect())
            .collect();

        let link = m.images[0].link;
        update(
            &mut m,
            AppEvent::ImageReady {
                link,
                cells: (40, 6),
                probe: probe(),
            },
        );

        assert_eq!(m.images[0].state.rows(), 6);
        let after: Vec<String> = m.page[link_line + 7..]
            .iter()
            .map(|l| l.cells.iter().map(|c| c.ch).collect())
            .collect();
        assert_eq!(after, below, "the text below must survive, only moved");
    }

    #[test]
    fn a_terminal_that_cannot_draw_gets_a_line_that_says_what_the_picture_is() {
        let (mut m, _) = image_model();
        let link = m.images[0].link;
        update(
            &mut m,
            AppEvent::ImageText {
                link,
                probe: Some(probe()),
                reason: "this terminal shows no images".to_string(),
            },
        );

        let drawn = flat(&render(&m, 80, 24));
        assert!(
            drawn.contains("antenne.png, PNG 640x320, 12.4 kB - this terminal shows no images"),
            "the fallback must say what the file is: {drawn}"
        );
    }

    #[test]
    fn a_failed_transfer_says_so_without_losing_the_link() {
        let (mut m, _) = image_model();
        let link = m.images[0].link;
        update(
            &mut m,
            AppEvent::ImageFailed {
                link,
                reason: "request timed out".to_string(),
            },
        );
        let drawn = flat(&render(&m, 80, 24));
        assert!(drawn.contains("antenne.png: request timed out"), "{drawn}");
        assert!(drawn.contains("Antenne"), "the link itself must remain");
    }

    #[test]
    fn a_queued_picture_shows_that_it_is_coming() {
        let (m, _) = image_model();
        let drawn = flat(&render(&m, 80, 24));
        assert!(drawn.contains("antenne.png: queued"), "{drawn}");
    }

    #[test]
    fn following_an_arrived_picture_saves_it_instead_of_fetching_it_again() {
        let (mut m, _) = image_model();
        let link = m.images[0].link;
        update(
            &mut m,
            AppEvent::ImageReady {
                link,
                cells: (40, 6),
                probe: probe(),
            },
        );
        m.focus = Some(m.images[0].link);

        let effects = press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            effects,
            vec![Effect::SaveImage {
                link: m.images[0].link
            }],
            "the bytes are already here; a second transfer would be minutes of airtime"
        );

        let effects = press(&mut m, KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(
            effects,
            vec![Effect::OpenImage {
                link: m.images[0].link
            }]
        );
    }

    #[test]
    fn following_a_picture_that_has_not_arrived_downloads_it() {
        let (mut m, _) = image_model();
        m.focus = Some(m.images[0].link);
        let effects = press(&mut m, KeyCode::Enter, KeyModifiers::NONE);
        let [Effect::Download(target)] = &effects[..] else {
            panic!("expected the ordinary download path, got {effects:?}");
        };
        assert_eq!(target.path, "/file/antenne.png");
    }

    #[test]
    fn o_does_nothing_without_a_focused_picture() {
        let (mut m, _) = image_model();
        assert!(press(&mut m, KeyCode::Char('o'), KeyModifiers::NONE).is_empty());
    }

    #[test]
    fn navigating_away_drops_the_pictures_of_the_page_left_behind() {
        let (mut m, _) = image_model();
        let link = m.images[0].link;
        update(
            &mut m,
            AppEvent::ImageReady {
                link,
                cells: (40, 6),
                probe: probe(),
            },
        );
        assert_eq!(m.images.len(), 1);

        // A fresh page with no pictures replaces the slots outright, and the
        // shell aborts the queue on the Navigate/FetchImages that come with it.
        update(
            &mut m,
            AppEvent::PageLoaded {
                doc: parse("Nur Text.\n"),
                title: "Text".to_string(),
            },
        );
        assert!(m.images.is_empty());
    }

    #[test]
    fn a_picture_scrolled_out_of_view_is_not_drawn_over_the_chrome() {
        // The status line for a slot above the viewport must not be painted
        // into the top bar, which is what a naive row calculation would do.
        let (mut m, _) = image_model();
        let link = m.images[0].link;
        update(
            &mut m,
            AppEvent::ImageText {
                link,
                probe: Some(probe()),
                reason: "this terminal shows no images".to_string(),
            },
        );
        m.scroll = m.page.len();
        let buffer = render(&m, 80, 24);
        let top: String = (0..80)
            .map(|x| buffer[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            !top.contains("antenne.png"),
            "a scrolled-away picture must not land on the top bar: {top}"
        );
    }

    /// A tiny picture, encoded so the test data cannot drift from what the
    /// decoder is handed.
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut img = ::image::RgbaImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = ::image::Rgba([(x * 8 % 256) as u8, (y * 8 % 256) as u8, 200, 255]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        ::image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ::image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    /// A store that draws with half-blocks, the fallback every colour terminal
    /// can take. Pinned rather than detected: a test must not depend on which
    /// terminal happens to be running it.
    fn halfblock_store() -> ImageStore {
        ImageStore {
            // Pinned rather than queried: `halfblocks()` fixes both the
            // protocol and the font size (10x20), so the assertions below do
            // not depend on which terminal runs the test.
            picker: Some(Picker::halfblocks()),
            backend: Backend::Halfblocks,
            entries: HashMap::new(),
            task: None,
            generation: 0,
            cache: ImageCache::default(),
        }
    }

    #[test]
    fn a_fetched_picture_is_encoded_and_painted_into_its_reserved_rows() {
        // The whole draw path, end to end: payload -> probe -> decode ->
        // half-block encode -> cells in the frame buffer.
        let (mut m, _) = image_model();
        let link = m.images[0].link;
        let mut store = halfblock_store();

        let event = store.accept(link, "antenne.png".to_string(), png_bytes(64, 32), &m);
        let AppEvent::ImageReady { cells, probe, .. } = &event else {
            panic!("a valid PNG on a half-block terminal must be drawable, got {event:?}");
        };
        assert_eq!((probe.width, probe.height), (64, 32));
        let (cols, rows) = *cells;
        assert!(cols > 0 && rows > 0);
        update(&mut m, event);

        let buffer = render_with_images(&m, &store, 80, 24);
        // Page line -> screen row: the content region starts below the top bar,
        // and the picture's rows start one line under its link.
        let content = regions(Rect::new(0, 0, 80, 24))[1];
        let first = content.y + (m.links[0].line + 1 - m.scroll) as u16;
        let left = content.x + m.links[0].col_start as u16;
        let painted = (0..rows)
            .flat_map(|y| (0..cols).map(move |x| (x, y)))
            .filter(|(x, y)| {
                let cell = &buffer[(left + x, first + y)];
                cell.symbol() != " " || cell.bg != Color::Reset
            })
            .count();
        assert_eq!(
            painted,
            (cols * rows) as usize,
            "every reserved cell must carry part of the picture ({cols}x{rows} at \
             row {first}, col {left})"
        );
    }

    #[test]
    fn rubbish_that_claims_to_be_a_picture_becomes_a_line_of_text() {
        // The payload comes from somebody else's node. Undecodable bytes must
        // not panic, and must leave the reader something readable.
        let (mut m, _) = image_model();
        let link = m.images[0].link;
        let mut store = halfblock_store();

        let event = store.accept(link, "antenne.png".to_string(), b"not a png".to_vec(), &m);
        let AppEvent::ImageText { probe, reason, .. } = &event else {
            panic!("undecodable bytes must fall back to text, got {event:?}");
        };
        assert!(probe.is_none());
        assert!(reason.contains("decode"), "{reason}");
        update(&mut m, event);
        assert!(flat(&render_with_images(&m, &store, 80, 24)).contains("antenne.png"));
    }

    #[test]
    fn a_resize_re_encodes_every_held_picture_for_the_new_width() {
        let (mut m, _) = image_model();
        let link = m.images[0].link;
        let mut store = halfblock_store();
        let event = store.accept(link, "antenne.png".to_string(), png_bytes(640, 320), &m);
        let wide = match event {
            AppEvent::ImageReady { cells, .. } => cells,
            other => panic!("expected a drawable picture, got {other:?}"),
        };
        update(&mut m, event);

        update(&mut m, AppEvent::Resize(40, 24));
        let events = store.reencode_all(&m);
        let narrow = match events.first() {
            Some(AppEvent::ImageReady { cells, .. }) => *cells,
            other => panic!("expected a re-encoded picture, got {other:?}"),
        };
        assert!(
            narrow.0 < wide.0,
            "a narrower terminal must draw the picture smaller: {narrow:?} vs {wide:?}"
        );
    }

    #[test]
    fn a_terminal_without_a_picker_reports_that_it_shows_no_images() {
        let (m, _) = image_model();
        let mut store = ImageStore::default();
        let event = store.accept(1, "antenne.png".to_string(), png_bytes(8, 8), &m);
        assert!(matches!(
            event,
            AppEvent::ImageText { reason, .. } if reason.contains("no images")
        ));
    }

    #[test]
    fn every_graphics_protocol_encodes_the_same_payload() {
        // The half-block path is asserted above end to end. This pins that we
        // drive the other three correctly too: each must accept the payload and
        // report a drawable size, since a terminal that answers "Kitty" gets
        // whichever one it named and there is no second chance.
        let (m, _) = image_model();
        for protocol in [
            ProtocolType::Kitty,
            ProtocolType::Iterm2,
            ProtocolType::Sixel,
            ProtocolType::Halfblocks,
        ] {
            let mut picker = Picker::halfblocks();
            picker.set_protocol_type(protocol);
            let mut store = ImageStore {
                picker: Some(picker),
                backend: Backend::Graphics(protocol),
                entries: HashMap::new(),
                task: None,
                generation: 0,
                cache: ImageCache::default(),
            };
            let event = store.accept(1, "antenne.png".to_string(), png_bytes(64, 32), &m);
            match event {
                AppEvent::ImageReady { cells, .. } => {
                    assert!(cells.0 > 0 && cells.1 > 0, "{protocol:?} gave {cells:?}");
                    assert!(
                        store.protocol(1).is_some(),
                        "{protocol:?} must leave something to draw"
                    );
                }
                other => panic!("{protocol:?} must encode a valid PNG, got {other:?}"),
            }
        }
    }

    /// The fetch target the fixture page's picture resolves to.
    fn image_target() -> Target {
        Target {
            dest_hash: [0xab; 16],
            path: "/file/antenne.png".to_string(),
            fields: Vec::new(),
            is_file: true,
        }
    }

    #[test]
    fn a_cached_picture_is_shown_without_a_transfer() {
        // The whole point of the cache: stepping back to a page must not spend
        // the airtime again.
        let (m, effects) = image_model();
        let [Effect::FetchImages(requests)] = &effects[..] else {
            panic!("expected one FetchImages effect, got {effects:?}");
        };
        let mut store = halfblock_store();
        store.cache.insert(image_target(), png_bytes(64, 32));

        let (events, pending) = store.take_cached(requests.clone(), &m);
        assert!(
            pending.is_empty(),
            "nothing may be left to fetch: {pending:?}"
        );
        assert!(
            matches!(events[..], [AppEvent::ImageReady { .. }]),
            "the cached payload must arrive drawable, got {events:?}"
        );
        assert!(store.protocol(requests[0].link).is_some());
    }

    #[test]
    fn an_uncached_picture_still_has_to_be_fetched() {
        let (m, effects) = image_model();
        let [Effect::FetchImages(requests)] = &effects[..] else {
            panic!("expected one FetchImages effect, got {effects:?}");
        };
        let mut store = halfblock_store();
        // A picture from another node under the same name is a different
        // picture, and must not be served from this entry.
        store.cache.insert(
            Target {
                dest_hash: [0x11; 16],
                ..image_target()
            },
            png_bytes(64, 32),
        );

        let (events, pending) = store.take_cached(requests.clone(), &m);
        assert!(events.is_empty());
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn leaving_a_page_keeps_the_cache_but_drops_the_page_holdings() {
        let (m, _) = image_model();
        let mut store = halfblock_store();
        store.accept(1, "antenne.png".to_string(), png_bytes(64, 32), &m);
        store.cache.insert(image_target(), png_bytes(64, 32));

        store.reset();

        assert!(
            store.protocol(1).is_none(),
            "the page's encoded pictures go with the page"
        );
        assert!(
            store.cache.contains(&image_target()),
            "the cache is what outlives it"
        );
    }

    #[test]
    fn the_cache_budget_comes_from_the_option() {
        let store = ImageStore {
            cache: ImageCache::new(2_000_000),
            ..halfblock_store()
        };
        assert_eq!(store.cache.max_bytes(), 2_000_000);
        // The default budget is what `--image-cache 10` asks for.
        assert_eq!(
            ImageStore::default().cache.max_bytes(),
            crate::image_cache::DEFAULT_MAX_BYTES
        );
    }

    #[test]
    fn a_photo_on_half_blocks_uses_the_page_width_not_the_font_box() {
        // The regression this guards: sizing a half-block picture by the
        // terminal's font box drew a 300x300 portrait as 38x38 half-block
        // pixels — "gigantic pixels", unrecognisable. A cell carries one pixel
        // across and two down, so the picture must instead grow to the page
        // width, and `Resize::Scale` must keep the protocol at that size.
        let (mut m, _) = image_model();
        let mut store = halfblock_store();

        let event = store.accept(1, "portrait.jpg".to_string(), png_bytes(300, 300), &m);
        let AppEvent::ImageReady { cells, .. } = &event else {
            panic!("a valid PNG must be drawable on half-blocks, got {event:?}");
        };
        let (cols, rows) = *cells;
        assert!(
            cols as usize >= m.width - 2,
            "a photo must use the page width on half-blocks, got {cols} of {}",
            m.width
        );
        assert!(
            cols > 38,
            "38 columns is the font-box fit this bug produced; got {cols}"
        );
        // Square source, square picture: the height follows the aspect ratio
        // in half-block geometry (one pixel across, two down).
        assert_eq!(
            rows * 2,
            cols,
            "a square picture must stay square: {cols}x{rows} cells"
        );

        // And the protocol really occupies that area rather than shrinking
        // back to the font box, which is what `Resize::Fit` did. The event has
        // to be folded first: until the slot is Ready, the reserved rows carry
        // the status line, not the picture.
        update(&mut m, event);
        let buffer = render_with_images(&m, &store, 80, 24);
        let content = regions(Rect::new(0, 0, 80, 24))[1];
        let first = content.y + (m.links[0].line + 1 - m.scroll) as u16;
        let left = content.x + m.links[0].col_start as u16;
        let painted = (0..cols)
            .filter(|x| {
                let c = &buffer[(left + x, first)];
                c.symbol() != " " || c.bg != Color::Reset
            })
            .count();
        assert_eq!(
            painted, cols as usize,
            "the picture's first row must be painted across its full width"
        );
    }
}
