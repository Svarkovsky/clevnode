//! lncp - Reticulum File Transfer Utility
//!
//! Standalone binary for sending/receiving files over Reticulum,
//! compatible with Python's rncp. Connects to a running daemon
//! (lnsd or rnsd) via shared instance IPC.

use std::path::PathBuf;

use clap::{ArgAction, Parser};
use tracing_subscriber::EnvFilter;

use leviculum_std::config::Config;
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::{Destination, DestinationType, Direction};

mod cp;

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[derive(Parser, Debug)]
#[command(name = "lncp", version = env!("LEVICULUM_VERSION"), about = "Reticulum File Transfer Utility")]
struct Args {
    /// File to send (send mode)
    file: Option<String>,

    /// Destination hash, 32 hex characters (send mode)
    destination: Option<String>,

    /// Path to alternative Reticulum config directory
    #[arg(long)]
    config: Option<PathBuf>,

    /// Increase verbosity
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,

    /// Decrease verbosity
    #[arg(short, long, action = ArgAction::Count)]
    quiet: u8,

    /// Listen for incoming transfer requests
    #[arg(short, long)]
    listen: bool,

    /// Fetch / transfer phase timeout in seconds. Counts after the link
    /// is established (link establishment has its own internal budget).
    /// Default: no timeout — the transfer runs to completion or until
    /// you interrupt it. Slow transports (LoRa) need no artificial cap;
    /// set this only if you want a hard wall-clock bound on the transfer.
    #[arg(short = 'w')]
    timeout: Option<f64>,

    /// Save received files in specified path
    #[arg(short, long)]
    save: Option<PathBuf>,

    /// Allow overwriting received files
    #[arg(short = 'O', long)]
    overwrite: bool,

    /// Accept requests from anyone
    #[arg(short = 'n', long = "no-auth")]
    no_auth: bool,

    /// Announce interval (-1=none, 0=once at startup, N=every N sec)
    #[arg(short = 'b', default_value = "0")]
    announce_interval: i64,

    /// Print identity and destination info and exit
    #[arg(short = 'p', long = "print-identity")]
    print_identity: bool,

    /// Path to identity file to use
    #[arg(short = 'i')]
    identity: Option<PathBuf>,

    /// Fully silent: no progress output and no log output at all
    /// (equivalent to -qq)
    #[arg(short = 'S', long)]
    silent: bool,

    /// Disable automatic compression
    #[arg(short = 'C', long = "no-compress")]
    no_compress: bool,

    /// Fetch file from remote listener
    #[arg(short = 'f', long)]
    fetch: bool,

    /// Allow authenticated clients to fetch files
    #[arg(short = 'F', long = "allow-fetch")]
    allow_fetch: bool,

    /// Restrict fetch requests to specified path
    #[arg(short = 'j', long)]
    jail: Option<PathBuf>,

    /// Display physical layer transfer rates
    #[arg(short = 'P', long = "phy-rates")]
    phy_rates: bool,

    /// Allow identity hash (can be specified multiple times)
    #[arg(short = 'a', action = ArgAction::Append)]
    allowed: Vec<String>,
}

