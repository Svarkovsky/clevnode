//! lnprobe - Reticulum Probe Utility
//!
//! Standalone binary for probing the reachability of a destination,
//! compatible with Python's rnprobe (RNS/Utilities/rnprobe.py, 1.3.5).
//! Connects to a running daemon (lnsd or rnsd) via shared instance IPC,
//! sends probe packets and reports round-trip time and hop count from
//! the delivery proofs the probed destination returns.

use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{ArgAction, Parser};
use tracing_subscriber::EnvFilter;

use leviculum_core::constants::DEFAULT_PER_HOP_TIMEOUT;
use leviculum_std::config::Config;
use leviculum_std::driver::{EventReceiver, ReticulumNode, ReticulumNodeBuilder};
use leviculum_std::{Destination, DestinationHash, DestinationType, Direction, NodeEvent};

// Shared with lnstest; each binary uses its own subset of the module.
#[allow(dead_code)]
mod daemon_rpc;

/// rnprobe.py:41
const DEFAULT_PROBE_SIZE: usize = 16;
/// rnprobe.py:42
const DEFAULT_TIMEOUT: f64 = 12.0;

/// rnprobe exit codes: 1 = no path, 2 = loss > 0, 3 = probe exceeds MTU.
const EXIT_NO_PATH: i32 = 1;
const EXIT_LOSS: i32 = 2;
const EXIT_TOO_LARGE: i32 = 3;

#[derive(Parser, Debug)]
#[command(name = "lnprobe", version = env!("LEVICULUM_VERSION"), about = "Reticulum Probe Utility")]
struct Args {
    /// Path to alternative Reticulum config directory
    #[arg(long)]
    config: Option<PathBuf>,

    /// Size of probe packet payload in bytes
    #[arg(short, long)]
    size: Option<usize>,

    /// Number of probes to send
    #[arg(short = 'n', long, default_value = "1")]
    probes: u64,

    /// Timeout before giving up
    #[arg(short, long, value_name = "seconds")]
    timeout: Option<f64>,

    /// Time between each probe
    #[arg(short, long, value_name = "seconds", default_value = "0")]
    wait: f64,

    /// Increase verbosity
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,

    /// Full destination name in dotted notation
    full_name: Option<String>,

    /// Hexadecimal hash of the destination
    destination_hash: Option<String>,
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Python `RNS.prettyhexrep`: `<hex>`.
fn prettyhexrep(bytes: &[u8]) -> String {
    format!("<{}>", hex_encode(bytes))
}

/// Python `str(round(v, ndigits))`: shortest decimal form of the rounded
/// value, but always at least one digit after the point (`12.0`, `530.9`).
fn py_round_str(v: f64, ndigits: usize) -> String {
    let mut s = format!("{v:.ndigits$}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.push('0');
    }
    s
}

/// Python `RNS.Destination.app_and_aspects_from_name` (Destination.py:88):
/// the first dotted component is the app name, the rest are aspects.
fn app_and_aspects(full_name: &str) -> Result<(String, Vec<String>), String> {
    let mut parts = full_name.split('.').map(str::to_string);
    let app_name = parts
        .next()
        .filter(|p| !p.is_empty())
        .ok_or("Invalid destination name")?;
    Ok((app_name, parts.collect()))
}

/// Map -v to a tracing filter. rnprobe consumes the first -v for its own
/// extra per-probe output (via/on, rnprobe.py:69-74) and raises the log
/// level with the rest; the client default is quiet like lncp's.
fn log_filter(verbose: u8) -> &'static str {
    match verbose {
        0 | 1 => "warn",
        2 => "info",
        3 => "debug",
        _ => "trace",
    }
}

/// The animated wait indicator (rnprobe.py:86). Only animated on a TTY so
/// piped output stays clean; the surrounding text is unchanged.
struct Spinner {
    syms: Vec<char>,
    i: usize,
    tty: bool,
}

impl Spinner {
    fn new() -> Self {
        Self {
            syms: "⢄⢂⢁⡁⡈⡐⡠".chars().collect(),
            i: 0,
            tty: std::io::stdout().is_terminal(),
        }
    }

    fn tick(&mut self) {
        if !self.tty {
            return;
        }
        print!("\u{8}\u{8}{} ", self.syms[self.i]);
        let _ = std::io::stdout().flush();
        self.i = (self.i + 1) % self.syms.len();
    }
}

fn main() {
    let args = Args::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(run(args));
    std::process::exit(code);
}

