//! lnsd - Reticulum daemon
//!
//! This is the main daemon process that runs the Reticulum network stack.
//! Equivalent to rnsd in the Python implementation.

use std::path::PathBuf;

use clap::{ArgAction, Parser};
use tracing::info;
// (tracing-subscriber init goes through leviculum_std's
// event_log::install_global_subscriber so the LEVICULUM_EVENT_LOG
// env var is honoured for structured-event capture.  Codeberg #39
// piece 1.)

use leviculum_std::config::Config;
use leviculum_std::Reticulum;

#[derive(Parser, Debug)]
#[command(name = "lnsd")]
#[command(author, version = env!("LEVICULUM_VERSION"), about = "Reticulum network daemon")]
struct Args {
    /// Path to Reticulum config directory (like Python rnsd --config)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Storage directory path (default: <config_dir>/storage).
    ///
    /// Long-only: rnsd's `-s` means `--service`, so `-s` is reserved for that
    /// and storage keeps only its long spelling (a Leviculum extension).
    #[arg(long)]
    storage: Option<PathBuf>,

    /// Running as a service (rnsd `-s/--service`). Recognised for drop-in
    /// compatibility with `rnsd -s`. lnsd keeps logging to stdout, which
    /// systemd/journald captures; it does not redirect to a log file.
    #[arg(short, long)]
    service: bool,

    /// Print an example configuration to stdout and exit (rnsd
    /// `--exampleconfig`). The output loads through lnsd's own config loader.
    #[arg(long)]
    exampleconfig: bool,

    /// Increase log verbosity (repeat for more: -v debug, -vv trace)
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,

    /// Decrease log verbosity (repeat for less: -q warn, -qq error)
    #[arg(short, long, action = ArgAction::Count)]
    quiet: u8,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Display, not Debug. Returning `Err` from `main` prints the
            // Debug form, which turned the shared-instance name clash into
            // `Error: Io(Os { code: 98, kind: AddrInUse, .. })` in the
            // journal — a struct dump where the operator needed a sentence.
            eprintln!("lnsd: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // --exampleconfig prints a config template and exits, before any logging
    // or stack init (rnsd.py:75-77).
    if args.exampleconfig {
        print!("{}", example_config());
        return Ok(());
    }

    // --config is a directory (like Python rnsd), config file is {dir}/config.
    // Resolved before the subscriber so we can peek the config loglevel.
    let config_dir = args.config.unwrap_or_else(Config::default_config_dir);
    let config_file = config_dir.join("config");
    let storage_path = args.storage.unwrap_or_else(|| config_dir.join("storage"));

    // Log-level precedence (matches rnsd, CLI overrides config):
    //   RUST_LOG > CLI -v/-q (when non-zero) > config [logging] loglevel > info.
    // RUST_LOG is applied by EnvFilter inside install_global_subscriber; here we
    // peek the config loglevel BEFORE installing the subscriber so it can seed
    // the default filter when no RUST_LOG / CLI verbosity is given.
    let rust_log_present = std::env::var("RUST_LOG").is_ok();
    let config_loglevel = if config_file.exists() {
        Config::peek_loglevel(&config_file)
    } else {
        None
    };
    let default_filter =
        resolve_default_filter(config_loglevel, args.verbose, args.quiet, rust_log_present);
    leviculum_std::event_log::install_global_subscriber(&default_filter);

    info!("Starting lnsd v{}", env!("CARGO_PKG_VERSION"));
    if args.service {
        info!("Service mode (-s): logging to stdout for systemd/journald capture");
    }

    info!("Config dir: {}", config_dir.display());
    info!("Config file: {}", config_file.display());
    info!("Storage: {}", storage_path.display());

    // Load and configure Reticulum
    let mut config = if config_file.exists() {
        Config::load(&config_file)?
    } else {
        Config::default()
    };
    config.reticulum.storage_path = Some(storage_path);

    // Resource receive-window policy (Codeberg #85): env var, not a config
    // key, because the config format is shared with Python rnsd.
    let window_policy = leviculum_std::resource_policy::resource_window_policy_from_env();

    let mut rns = Reticulum::with_config_daemon(config, window_policy)?;
    rns.start().await?;

    info!("Reticulum daemon running");

    // Wait for shutdown signal (SIGINT or SIGTERM), dump diagnostics on
    // SIGUSR1, command a firmware reset on SIGUSR2.
    //
    // SIGUSR2 joins an existing surface rather than inventing one: SIGUSR1
    // is already how this daemon is asked to do something out of band, and
    // a signal needs no socket, no authentication story and no wire format
    // to work identically for a daemon in a container (`docker kill
    // --signal=USR2`) and one on the host. The alternative was a verb on
    // the RPC server, which speaks Python `multiprocessing.connection` for
    // rnstatus/rnpath compatibility — a protocol whose whole point is that
    // it mirrors Python-RNS, and which would gain a verb Python-RNS has
    // no idea about.
    //
    // What it does: every firmware board this daemon holds gets the
    // commanded-reset frame on its data port. The daemon is the only
    // process that CAN send it, because it holds the port exclusively for
    // its whole run (periculum #255).
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigusr1 = signal(SignalKind::user_defined1())?;
        let mut sigusr2 = signal(SignalKind::user_defined2())?;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { info!("Received SIGINT"); break; }
                _ = sigterm.recv() => { info!("Received SIGTERM"); break; }
                _ = sigusr1.recv() => {
                    let dump = rns.diagnostic_dump();
                    eprint!("{}", dump);
                }
                _ = sigusr2.recv() => {
                    let reached = leviculum_std::interfaces::request_firmware_reset();
                    // Said out loud in both directions: a daemon holding
                    // no board is a caller that signalled the wrong node,
                    // and silence would read as a reset that happened.
                    info!("Received SIGUSR2: commanded firmware reset on {reached} serial interface(s)");
                }
            }
        }
    }

    info!("Shutting down...");
    rns.stop().await?;

    Ok(())
}