/// Map -v/-q/--silent to (log filter, progress suppression).
///
/// Stacked quietness mirrors Python rncp:
///   (none)  : progress on, logs at "warn" (lncp is a user tool, not a
///             daemon — hence not "info")
///   -q      : progress off, errors only ("error" logs)
///   -qq     : fully silent — progress off AND the log filter off;
///             identical to --silent
///   -v/-vv/… raise the log level as before.
fn output_levels(verbose: u8, quiet: u8, silent: bool) -> (&'static str, bool) {
    if silent || quiet >= 2 {
        return ("off", true);
    }
    let filter = match (verbose as i8) - (quiet as i8) {
        3.. => "trace",
        2 => "debug",
        1 => "info",
        0 => "warn",
        _ => "error",
    };
    (filter, quiet >= 1)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // RUST_LOG env takes precedence; otherwise use -v/-q flags.
    let (default_filter, _) = output_levels(args.verbose, args.quiet, args.silent);
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .init();

    if let Err(e) = run(args).await {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let config_dir = args
        .config
        .clone()
        .unwrap_or_else(Config::default_config_dir);

    // Handle -p/--print-identity (no node needed)
    if args.print_identity {
        return print_identity(&config_dir, args.identity.as_deref());
    }

    // Determine instance name from config
    let instance_name = read_instance_name(&config_dir);

    // Connect to daemon
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        // Resource receive-window policy (Codeberg #85): the window lives in
        // the endpoint process terminating the link, so lncp reads the env
        // var itself rather than inheriting anything from the daemon.
        .resource_window_policy(leviculum_std::resource_policy::resource_window_policy_from_env())
        .connect_to_shared_instance(&instance_name)
        // Safe to share storage path with lnsd: a client with
        // enable_transport(false) writes no paths, announces, or
        // packet hashes to storage. Identity is loaded separately.
        .storage_path(config_dir.join("storage"))
        .build_sync()
        .map_err(|e| daemon_connect_error(&instance_name, &e, args.verbose))?;
    node.start()
        .await
        .map_err(|e| daemon_connect_error(&instance_name, &e, args.verbose))?;
    let mut events = node.take_event_receiver().ok_or("no event receiver")?;

    let (_, progress_quiet) = output_levels(args.verbose, args.quiet, args.silent);

    // Validate flag combinations
    if args.allow_fetch && !args.listen {
        eprintln!("Warning: -F/--allow-fetch only applies in listen mode (-l)");
    }
    if args.jail.is_some() && !args.allow_fetch {
        eprintln!("Warning: -j/--jail only applies with -F/--allow-fetch");
    }

    let result = if args.listen {
        let identity_path = args
            .identity
            .unwrap_or_else(|| config_dir.join("identities").join("lncp"));
        let identity = cp::load_or_generate_identity(&identity_path)?;
        let allowed_identities = parse_identity_hashes(&args.allowed)?;

        if args.no_auth && !allowed_identities.is_empty() {
            eprintln!("Warning: -n/--no-auth overrides -a (accepting all identities)");
        }

        cp::run_listen(
            &node,
            &mut events,
            identity,
            args.save,
            args.overwrite,
            args.no_auth,
            &allowed_identities,
            args.announce_interval,
            args.verbose,
            progress_quiet,
            args.allow_fetch,
            args.jail.clone(),
            args.phy_rates,
        )
        .await
    } else if args.fetch {
        let file = args.file.ok_or("remote file path required in fetch mode")?;
        let dest = args
            .destination
            .ok_or("destination hash required in fetch mode")?;
        let identity_path = args
            .identity
            .unwrap_or_else(|| config_dir.join("identities").join("lncp"));
        let identity = cp::load_or_generate_identity(&identity_path)?;

        cp::run_fetch(
            &node,
            &mut events,
            &file,
            &dest,
            args.save,
            args.overwrite,
            args.timeout,
            args.verbose,
            progress_quiet,
            args.no_compress,
            Some(&identity),
            args.phy_rates,
        )
        .await
    } else {
        let file = args.file.ok_or("file argument required in send mode")?;
        let dest = args
            .destination
            .ok_or("destination argument required in send mode")?;

        // Sender always identifies
        let identity_path = args
            .identity
            .unwrap_or_else(|| config_dir.join("identities").join("lncp"));
        let identity = cp::load_or_generate_identity(&identity_path)?;

        cp::run_send(
            &node,
            &mut events,
            &file,
            &dest,
            args.timeout,
            args.verbose,
            progress_quiet,
            args.no_compress,
            Some(&identity),
            args.phy_rates,
        )
        .await
    };

    node.stop().await?;
    result
}

fn daemon_connect_error(instance_name: &str, error: &dyn std::fmt::Display, verbose: u8) -> String {
    format!(
        "Could not connect to a running Reticulum daemon on rns/{}.\n\
         Start lnsd or rnsd first.{}",
        instance_name,
        if verbose > 0 {
            format!("\nDetail: {}", error)
        } else {
            String::new()
        }
    )
}

fn read_instance_name(config_dir: &std::path::Path) -> String {
    // Config has `instance_name` field (config.rs:39),
    // INI parser handles it (ini_config.rs:152), default is "default".
    let config_file = config_dir.join("config");
    if config_file.exists() {
        if let Ok(config) = Config::load(&config_file) {
            return config.reticulum.instance_name;
        }
    }
    "default".to_string()
}

fn parse_identity_hashes(allowed: &[String]) -> Result<Vec<[u8; 16]>, Box<dyn std::error::Error>> {
    allowed
        .iter()
        .map(|hex_str| {
            let bytes =
                hex_decode(hex_str).map_err(|e| format!("-a: invalid hex '{}': {}", hex_str, e))?;
            if bytes.len() != 16 {
                return Err(format!(
                    "-a: identity hash must be 32 hex chars (16 bytes), got {} chars",
                    hex_str.len()
                )
                .into());
            }
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&bytes);
            Ok(arr)
        })
        .collect()
}

fn print_identity(
    config_dir: &std::path::Path,
    identity_path: Option<&std::path::Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = identity_path
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir.join("identities").join("lncp"));
    let identity = cp::load_or_generate_identity(&path)?;
    let id_hash = hex_encode(identity.hash());
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "rncp",
        &["receive"],
    )
    .map_err(|e| format!("destination error: {e}"))?;
    println!("{}", hex_encode(dest.hash().as_bytes()));
    println!("Identity  : {}", id_hash);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_identity_hashes_valid() {
        let hashes =
            parse_identity_hashes(&["a1b2c3d4e5f6a7b8a1b2c3d4e5f6a7b8".to_string()]).unwrap();
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0][0], 0xa1);
        assert_eq!(hashes[0][15], 0xb8);
    }

    #[test]
    fn test_parse_identity_hashes_multiple() {
        let hashes = parse_identity_hashes(&[
            "a1b2c3d4e5f6a7b8a1b2c3d4e5f6a7b8".to_string(),
            "00112233445566778899aabbccddeeff".to_string(),
        ])
        .unwrap();
        assert_eq!(hashes.len(), 2);
    }

    #[test]
    fn test_parse_identity_hashes_empty() {
        let hashes = parse_identity_hashes(&[]).unwrap();
        assert!(hashes.is_empty());
    }

    #[test]
    fn test_parse_identity_hashes_wrong_length() {
        let result = parse_identity_hashes(&["abcd".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_identity_hashes_invalid_hex() {
        let result = parse_identity_hashes(&["zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_output_levels_quietness_mapping() {
        // default: progress on, warn logs
        assert_eq!(output_levels(0, 0, false), ("warn", false));
        // -q: progress off, errors only
        assert_eq!(output_levels(0, 1, false), ("error", true));
        // -qq: fully silent
        assert_eq!(output_levels(0, 2, false), ("off", true));
        // --silent ≡ -qq, regardless of -v
        assert_eq!(output_levels(0, 0, true), ("off", true));
        assert_eq!(output_levels(2, 0, true), ("off", true));
        // verbosity unchanged
        assert_eq!(output_levels(1, 0, false), ("info", false));
        assert_eq!(output_levels(2, 0, false), ("debug", false));
        assert_eq!(output_levels(3, 0, false), ("trace", false));
        // -v -q cancel out for the filter; one -q still mutes progress
        assert_eq!(output_levels(1, 1, false), ("warn", true));
    }
}
