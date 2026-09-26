//! `lnomad` binary: a terminal browser for NomadNet micron pages.
//!
//! It connects to a running `lnsd`/`rnsd` shared instance, fetches the page at
//! the given URL, renders it to ANSI text, and (on a tty) enters an interactive
//! navigation loop. Started without a URL on a tty it opens the browser's start
//! screen instead, with the places panel showing. With `--print`, or when stdout
//! is not a terminal, it fetches and prints a single page and exits, for
//! scripting and acceptance tests.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use leviculum_std::config::Config;
use lnomad::browser::{self, BrowserOptions};
use lnomad::cli::{resolve_args, Mode};
use lnomad::color::{resolve_depth, ColorFlag};
use lnomad::fetch::Session;
use lnomad::theme::ThemeFlag;
use lnomad::tui::run_tui;
use lnomad::url::parse_url;

/// Fallback render width when no terminal size can be detected.
const FALLBACK_WIDTH: usize = 80;

#[derive(Parser, Debug)]
#[command(
    name = "lnomad",
    version = env!("LEVICULUM_VERSION"),
    about = "Terminal browser for NomadNet micron pages"
)]
struct Args {
    /// Page URL: `<dest_hash>[:/page/x.mu[`f=v|...]]` (a bare hash opens the
    /// default page). Omitted on a terminal, the browser opens its start screen
    /// with the places panel showing.
    url: Option<String>,

    /// Shared-instance name to connect to (overrides the config file's).
    #[arg(long)]
    instance: Option<String>,

    /// Reticulum config directory (default: the platform default, like `lncp`).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Disable ANSI colour in the rendered output.
    #[arg(long)]
    no_color: bool,

    /// Colour theme for the interactive TUI: `auto` detects the terminal
    /// background, `light`/`dark` force a theme. Ignored with `--print` / non-tty.
    #[arg(long, value_enum, default_value_t = ThemeArg::Auto)]
    theme: ThemeArg,

    /// Terminal colour depth: `auto` picks true colour when `COLORTERM` is
    /// `truecolor`/`24bit` and otherwise downgrades to the xterm-256 palette;
    /// `truecolor`/`256` force the depth. `--no-color` still overrides this.
    #[arg(long, value_enum, default_value_t = ColorArg::Auto)]
    color: ColorArg,

    /// Render width in columns (default: the detected terminal width, else 80).
    #[arg(long)]
    width: Option<usize>,

    /// Per-request fetch timeout, in seconds.
    #[arg(long, default_value_t = 30)]
    timeout: u64,

    /// Where a `/file/` download is saved: an existing directory (or a path
    /// ending in `/`) receives the file under the server-sent name (else the
    /// URL basename), any other path names the exact file to write. Default:
    /// that name in the current working directory (existing files get ` (1)`,
    /// ` (2)`, ... appended).
    #[arg(long)]
    output: Option<PathBuf>,

    /// Inline images: `auto` draws a page's pictures where they are linked,
    /// using the terminal's graphics protocol (Kitty, iTerm2 or Sixel) or
    /// Unicode half-blocks; `off` leaves every image link an ordinary link and
    /// fetches nothing. Ignored with `--print` / non-tty, which never draw
    /// pictures.
    #[arg(long, value_enum, default_value_t = ImagesArg::Auto)]
    images: ImagesArg,

    /// How much fetched image data to keep in memory for revisits, in
    /// megabytes. A revisited page then costs no airtime for pictures it has
    /// already shown; when the budget is exceeded, the pictures untouched
    /// longest are dropped until it fits again. `0` disables the cache.
    #[arg(long, default_value_t = 10)]
    image_cache: u64,

    /// Fetch, render and print the page once, then exit (non-interactive).
    #[arg(long)]
    print: bool,
}

/// The `--images` choice.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ImagesArg {
    /// Draw pictures inline when the terminal can show them.
    Auto,
    /// Never fetch or draw pictures.
    Off,
}

/// The `--theme` choice, a clap-facing mirror of [`ThemeFlag`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ThemeArg {
    /// Detect the terminal background and pick the matching theme.
    Auto,
    /// Force the light theme.
    Light,
    /// Force the dark theme.
    Dark,
}

impl From<ThemeArg> for ThemeFlag {
    fn from(arg: ThemeArg) -> Self {
        match arg {
            ThemeArg::Auto => ThemeFlag::Auto,
            ThemeArg::Light => ThemeFlag::Light,
            ThemeArg::Dark => ThemeFlag::Dark,
        }
    }
}

