//! `lnstatus` — native Rust drop-in for Python `rnstatus` (Codeberg #86).
//!
//! LOCAL mode drives a running `lnsd` (or Python `rnsd`) over the shared-instance
//! RPC (`interface_stats`, and `link_count` for `-l`), then renders the exact
//! rnstatus per-interface layout. REMOTE mode (`-R/-i/-w`, Stage 3) queries a
//! remote transport instance's status over a link the way Python `rnstatus -R`
//! does; the flow lives in [`leviculum_std::remote_status`] and its result is
//! fed to the same renderer, so remote and local output match. Discovered
//! interfaces (`-d/-D`, #32) read the local discovered-interface registry over
//! the RPC and render the rnstatus discovered layout.
//!
//! Output parity is the point: the same `interface_stats` dict fed to this
//! renderer and to Python rnstatus yields byte-identical output, so a
//! `lnstatus | diff rnstatus` against the same daemon passes. See
//! `lnstatus_render` for the format port and the golden-output tests.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;

use leviculum_std::config::Config;
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::remote_status;
use leviculum_std::Identity;

mod lnstatus_render;
use lnstatus_render::StatusOptions;

/// Reticulum Network Stack Status (drop-in for rnstatus).
#[derive(Parser, Debug)]
#[command(
    name = "lnstatus",
    version = env!("LEVICULUM_VERSION"),
    about = "Reticulum Network Stack Status"
)]
struct Args {
    /// path to alternative Reticulum config directory
    #[arg(long)]
    config: Option<PathBuf>,

    /// show all interfaces
    #[arg(short = 'a', long = "all", default_value_t = false)]
    all: bool,

    /// show announce stats
    #[arg(short = 'A', long = "announce-stats", default_value_t = false)]
    announce_stats: bool,

    /// show path request stats
    #[arg(short = 'P', long = "pr-stats", default_value_t = false)]
    pr_stats: bool,

    /// show link stats
    #[arg(short = 'l', long = "link-stats", default_value_t = false)]
    link_stats: bool,

    /// only show interfaces with active bursts
    #[arg(short = 'B', long = "burst", default_value_t = false)]
    burst: bool,

    /// display traffic totals
    #[arg(short = 't', long = "totals", default_value_t = false)]
    totals: bool,

    /// sort interfaces by [rate, traffic, rx, tx, rxs, txs, announces, arx, atx, prx, ptx, held]
    #[arg(short = 's', long = "sort")]
    sort: Option<String>,

    /// reverse sorting
    #[arg(short = 'r', long = "reverse", default_value_t = false)]
    reverse: bool,

    /// output in JSON format
    #[arg(short = 'j', long = "json", default_value_t = false)]
    json: bool,

    /// add the transport's internal tables to the JSON output (requires -j)
    ///
    /// Additive Leviculum extension (Codeberg #174): `rnstatus` has no
    /// counterpart, so the flag is gated on `-j` rather than changing what any
    /// reference flag does. See `lnstatus_render::merge_transport_tables`.
    #[arg(
        long = "tables",
        default_value_t = false,
        requires = "json",
        conflicts_with_all = ["remote", "discovered", "discovered_details"]
    )]
    tables: bool,

    /// list identities learned from announces, with their derived destinations
    ///
    /// Additive Leviculum extension: `rnstatus` has no counterpart, and the
    /// short form -N collides with none of its letters. Renders one row per
    /// identity the daemon has heard announce, plus the `lxmf.delivery` and
    /// `rnstransport.probe` destinations derived from the identity hash, so a
    /// remote board can be probed without computing its probe destination by
    /// hand. With -j the raw RPC response is printed instead.
    #[arg(
        short = 'N',
        long = "identities",
        default_value_t = false,
        conflicts_with_all = ["remote", "discovered", "discovered_details", "tables", "monitor"]
    )]
    identities: bool,

    /// transport identity hash of remote instance to get status from
    #[arg(short = 'R')]
    remote: Option<String>,

    /// path to identity used for remote management
    #[arg(short = 'i')]
    identity: Option<PathBuf>,

    /// timeout before giving up on remote queries
    #[arg(short = 'w')]
    timeout: Option<f64>,

    /// list discovered interfaces
    #[arg(short = 'd', long = "discovered", default_value_t = false)]
    discovered: bool,

    /// show details and config entries for discovered interfaces
    #[arg(short = 'D', default_value_t = false)]
    discovered_details: bool,

    /// continuously monitor status
    #[arg(short = 'm', long = "monitor", default_value_t = false)]
    monitor: bool,

    /// refresh interval for monitor mode (default: 1)
    #[arg(short = 'I', long = "monitor-interval", default_value_t = 1.0)]
    monitor_interval: f64,

    /// verbose logging (repeatable)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// only display interfaces with names including filter
    filter: Option<String>,

    /// shared-instance name to query (default: from config, else "default")
    #[arg(long)]
    instance_name: Option<String>,
}