async fn run(args: Args) -> i32 {
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(log_filter(args.verbose))),
        )
        .init();

    let Some(destination_hexhash) = args.destination_hash.as_deref() else {
        // rnprobe prints its help when the hash is missing (rnprobe.py:231).
        use clap::CommandFactory;
        println!();
        let _ = Args::command().print_help();
        println!();
        return EXIT_NO_PATH;
    };
    let Some(full_name) = args.full_name.as_deref() else {
        // Unreachable through clap's positional ordering, but stated like
        // the reference (rnprobe.py:46-48).
        println!("The full destination name including application name aspects must be specified for the destination");
        return EXIT_NO_PATH;
    };
    let (app_name, aspects) = match app_and_aspects(full_name) {
        Ok(parsed) => parsed,
        Err(e) => {
            println!("{e}");
            return EXIT_NO_PATH;
        }
    };

    // rnprobe.py:58-64: 32 hex characters, 16 bytes.
    let dest_hash = match parse_destination_hash(destination_hexhash) {
        Ok(hash) => hash,
        Err(e) => {
            println!("{e}");
            return EXIT_NO_PATH;
        }
    };

    let size = args.size.unwrap_or(DEFAULT_PROBE_SIZE);
    let more_output = args.verbose > 0;

    let config_dir = args
        .config
        .clone()
        .unwrap_or_else(Config::default_config_dir);
    let config = daemon_rpc::load_config(&config_dir);
    let instance_name = daemon_rpc::resolve_instance_name(None, config.as_ref());
    // The authkey is only needed for the informational RPC verbs
    // (first_hop_timeout, next_hop); probing itself runs over the packet
    // path. Missing identity file degrades those verbs, never the probe.
    let authkey = daemon_rpc::resolve_authkey(&config_dir, config.as_ref())
        .ok()
        .map(|(key, _)| key);

    let mut node = match build_client(&instance_name, &config_dir).await {
        Ok(node) => node,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_NO_PATH;
        }
    };
    let Some(mut events) = node.take_event_receiver() else {
        eprintln!("Internal error: event receiver unavailable");
        return EXIT_NO_PATH;
    };

    let code = probe(
        &node,
        &mut events,
        &dest_hash,
        &app_name,
        &aspects,
        size,
        args.probes,
        args.timeout,
        args.wait,
        more_output,
        &instance_name,
        authkey.as_ref(),
    )
    .await;
    let _ = node.stop().await;
    code
}

fn parse_destination_hash(hex: &str) -> Result<DestinationHash, String> {
    if hex.len() != 32 {
        return Err(
            "Destination length is invalid, must be 32 hexadecimal characters (16 bytes).".into(),
        );
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in bytes.iter_mut().enumerate() {
        *chunk = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "Invalid destination entered. Check your input.".to_string())?;
    }
    Ok(DestinationHash::new(bytes))
}

async fn build_client(
    instance_name: &str,
    config_dir: &std::path::Path,
) -> Result<ReticulumNode, String> {
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .connect_to_shared_instance(instance_name)
        // Same sharing rationale as lncp: a transportless client writes no
        // paths or announces to storage.
        .storage_path(config_dir.join("storage"))
        .build_sync()
        .map_err(|e| connect_error(instance_name, &e))?;
    node.start()
        .await
        .map_err(|e| connect_error(instance_name, &e))?;
    Ok(node)
}

fn connect_error(instance_name: &str, error: &dyn std::fmt::Display) -> String {
    format!(
        "Could not connect to a running Reticulum daemon on rns/{instance_name}.\n\
         Start lnsd or rnsd first.\nDetail: {error}"
    )
}

/// Python `reticulum.get_first_hop_timeout(dest)`: over the shared
/// instance this is an RPC to the daemon (Reticulum.py); the daemon
/// scales it with the next-hop interface bitrate. Falls back to the flat
/// `DEFAULT_PER_HOP_TIMEOUT` when the daemon does not answer, which is
/// also what both daemons answer while no path is known.
async fn first_hop_timeout(
    instance_name: &str,
    authkey: Option<&[u8; 32]>,
    dest_hash: &DestinationHash,
) -> f64 {
    let Some(authkey) = authkey else {
        return DEFAULT_PER_HOP_TIMEOUT as f64;
    };
    match leviculum_std::rpc_query_hash_param(
        instance_name,
        authkey,
        "first_hop_timeout",
        "destination_hash",
        dest_hash.as_bytes(),
    )
    .await
    {
        Ok(value) => value.as_f64().unwrap_or(DEFAULT_PER_HOP_TIMEOUT as f64),
        Err(_) => DEFAULT_PER_HOP_TIMEOUT as f64,
    }
}