/// Map an RNS log level (0-7) to a tracing env-filter directive.
///
/// RNS levels: 0 critical, 1 error, 2 warning, 3 notice, 4 info, 5 verbose,
/// 6 debug, 7 extreme (reference/Reticulum/RNS/__init__.py:66-73). tracing has
/// no notice/verbose/extreme, so notice folds into info, verbose into debug,
/// and extreme into trace. Values above 7 map to trace.
fn rns_loglevel_to_filter(level: u8) -> &'static str {
    match level {
        0 | 1 => "error",
        2 => "warn",
        3 | 4 => "info",
        5 | 6 => "debug",
        _ => "trace",
    }
}

/// Map the CLI `-v/-q` net verbosity delta to a tracing directive, matching the
/// historical lnsd behaviour.
fn cli_verbosity_to_filter(delta: i8) -> &'static str {
    match delta {
        2.. => "trace",
        1 => "debug",
        0 => "info",
        -1 => "warn",
        _ => "error",
    }
}

/// Select the default tracing filter used when `RUST_LOG` is unset.
///
/// Precedence matches rnsd (CLI overrides config): `RUST_LOG` > CLI `-v/-q`
/// (when non-zero) > config `[logging] loglevel` > default `info`. `RUST_LOG`
/// itself is applied one layer up by `EnvFilter::try_from_default_env`, so when
/// `rust_log_present` is true this returned default is unused by the subscriber;
/// we still avoid letting config override it here.
fn resolve_default_filter(
    loglevel: Option<u8>,
    verbose: u8,
    quiet: u8,
    rust_log_present: bool,
) -> String {
    let delta = (verbose as i8) - (quiet as i8);
    if delta != 0 {
        // Explicit CLI verbosity wins over config loglevel.
        return cli_verbosity_to_filter(delta).to_string();
    }
    if !rust_log_present {
        if let Some(level) = loglevel {
            return rns_loglevel_to_filter(level).to_string();
        }
    }
    "info".to_string()
}

/// A concise, representative example configuration, printed by
/// `--exampleconfig`. It is deliberately short (a `[reticulum]` block, a
/// `[logging]` level, a `[[Default Interface]]` AutoInterface, and a commented
/// TCPClient example) and MUST round-trip through [`Config::load`].
fn example_config() -> &'static str {
    r#"# Example Reticulum configuration for lnsd.
# Edit to add the interfaces and settings you need.

[reticulum]

# Enable Transport to route traffic for other peers and pass announces.
# Recommended for always-on, stationary transport nodes.
enable_transport = No