impl Args {
    fn status_options(&self) -> StatusOptions {
        StatusOptions {
            dispall: self.all,
            astats: self.announce_stats,
            pstats: self.pr_stats,
            lstats: self.link_stats,
            burst_filter: self.burst,
            totals: self.totals,
            sort: self.sort.clone(),
            reverse: self.reverse,
            name_filter: self.filter.clone(),
        }
    }
}

/// Resolve the daemon's RPC authkey: `SHA256(storage/transport_identity)`.
/// Mirrors the diag path (`leviculum-cli/src/diag.rs:303`).
fn resolve_authkey(config_dir: &Path, config: Option<&Config>) -> Result<[u8; 32], String> {
    let mut candidates: Vec<PathBuf> = vec![config_dir.join("storage").join("transport_identity")];
    if let Some(sp) = config.and_then(|c| c.reticulum.storage_path.as_ref()) {
        candidates.push(sp.join("transport_identity"));
    }
    let mut errors = Vec::new();
    for path in &candidates {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() == 64 => {
                use sha2::Digest;
                let digest = sha2::Sha256::digest(&bytes);
                let mut key = [0u8; 32];
                key.copy_from_slice(&digest);
                return Ok(key);
            }
            Ok(bytes) => errors.push(format!(
                "{}: unexpected size {} (expected 64)",
                path.display(),
                bytes.len()
            )),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    Err(errors.join("; "))
}

/// Fetch `interface_stats` (and `link_count` when `-l`) from the daemon.
async fn fetch_status(
    instance_name: &str,
    authkey: &[u8; 32],
    want_link_count: bool,
) -> Result<(serde_json::Value, Option<i64>), String> {
    let stats = leviculum_std::rpc_query(instance_name, authkey, "interface_stats")
        .await
        .map_err(|e| e.to_string())?;

    let link_count = if want_link_count {
        // rnstatus tolerates link_count failing (rnstatus.py:337-338); mirror that.
        match leviculum_std::rpc_query(instance_name, authkey, "link_count").await {
            Ok(v) => v
                .as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok())),
            Err(_) => None,
        }
    } else {
        None
    };
    Ok((stats, link_count))
}