/// The `-v` "via `<hash>` on `<ifname>`" suffix (rnprobe.py:124-128), asked of
/// the daemon like Python's `get_next_hop` / `get_next_hop_if_name` RPCs.
async fn via_suffix(
    instance_name: &str,
    authkey: Option<&[u8; 32]>,
    dest_hash: &DestinationHash,
) -> String {
    let Some(authkey) = authkey else {
        return String::new();
    };
    let mut more = String::new();
    if let Ok(value) = leviculum_std::rpc_query_hash_param(
        instance_name,
        authkey,
        "next_hop",
        "destination_hash",
        dest_hash.as_bytes(),
    )
    .await
    {
        if let Some(hex) = value.as_str() {
            if let Ok(bytes) = hex_decode(hex) {
                more.push_str(&format!(" via {}", prettyhexrep(&bytes)));
            }
        }
    }
    if let Ok(value) = leviculum_std::rpc_query_hash_param(
        instance_name,
        authkey,
        "next_hop_if_name",
        "destination_hash",
        dest_hash.as_bytes(),
    )
    .await
    {
        if let Some(name) = value.as_str() {
            if name != "None" {
                more.push_str(&format!(" on {name}"));
            }
        }
    }
    more
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

#[allow(clippy::too_many_arguments)]
async fn probe(
    node: &ReticulumNode,
    events: &mut EventReceiver,
    dest_hash: &DestinationHash,
    app_name: &str,
    aspects: &[String],
    size: usize,
    probes: u64,
    timeout: Option<f64>,
    wait: f64,
    more_output: bool,
    instance_name: &str,
    authkey: Option<&[u8; 32]>,
) -> i32 {
    // rnprobe.py:79-95: no path -> request one and wait, bounded by the
    // timeout (or the default plus the daemon's first-hop timeout).
    if !node.has_path(dest_hash) {
        if let Err(e) = node.request_path(dest_hash).await {
            eprintln!("{e}");
            return EXIT_NO_PATH;
        }
        print!(
            "Path to {} requested   ",
            prettyhexrep(dest_hash.as_bytes())
        );
        let _ = std::io::stdout().flush();

        let budget = timeout.unwrap_or(
            DEFAULT_TIMEOUT + first_hop_timeout(instance_name, authkey, dest_hash).await,
        );
        let deadline = Instant::now() + Duration::from_secs_f64(budget.max(0.0));
        let mut spinner = Spinner::new();
        while !node.has_path(dest_hash) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            spinner.tick();
        }
        if !node.has_path(dest_hash) {
            println!("\r{:58}\rPath request timed out", "");
            return EXIT_NO_PATH;
        }
    }

    // rnprobe.py:97-105: recall the announced identity and address the
    // probe at the destination derived from the full name and it.
    let Some(server_identity) = node.get_identity(dest_hash) else {
        eprintln!(
            "Identity for {} not found (no announce received)",
            hex_encode(dest_hash.as_bytes())
        );
        return EXIT_NO_PATH;
    };
    let aspect_refs: Vec<&str> = aspects.iter().map(String::as_str).collect();
    let request_dest = match Destination::new(
        Some(server_identity.clone()),
        Direction::Out,
        DestinationType::Single,
        app_name,
        &aspect_refs,
    ) {
        Ok(dest) => dest,
        Err(e) => {
            println!("{e}");
            return EXIT_NO_PATH;
        }
    };
    let probe_dest_hash = *request_dest.hash();
    if probe_dest_hash != *dest_hash {
        // A wrong full name derives a different hash; like Python, the
        // probe then goes to that derived destination (and times out
        // unless it happens to exist). The recalled identity is what
        // encrypts either way.
        node.remember_identity(probe_dest_hash, server_identity);
    }

    let mut sent: u64 = 0;
    let mut replies: u64 = 0;
    let mut remaining = probes;
    while remaining > 0 {
        if sent > 0 && wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }

        let mut payload = vec![0u8; size];
        {
            use rand_core::RngCore;
            rand_core::OsRng.fill_bytes(&mut payload);
        }

        let sent_at = Instant::now();
        let packet_hash = match node.send_single_packet(&probe_dest_hash, &payload).await {
            Ok(hash) => hash,
            Err(leviculum_std::Error::Send(leviculum_core::SendError::TooLarge)) => {
                println!(
                    "Error: Probe packet size of {} bytes exceed MTU of {} bytes",
                    size,
                    leviculum_core::constants::MTU
                );
                return EXIT_TOO_LARGE;
            }
            Err(e) => {
                eprintln!("{e}");
                return EXIT_NO_PATH;
            }
        };
        sent += 1;

        let more = if more_output {
            via_suffix(instance_name, authkey, dest_hash).await
        } else {
            String::new()
        };
        print!(
            "\rSent probe {} ({} bytes) to {}{}   ",
            sent,
            size,
            prettyhexrep(dest_hash.as_bytes()),
            more
        );
        let _ = std::io::stdout().flush();

        // rnprobe.py:134-142: wait for the proof, bounded like the path wait.
        let budget = timeout.unwrap_or(
            DEFAULT_TIMEOUT + first_hop_timeout(instance_name, authkey, dest_hash).await,
        );
        let deadline = Instant::now() + Duration::from_secs_f64(budget.max(0.0));
        let mut spinner = Spinner::new();
        let delivered = loop {
            let now = Instant::now();
            if now >= deadline {
                break None;
            }
            match tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
                Ok(Some(NodeEvent::PacketDeliveryConfirmed { packet_hash: ph }))
                    if ph == packet_hash =>
                {
                    break Some(sent_at.elapsed());
                }
                Ok(Some(NodeEvent::DeliveryFailed {
                    packet_hash: ph, ..
                })) if ph == packet_hash => {
                    break None;
                }
                Ok(Some(_)) => {}
                Ok(None) => break None,
                Err(_) => spinner.tick(),
            }
        };

        match delivered {
            None => {
                println!("\r{:64}\rProbe timed out", "");
            }
            Some(rtt) => {
                println!("\u{8}\u{8} ");
                let _ = std::io::stdout().flush();
                replies += 1;
                let hops = node.hops_to(dest_hash).unwrap_or(1);
                let ms = if hops != 1 { "s" } else { "" };
                let rtt_secs = rtt.as_secs_f64();
                let rttstring = if rtt_secs >= 1.0 {
                    format!("{} seconds", py_round_str(rtt_secs, 3))
                } else {
                    format!("{} milliseconds", py_round_str(rtt_secs * 1000.0, 3))
                };
                println!(
                    "Valid reply from {}\nRound-trip time is {} over {} hop{}\n",
                    prettyhexrep(probe_dest_hash.as_bytes()),
                    rttstring,
                    hops,
                    ms
                );
            }
        }

        remaining -= 1;
    }

    let loss = (1.0 - (replies as f64 / sent as f64)) * 100.0;
    println!(
        "Sent {}, received {}, packet loss {}%",
        sent,
        replies,
        py_round_str(loss, 2)
    );
    if loss > 0.0 {
        EXIT_LOSS
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_hash_parsing_matches_rnprobe_rules() {
        assert!(parse_destination_hash("a1b2c3d4e5f6a7b8a1b2c3d4e5f6a7b8").is_ok());
        // Wrong length
        assert!(parse_destination_hash("a1b2").is_err());
        // Right length, not hex
        assert!(parse_destination_hash("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
    }

    #[test]
    fn full_names_split_like_app_and_aspects_from_name() {
        let (app, aspects) = app_and_aspects("rnstransport.probe").unwrap();
        assert_eq!(app, "rnstransport");
        assert_eq!(aspects, vec!["probe".to_string()]);

        let (app, aspects) = app_and_aspects("example_utilities").unwrap();
        assert_eq!(app, "example_utilities");
        assert!(aspects.is_empty());

        assert!(app_and_aspects("").is_err());
    }

    #[test]
    fn float_rendering_matches_python_str_of_round() {
        // str(round(12.0, 3)) == "12.0"
        assert_eq!(py_round_str(12.0, 3), "12.0");
        // str(round(530.9004, 3)) == "530.9"
        assert_eq!(py_round_str(530.9004, 3), "530.9");
        // str(round(0.531, 3)) == "0.531"
        assert_eq!(py_round_str(0.531, 3), "0.531");
        // Loss: str(round(0.0, 2)) == "0.0", str(round(33.333.., 2)) == "33.33"
        assert_eq!(py_round_str(0.0, 2), "0.0");
        assert_eq!(py_round_str(100.0 / 3.0, 2), "33.33");
        assert_eq!(py_round_str(100.0, 2), "100.0");
    }
}