/// The `--color` choice, a clap-facing mirror of [`ColorFlag`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ColorArg {
    /// Detect the depth from `COLORTERM`.
    Auto,
    /// Force 24-bit true colour.
    Truecolor,
    /// Force the xterm-256 palette.
    #[value(name = "256")]
    Ansi256,
}

impl From<ColorArg> for ColorFlag {
    fn from(arg: ColorArg) -> Self {
        match arg {
            ColorArg::Auto => ColorFlag::Auto,
            ColorArg::Truecolor => ColorFlag::Truecolor,
            ColorArg::Ansi256 => ColorFlag::Ansi256,
        }
    }
}

/// Detect the terminal width in columns, falling back to [`FALLBACK_WIDTH`].
fn detect_width() -> usize {
    terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), _)| w as usize)
        .filter(|&w| w > 0)
        .unwrap_or(FALLBACK_WIDTH)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    // Print-once mode: also chosen automatically when stdout is not a tty, so a
    // piped/redirected invocation never blocks on the UI. It also decides what a
    // missing URL means: the start screen on a terminal, an error otherwise.
    let interactive =
        !args.print && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let mode = match resolve_args(args.url.as_deref(), interactive) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("lnomad: {err}");
            return ExitCode::from(2);
        }
    };

    // In page mode, parse and validate the URL up front.
    let target = match &mode {
        Mode::Start => None,
        Mode::Page { url } => match parse_url(url, None) {
            Ok(target) => Some(target),
            Err(err) => {
                // URL first, then the reason: the reasons carry advice of their
                // own now, so trailing the URL behind one read as part of it.
                eprintln!("lnomad: {url}: {err}");
                return ExitCode::from(2);
            }
        },
    };

    // Resolve the colour depth once: the `--color` flag over the `COLORTERM`
    // heuristic. Threaded into both the print sink and the interactive TUI.
    let depth = resolve_depth(
        args.color.into(),
        std::env::var("COLORTERM").ok().as_deref(),
    );

    let opts = BrowserOptions {
        width: args.width.unwrap_or_else(detect_width).max(1),
        no_color: args.no_color || !std::io::stdout().is_terminal(),
        depth,
        timeout: Duration::from_secs(args.timeout),
        no_images: args.images == ImagesArg::Off,
        image_cache_bytes: args.image_cache.saturating_mul(1_000_000),
    };

    // Connect to the shared instance: an explicit --instance overrides the
    // config file's instance name; otherwise resolve it like lncp does.
    let config_dir = args
        .config
        .clone()
        .unwrap_or_else(Config::default_config_dir);
    let connect = match &args.instance {
        Some(name) => Session::connect_to(name, config_dir.join("storage")).await,
        None => Session::connect(&config_dir).await,
    };
    let mut session = match connect {
        Ok(session) => session,
        Err(err) => {
            eprintln!("lnomad: {err}");
            return ExitCode::FAILURE;
        }
    };

    // A /file/ target downloads to disk (never rendered, never a TUI session):
    // fetch the raw bytes, save them, print the save line.
    let downloading = target.as_ref().is_some_and(|t| t.is_file);

    if interactive && !downloading {
        // The TUI owns the session and drives navigation: it does the initial
        // fetch of `target` (or opens the start screen when there is none) and
        // every subsequent navigation (links, the address bar, history) itself,
        // keeping the UI live while a page loads. The session is moved in and
        // closed there, so we return directly rather than fall through to the
        // shared teardown below.
        return match run_tui(session, target, opts, args.theme.into()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("lnomad: {err}");
                ExitCode::FAILURE
            }
        };
    }

    // Non-interactive from here on, so a target is always present: `Mode::Start`
    // is only reachable when the browser can run interactively.
    let code = match target {
        Some(target) => {
            let mut out = std::io::stdout();
            let result = if target.is_file {
                browser::download_once(
                    &mut out,
                    &mut session,
                    &target,
                    args.output.as_deref(),
                    opts.timeout,
                )
                .await
            } else {
                browser::print_once(&mut out, &mut session, &target, &opts).await
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("lnomad: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        None => {
            // Unreachable: resolve_args rejects a missing URL when the browser
            // cannot run interactively.
            eprintln!("lnomad: a page URL is required when the browser cannot run interactively");
            ExitCode::from(2)
        }
    };

    // Best-effort teardown; the exit code already reflects the fetch outcome.
    let _ = session.close().await;
    code
}