/// Fetch the `transport_tables` snapshot for `--tables` (Codeberg #174).
///
/// Returns `None` when the daemon does not answer the command — a Python
/// `rnsd`, or an `lnsd` from before #174, both match no arm and close the
/// connection, which surfaces here as a transport error. That is the same
/// tolerance `rnstatus` applies to `link_count` (rnstatus.py:337-338) and the
/// same one `fetch_status` above already mirrors: the status query the user
/// asked for succeeded, so the exit code stays 0 and the stats dict is still
/// printed — just without the additive key. The reason goes to stderr so the
/// omission is never silent.
async fn fetch_transport_tables(
    instance_name: &str,
    authkey: &[u8; 32],
) -> Option<serde_json::Value> {
    match leviculum_std::rpc_query(instance_name, authkey, "transport_tables").await {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!(
                "(daemon did not answer the transport_tables query, omitting \
                 \"{}\" from the output: {e})",
                lnstatus_render::TRANSPORT_TABLES_KEY
            );
            None
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // --- Resolve config dir / instance name. ---
    let config_dir = args
        .config
        .clone()
        .unwrap_or_else(Config::default_config_dir);
    let config_file = config_dir.join("config");
    let loaded_config: Option<Config> = if config_file.exists() {
        Config::load(&config_file).ok()
    } else {
        None
    };
    let instance_name = args
        .instance_name
        .clone()
        .or_else(|| {
            loaded_config
                .as_ref()
                .map(|c| c.reticulum.instance_name.clone())
        })
        .unwrap_or_else(|| "default".to_string());

    // --- Discovered interfaces (-d/-D): read the local registry over the
    // shared-instance RPC and render it. Matches rnstatus, which handles the
    // discovered block before remote management and exits. ---
    if args.discovered || args.discovered_details {
        run_discovered(&args, &config_dir, loaded_config.as_ref(), &instance_name).await;
        return;
    }

    // --- Remote management (-R/-i/-w): query a remote transport instance. ---
    if args.remote.is_some() {
        run_remote(&args, &config_dir, &instance_name).await;
        return;
    }

    // --- Identity listing (-N/--identities): its own mode, like -d. ---
    if args.identities {
        run_identities(&args, &config_dir, loaded_config.as_ref(), &instance_name).await;
        return;
    }

    // --- Local mode: resolve authkey and drive the shared-instance RPC. ---
    let authkey = match resolve_authkey(&config_dir, loaded_config.as_ref()) {
        Ok(k) => k,
        Err(msg) => {
            eprintln!("No shared RNS instance available to get status from");
            eprintln!("(cannot derive RPC authkey: {msg})");
            std::process::exit(1);
        }
    };

    let opts = args.status_options();

    if args.monitor {
        run_monitor(&instance_name, &authkey, &args, &opts).await;
        return;
    }

    match fetch_status(&instance_name, &authkey, args.link_stats).await {
        Ok((mut stats, link_count)) => {
            if args.json {
                if args.tables {
                    let tables = fetch_transport_tables(&instance_name, &authkey).await;
                    lnstatus_render::merge_transport_tables(&mut stats, tables);
                }
                println!("{}", lnstatus_render::render_json(&stats));
            } else {
                print!(
                    "{}",
                    lnstatus_render::render_status(&stats, link_count, &opts)
                );
            }
        }
        Err(e) => {
            eprintln!("Could not get RNS status");
            eprintln!("({e})");
            std::process::exit(2);
        }
    }
}

/// `-d/--discovered` (list) and `-D` (details + config entries): read the
/// discovered-interface registry from the local shared instance over the RPC
/// and render it. Mirrors `rnstatus -d/-D`, which reads the local storage
/// directly; we go through the RPC so the daemon owns the storage lifecycle.
async fn run_discovered(
    args: &Args,
    config_dir: &Path,
    config: Option<&Config>,
    instance_name: &str,
) {
    let authkey = match resolve_authkey(config_dir, config) {
        Ok(k) => k,
        Err(msg) => {
            eprintln!("No shared RNS instance available to get status from");
            eprintln!("(cannot derive RPC authkey: {msg})");
            std::process::exit(1);
        }
    };

    let list =
        match leviculum_std::rpc_query(instance_name, &authkey, "discovered_interfaces").await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Could not get RNS status");
                eprintln!("({e})");
                std::process::exit(2);
            }
        };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    if args.json {
        println!("{}", lnstatus_render::render_json(&list));
    } else {
        // -D selects the detailed layout with config entries; -d the summary.
        let details = args.discovered_details;
        print!(
            "{}",
            lnstatus_render::render_discovered(&list, details, args.filter.as_deref(), now)
        );
    }
}

/// `-N/--identities`: fetch the `identities` listing over the shared-instance
/// RPC and render it (see `lnstatus_render::render_identities`). A daemon that
/// does not know the verb (Python `rnsd`, or an older `lnsd`) closes the
/// connection without a reply; unlike the additive `--tables` key there is no
/// other output to fall back to here, so that is a hard error, exit 2 — the
/// same exit the status query uses when the daemon cannot be reached.
async fn run_identities(
    args: &Args,
    config_dir: &Path,
    config: Option<&Config>,
    instance_name: &str,
) {
    let authkey = match resolve_authkey(config_dir, config) {
        Ok(k) => k,
        Err(msg) => {
            eprintln!("No shared RNS instance available to get status from");
            eprintln!("(cannot derive RPC authkey: {msg})");
            std::process::exit(1);
        }
    };

    let list = match leviculum_std::rpc_query(instance_name, &authkey, "identities").await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Could not get identity list from the daemon");
            eprintln!("({e})");
            std::process::exit(2);
        }
    };

    if args.json {
        println!("{}", lnstatus_render::render_json(&list));
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        print!("{}", lnstatus_render::render_identities(&list, now));
    }
}