# Share this instance with other local programs over a local socket.
share_instance = Yes

[logging]

# Log level 0-7: 0 critical, 1 error, 2 warning, 3 notice,
# 4 info (default), 5 verbose, 6 debug, 7 extreme.
loglevel = 4

[interfaces]

  # Communicate with link-local Reticulum peers over UDP/IPv6.
  # Needs no routers or DHCP, only link-local IPv6 (on by default).
  [[Default Interface]]
    type = AutoInterface
    enabled = yes

  # Connect out to a remote TCP server interface. Uncomment and edit
  # target_host / target_port, then set enabled = yes.
  # [[TCP Client Interface]]
  #   type = TCPClientInterface
  #   enabled = no
  #   target_host = 127.0.0.1
  #   target_port = 4242

  # Join the Columba BLE mesh (LNode boards, phones) as a dual-role
  # BlueZ node. Needs bluetoothd and a BLE-capable adapter. Off by
  # default; uncomment and set enabled = yes to use it.
  # [[BLE Interface]]
  #   type = BLEInterface
  #   enabled = no
"#
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        let mut v = vec!["lnsd"];
        v.extend_from_slice(args);
        Args::try_parse_from(v).expect("parse ok")
    }

    #[test]
    fn service_short_and_long_set_flag() {
        assert!(parse(&["-s"]).service);
        assert!(parse(&["--service"]).service);
        assert!(!parse(&[]).service);
    }

    #[test]
    fn storage_is_long_only_and_s_is_not_storage() {
        // --storage still parses to a path.
        let a = parse(&["--storage", "/tmp/store"]);
        assert_eq!(a.storage, Some(PathBuf::from("/tmp/store")));
        assert!(!a.service);

        // -s is the service flag, never storage: `-s /tmp/store` treats
        // /tmp/store as a positional (which lnsd has none of) -> error,
        // and -s itself does not consume a value as storage.
        assert!(parse(&["-s"]).storage.is_none());
        assert!(Args::try_parse_from(["lnsd", "-s", "/tmp/store"]).is_err());
    }

    #[test]
    fn loglevel_precedence_cli_over_config() {
        // Config loglevel applies when neither CLI -v/-q nor RUST_LOG is set.
        assert_eq!(resolve_default_filter(Some(6), 0, 0, false), "debug");
        assert_eq!(resolve_default_filter(Some(2), 0, 0, false), "warn");
        // CLI -v wins over config loglevel.
        assert_eq!(resolve_default_filter(Some(2), 1, 0, false), "debug");
        // CLI -q wins over config loglevel.
        assert_eq!(resolve_default_filter(Some(6), 0, 2, false), "error");
        // RUST_LOG present -> config is not applied (EnvFilter uses RUST_LOG,
        // so the returned default is the CLI/default only).
        assert_eq!(resolve_default_filter(Some(6), 0, 0, true), "info");
        // No config, no CLI -> default info.
        assert_eq!(resolve_default_filter(None, 0, 0, false), "info");
    }

    #[test]
    fn rns_loglevel_maps_to_tracing() {
        assert_eq!(rns_loglevel_to_filter(0), "error");
        assert_eq!(rns_loglevel_to_filter(1), "error");
        assert_eq!(rns_loglevel_to_filter(2), "warn");
        assert_eq!(rns_loglevel_to_filter(3), "info");
        assert_eq!(rns_loglevel_to_filter(4), "info");
        assert_eq!(rns_loglevel_to_filter(5), "debug");
        assert_eq!(rns_loglevel_to_filter(6), "debug");
        assert_eq!(rns_loglevel_to_filter(7), "trace");
    }

    #[test]
    fn exampleconfig_flag_parses() {
        assert!(parse(&["--exampleconfig"]).exampleconfig);
        assert!(!parse(&[]).exampleconfig);
    }

    #[test]
    fn example_config_roundtrips_through_loader() {
        // The printed example must load through our own INI loader without
        // error and yield at least the Default AutoInterface.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config");
        std::fs::write(&path, example_config()).expect("write");

        let config = Config::load(&path).expect("example config must load");
        let auto = config
            .interfaces
            .get("Default Interface")
            .expect("Default Interface present");
        assert_eq!(auto.interface_type, "AutoInterface");
        assert!(auto.enabled);
    }
}