/// Default remote-query timeout, matching Python's
/// `RNS.Transport.PATH_REQUEST_TIMEOUT` (15 s, `Transport.py:79`).
const DEFAULT_REMOTE_TIMEOUT_SECS: f64 = 15.0;

/// `-R/-i/-w`: query a remote transport instance's status over a link, the way
/// Python `rnstatus -R` does. Connects to the local shared instance for
/// transport (like `lncp`), then drives the remote flow in
/// `leviculum_std::remote_status::fetch_remote_status`.
async fn run_remote(args: &Args, config_dir: &Path, instance_name: &str) {
    // -R is required to reach this path; -i (management identity) is mandatory,
    // matching Python (rnstatus.py:313).
    let remote_hex = args.remote.as_deref().unwrap_or_default();
    let Some(identity_path) = args.identity.as_ref() else {
        eprintln!(
            "Remote management requires an identity file. Use -i to specify the path \
             to a management identity."
        );
        std::process::exit(20);
    };

    // -R is a 16-byte (32 hex char) transport identity hash.
    let identity_hash = match parse_identity_hash(remote_hex) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(20);
        }
    };

    // Load the management identity from its RNS identity file (64 raw private
    // key bytes), matching `RNS.Identity.from_file`.
    let identity = match std::fs::read(identity_path)
        .map_err(|e| e.to_string())
        .and_then(|b| Identity::from_private_key_bytes(&b).map_err(|e| e.to_string()))
    {
        Ok(id) => id,
        Err(e) => {
            eprintln!(
                "Could not load management identity from {}: {e}",
                identity_path.display()
            );
            std::process::exit(20);
        }
    };

    let timeout = Duration::from_secs_f64(
        args.timeout
            .filter(|t| *t > 0.0)
            .unwrap_or(DEFAULT_REMOTE_TIMEOUT_SECS),
    );

    // Connect to the shared instance for transport (no local authkey needed;
    // this uses the shared-instance IPC socket, like `lncp`).
    let mut node = match ReticulumNodeBuilder::new()
        .enable_transport(false)
        .connect_to_shared_instance(instance_name)
        .storage_path(config_dir.join("storage"))
        .build_sync()
    {
        Ok(n) => n,
        Err(e) => {
            eprintln!("No shared RNS instance available to get status from");
            eprintln!("({e})");
            std::process::exit(1);
        }
    };
    if let Err(e) = node.start().await {
        eprintln!("No shared RNS instance available to get status from");
        eprintln!("({e})");
        std::process::exit(1);
    }
    let mut events = match node.take_event_receiver() {
        Some(rx) => rx,
        None => {
            eprintln!("Internal error: event receiver unavailable");
            std::process::exit(1);
        }
    };

    let opts = args.status_options();
    let quiet = args.json;

    loop {
        match remote_status::fetch_remote_status(
            &node,
            &mut events,
            &identity_hash,
            &identity,
            args.link_stats,
            timeout,
            quiet,
        )
        .await
        {
            Ok((stats, link_count)) => {
                let rendered = if args.json {
                    lnstatus_render::render_json(&stats) + "\n"
                } else {
                    lnstatus_render::render_status(&stats, link_count, &opts)
                };
                if args.monitor {
                    print!("\x1b[H\x1b[2J{rendered}");
                    use std::io::Write as _;
                    let _ = std::io::stdout().flush();
                } else {
                    print!("{rendered}");
                    return;
                }
            }
            Err(e) => {
                eprintln!("{e}");
                if !args.monitor {
                    std::process::exit(20);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs_f64(args.monitor_interval.max(0.2))).await;
    }
}

/// Parse a 32-hex-char transport identity hash into 16 bytes.
fn parse_identity_hash(hex: &str) -> Result<[u8; 16], String> {
    if hex.len() != 32 {
        return Err(format!(
            "Destination length is invalid, must be 32 hexadecimal characters (16 bytes), got {}.",
            hex.len()
        ));
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s =
            std::str::from_utf8(chunk).map_err(|_| "Invalid destination entered.".to_string())?;
        out[i] =
            u8::from_str_radix(s, 16).map_err(|_| "Invalid destination entered.".to_string())?;
    }
    Ok(out)
}

/// `-m/--monitor`: clear the screen and re-render on each interval. The redraw
/// uses the same single-shot render as the one-off path (rnstatus buffers its
/// output then clears + prints; this matches that behaviour).
async fn run_monitor(instance_name: &str, authkey: &[u8; 32], args: &Args, opts: &StatusOptions) {
    let interval = args.monitor_interval.max(0.2);
    loop {
        let rendered = match fetch_status(instance_name, authkey, args.link_stats).await {
            Ok((mut stats, link_count)) => {
                if args.json {
                    if args.tables {
                        let tables = fetch_transport_tables(instance_name, authkey).await;
                        lnstatus_render::merge_transport_tables(&mut stats, tables);
                    }
                    lnstatus_render::render_json(&stats) + "\n"
                } else {
                    lnstatus_render::render_status(&stats, link_count, opts)
                }
            }
            Err(e) => format!("Could not get RNS status\n({e})\n"),
        };
        // ANSI: cursor home + clear screen (rnstatus.py:738).
        print!("\x1b[H\x1b[2J{rendered}");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Args {
        let mut v = vec!["lnstatus"];
        v.extend_from_slice(args);
        Args::try_parse_from(v).expect("parse ok")
    }

    #[test]
    fn defaults_are_all_false() {
        let a = parse(&[]);
        let o = a.status_options();
        assert!(!o.dispall && !o.astats && !o.pstats && !o.lstats);
        assert!(!o.burst_filter && !o.totals && !o.reverse);
        assert!(o.sort.is_none() && o.name_filter.is_none());
        assert!(!a.json && !a.monitor && !a.discovered);
        assert_eq!(a.monitor_interval, 1.0);
    }

    #[test]
    fn short_flags_map_to_options() {
        let o = parse(&["-a"]).status_options();
        assert!(o.dispall);
        assert!(parse(&["-A"]).status_options().astats);
        assert!(parse(&["-P"]).status_options().pstats);
        assert!(parse(&["-l"]).status_options().lstats);
        assert!(parse(&["-B"]).status_options().burst_filter);
        assert!(parse(&["-t"]).status_options().totals);
        assert!(parse(&["-r"]).status_options().reverse);
        assert!(parse(&["-j"]).json);
        assert!(parse(&["-m"]).monitor);
        assert!(parse(&["-d"]).discovered);
        assert!(parse(&["-D"]).discovered_details);
    }

    #[test]
    fn long_flags_map_to_options() {
        assert!(parse(&["--all"]).status_options().dispall);
        assert!(parse(&["--announce-stats"]).status_options().astats);
        assert!(parse(&["--pr-stats"]).status_options().pstats);
        assert!(parse(&["--link-stats"]).status_options().lstats);
        assert!(parse(&["--burst"]).status_options().burst_filter);
        assert!(parse(&["--totals"]).status_options().totals);
        assert!(parse(&["--reverse"]).status_options().reverse);
        assert!(parse(&["--json"]).json);
        assert!(parse(&["--monitor"]).monitor);
        assert!(parse(&["--discovered"]).discovered);
    }

    #[test]
    fn sort_and_reverse() {
        let a = parse(&["-s", "rate", "-r"]);
        let o = a.status_options();
        assert_eq!(o.sort.as_deref(), Some("rate"));
        assert!(o.reverse);
        assert_eq!(
            parse(&["--sort", "traffic"])
                .status_options()
                .sort
                .as_deref(),
            Some("traffic")
        );
    }

    #[test]
    fn positional_filter_and_combo() {
        let a = parse(&["-A", "-P", "eth0"]);
        let o = a.status_options();
        assert!(o.astats && o.pstats);
        assert_eq!(o.name_filter.as_deref(), Some("eth0"));
    }

    #[test]
    fn monitor_interval_parses_float() {
        let a = parse(&["-m", "-I", "2.5"]);
        assert!(a.monitor);
        assert_eq!(a.monitor_interval, 2.5);
    }

    #[test]
    fn remote_and_config_options() {
        let a = parse(&["-R", "abcdef", "-i", "/tmp/id", "-w", "3.0"]);
        assert_eq!(a.remote.as_deref(), Some("abcdef"));
        assert_eq!(a.identity.as_deref(), Some(std::path::Path::new("/tmp/id")));
        assert_eq!(a.timeout, Some(3.0));
        assert_eq!(
            parse(&["--config", "/etc/reticulum"]).config.as_deref(),
            Some(std::path::Path::new("/etc/reticulum"))
        );
        assert_eq!(
            parse(&["--instance-name", "foo"]).instance_name.as_deref(),
            Some("foo")
        );
    }

    #[test]
    fn identity_hash_parsing() {
        let h = super::parse_identity_hash("000102030405060708090a0b0c0d0e0f").unwrap();
        assert_eq!(h, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        // Wrong length and non-hex are rejected.
        assert!(super::parse_identity_hash("abcd").is_err());
        assert!(super::parse_identity_hash(&"z".repeat(32)).is_err());
    }

    // Codeberg #174: --tables is additive. It is off unless asked for, it
    // cannot be asked for without -j (so it can never change what a reference
    // flag prints), and it is refused in the two modes that answer from a
    // different source than the local transport.
    #[test]
    fn tables_flag_is_additive_and_gated() {
        assert!(!parse(&[]).tables, "off by default");
        assert!(!parse(&["-j"]).tables, "-j alone stays exactly what it was");
        assert!(parse(&["-j", "--tables"]).tables);
        assert!(parse(&["--json", "--tables"]).tables);
        // Without -j there is nothing to add the key to.
        assert!(Args::try_parse_from(["lnstatus", "--tables"]).is_err());
        // No short flag, and it must not be reachable by prefixing -t/--totals.
        assert!(Args::try_parse_from(["lnstatus", "-j", "-T"]).is_err());
        assert!(!parse(&["-t"]).tables && parse(&["-t"]).totals);
        // Remote (-R) and the discovered registry (-d/-D) answer from
        // somewhere other than the local transport's tables.
        assert!(
            Args::try_parse_from(["lnstatus", "-j", "--tables", "-R", &"a".repeat(32)]).is_err()
        );
        assert!(Args::try_parse_from(["lnstatus", "-j", "--tables", "-d"]).is_err());
        assert!(Args::try_parse_from(["lnstatus", "-j", "--tables", "-D"]).is_err());
    }

    // -N/--identities is its own local-transport mode: off by default,
    // reachable by short and long form, refused in the modes that answer from
    // a different source (remote, discovered) and alongside --tables/-m.
    #[test]
    fn identities_flag_modes_and_conflicts() {
        assert!(!parse(&[]).identities, "off by default");
        assert!(parse(&["-N"]).identities);
        assert!(parse(&["--identities"]).identities);
        assert!(parse(&["-N", "-j"]).json, "-j selects the raw RPC output");
        assert!(
            Args::try_parse_from(["lnstatus", "-N", "-R", &"a".repeat(32)]).is_err(),
            "remote mode answers from elsewhere"
        );
        assert!(Args::try_parse_from(["lnstatus", "-N", "-d"]).is_err());
        assert!(Args::try_parse_from(["lnstatus", "-N", "-D"]).is_err());
        assert!(Args::try_parse_from(["lnstatus", "-N", "-j", "--tables"]).is_err());
        assert!(Args::try_parse_from(["lnstatus", "-N", "-m"]).is_err());
    }

    #[test]
    fn verbose_counts() {
        assert_eq!(parse(&["-v"]).verbose, 1);
        assert_eq!(parse(&["-vvv"]).verbose, 3);
        assert_eq!(parse(&[]).verbose, 0);
    }

    #[test]
    fn bad_input_errors() {
        // Non-numeric -w / -I, and unknown flag, must be rejected.
        assert!(Args::try_parse_from(["lnstatus", "-w", "notanumber"]).is_err());
        assert!(Args::try_parse_from(["lnstatus", "-I", "abc"]).is_err());
        assert!(Args::try_parse_from(["lnstatus", "--nope"]).is_err());
        // Too many positionals (filter takes exactly one).
        assert!(Args::try_parse_from(["lnstatus", "a", "b"]).is_err());
    }
}
