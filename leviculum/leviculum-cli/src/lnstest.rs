//! lnstest - Reticulum test and diagnostics tool
//!
//! This provides various utility commands for interacting with Reticulum.
//! Equivalent to rnstatus, rnpath, etc. in the Python implementation.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use tokio::io::AsyncBufReadExt;

mod daemon_rpc;
mod diag;
mod selftest;

use leviculum_std::driver::{EventReceiver, LinkHandle, PacketSender, ReticulumNodeBuilder};
use leviculum_std::{
    Destination, DestinationHash, DestinationType, Direction, Identity, LinkId, NodeEvent,
};

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
        return Err("odd length hex string".to_string());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// Display app_data as a human-readable string.
///
/// Some Python Reticulum apps (LXMF, Sideband, NomadNet) encode app_data
/// as a msgpack structure, typically `[display_name, stamp]` or
/// `[stamp, display_name]`. This function tries to extract the display
/// name from such structures, falling back to UTF-8 lossy conversion.
fn display_app_data(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    if let Some(s) = try_parse_app_data(data) {
        return s;
    }
    // Unwrap a top-level msgpack str/bin wrapper and retry
    if let Some((inner, _)) = read_msgpack_text(data) {
        if !inner.is_empty() {
            // Inner content is valid UTF-8 text, try parsing it as nested msgpack
            if let Some(s) = try_parse_app_data(inner.as_bytes()) {
                return s;
            }
            return inner;
        }
    } else if let Some((inner_bytes, _)) = read_msgpack_bin_raw(data) {
        if let Some(s) = try_parse_app_data(inner_bytes) {
            return s;
        }
    }
    // Plain UTF-8
    if let Ok(s) = core::str::from_utf8(data) {
        return s.to_string();
    }
    // Last resort: find the longest printable ASCII run in raw bytes
    longest_printable_run(data)
}

/// Try parsing data as a msgpack container (array or map) and extract the display name.
fn try_parse_app_data(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    // Try msgpack fixarray: 0x90..0x9f = array of 0..15 elements
    if (data[0] & 0xf0) == 0x90 {
        let count = (data[0] & 0x0f) as usize;
        if count >= 1 {
            if let Some(s) = find_display_name_in_elements(&data[1..], count) {
                return Some(s);
            }
        }
    }
    // Try msgpack fixmap: 0x80..0x8f = map of 0..15 entries, scan values
    if (data[0] & 0xf0) == 0x80 {
        let count = (data[0] & 0x0f) as usize;
        if count >= 1 {
            if let Some(s) = find_display_name_in_map(&data[1..], count) {
                return Some(s);
            }
        }
    }
    None
}

/// Read a msgpack bin element, returning raw bytes (not requiring UTF-8).
fn read_msgpack_bin_raw(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&tag, rest) = data.split_first()?;
    match tag {
        0xc4 => {
            let (&len, rest) = rest.split_first()?;
            let len = len as usize;
            Some((rest.get(..len)?, rest.get(len..)?))
        }
        0xc5 => {
            if rest.len() < 2 {
                return None;
            }
            let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            let rest = &rest[2..];
            Some((rest.get(..len)?, rest.get(len..)?))
        }
        _ => None,
    }
}

/// Scan `count` sequential msgpack elements for the best display name candidate.
/// Returns the longest valid UTF-8 text found among str/bin elements.
fn find_display_name_in_elements(mut data: &[u8], count: usize) -> Option<String> {
    let mut best: Option<String> = None;

    for _ in 0..count {
        if data.is_empty() {
            break;
        }
        // Try reading this element as text (str or bin that's valid UTF-8)
        if let Some((text, rest)) = read_msgpack_text(data) {
            if !text.is_empty() {
                let dominated = best.as_ref().is_some_and(|b| b.len() >= text.len());
                if !dominated {
                    best = Some(text);
                }
            }
            data = rest;
        } else if let Some(rest) = skip_msgpack_element(data) {
            data = rest;
        } else {
            break;
        }
    }
    best
}

/// Scan a msgpack fixmap's values for the best display name candidate.
/// Skips keys (short labels), only checks values for valid UTF-8 text.
fn find_display_name_in_map(mut data: &[u8], count: usize) -> Option<String> {
    let mut best: Option<String> = None;

    for _ in 0..count {
        if data.is_empty() {
            break;
        }
        // Skip the key
        if let Some(rest) = skip_msgpack_element(data) {
            data = rest;
        } else {
            break;
        }
        if data.is_empty() {
            break;
        }
        // Try reading the value as text
        if let Some((text, rest)) = read_msgpack_text(data) {
            if !text.is_empty() {
                let dominated = best.as_ref().is_some_and(|b| b.len() >= text.len());
                if !dominated {
                    best = Some(text);
                }
            }
            data = rest;
        } else if let Some(rest) = skip_msgpack_element(data) {
            data = rest;
        } else {
            break;
        }
    }
    best
}

/// Find the longest run of printable ASCII (0x20..0x7e) in raw bytes.
/// Returns the run if >= 3 characters, otherwise empty string.
fn longest_printable_run(data: &[u8]) -> String {
    let mut best_start = 0;
    let mut best_len = 0;
    let mut start = 0;
    let mut len = 0;

    for (i, &b) in data.iter().enumerate() {
        if (0x20..=0x7e).contains(&b) {
            if len == 0 {
                start = i;
            }
            len += 1;
        } else {
            if len > best_len {
                best_start = start;
                best_len = len;
            }
            len = 0;
        }
    }
    if len > best_len {
        best_start = start;
        best_len = len;
    }

    if best_len >= 3 {
        // All bytes are 0x20-0x7e so this is valid ASCII/UTF-8
        String::from_utf8_lossy(&data[best_start..best_start + best_len]).into_owned()
    } else {
        String::new()
    }
}

/// Try to read a msgpack string or bin element as UTF-8 text.
/// Returns (text, remaining_bytes) on success.
fn read_msgpack_text(data: &[u8]) -> Option<(String, &[u8])> {
    let (&tag, rest) = data.split_first()?;
    let (bytes, remaining) = match tag {
        // fixstr (0xa0..0xbf): length in lower 5 bits
        0xa0..=0xbf => {
            let len = (tag & 0x1f) as usize;
            (rest.get(..len)?, rest.get(len..)?)
        }
        // str8 (0xd9): 1-byte length
        0xd9 => {
            let (&len, rest) = rest.split_first()?;
            let len = len as usize;
            (rest.get(..len)?, rest.get(len..)?)
        }
        // str16 (0xda): 2-byte length
        0xda => {
            if rest.len() < 2 {
                return None;
            }
            let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            let rest = &rest[2..];
            (rest.get(..len)?, rest.get(len..)?)
        }
        // bin8 (0xc4): 1-byte length, try as UTF-8
        0xc4 => {
            let (&len, rest) = rest.split_first()?;
            let len = len as usize;
            (rest.get(..len)?, rest.get(len..)?)
        }
        // bin16 (0xc5): 2-byte length, try as UTF-8
        0xc5 => {
            if rest.len() < 2 {
                return None;
            }
            let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            let rest = &rest[2..];
            (rest.get(..len)?, rest.get(len..)?)
        }
        _ => return None,
    };
    let text = core::str::from_utf8(bytes).ok()?;
    Some((text.to_string(), remaining))
}

/// Skip one msgpack element, returning the remaining bytes.
fn skip_msgpack_element(data: &[u8]) -> Option<&[u8]> {
    let (&tag, rest) = data.split_first()?;
    match tag {
        // positive fixint (0x00..0x7f)
        0x00..=0x7f => Some(rest),
        // negative fixint (0xe0..0xff)
        0xe0..=0xff => Some(rest),
        // nil, false, true
        0xc0..=0xc3 => Some(rest),
        // fixstr (0xa0..0xbf)
        0xa0..=0xbf => rest.get((tag & 0x1f) as usize..),
        // fixmap (0x80..0x8f): skip 2*N elements
        0x80..=0x8f => {
            let count = ((tag & 0x0f) as usize) * 2;
            let mut d = rest;
            for _ in 0..count {
                d = skip_msgpack_element(d)?;
            }
            Some(d)
        }
        // fixarray (0x90..0x9f): skip N elements
        0x90..=0x9f => {
            let count = (tag & 0x0f) as usize;
            let mut d = rest;
            for _ in 0..count {
                d = skip_msgpack_element(d)?;
            }
            Some(d)
        }
        // bin8 (0xc4)
        0xc4 => {
            let (&len, rest) = rest.split_first()?;
            rest.get(len as usize..)
        }
        // bin16 (0xc5)
        0xc5 => {
            if rest.len() < 2 {
                return None;
            }
            let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            rest.get(2 + len..)
        }
        // str8 (0xd9)
        0xd9 => {
            let (&len, rest) = rest.split_first()?;
            rest.get(len as usize..)
        }
        // str16 (0xda)
        0xda => {
            if rest.len() < 2 {
                return None;
            }
            let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            rest.get(2 + len..)
        }
        // uint8, int8
        0xcc | 0xd0 => rest.get(1..),
        // uint16, int16
        0xcd | 0xd1 => rest.get(2..),
        // uint32, int32, float32
        0xce | 0xd2 | 0xca => rest.get(4..),
        // uint64, int64, float64
        0xcf | 0xd3 | 0xcb => rest.get(8..),
        _ => None,
    }
}

#[derive(Parser, Debug)]
#[command(name = "lnstest")]
#[command(author, version = env!("LEVICULUM_VERSION"), about = "Reticulum test and diagnostics tool")]
struct Args {
    /// Config directory of the daemon to ask (the one `lnsd --config` was
    /// given). `diag` reads its config and identity from here; `selftest`
    /// uses it to ask that daemon for the link profile its drain windows are
    /// sized from. Without it, `selftest` falls back to fixed waits.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Corrupt ~1 byte per N bytes on TCP write (fault injection).
    /// For `selftest`, corruption is deferred until after Phase-2
    /// mutual discovery completes, so announces cross a clean stream.
    #[arg(long, global = true)]
    corrupt_every: Option<u64>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Identity management
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },

    /// Run integration self-test through relay node(s)
    Selftest {
        /// Address(es) of relay node(s) (host:port). One or two addresses.
        #[arg(num_args = 1..=2)]
        targets: Vec<String>,
        /// Test duration in seconds
        #[arg(long, default_value = "180")]
        duration: u64,
        /// Messages per second per direction
        #[arg(long, default_value = "1")]
        rate: f64,
        /// Which test phases to run: all, link, packet, ratchet-basic, ratchet-enforced, bulk-transfer, ratchet-rotation
        #[arg(long, default_value = "all")]
        mode: String,
        /// Discovery timeout in seconds (Phase 2: mutual path discovery)
        #[arg(long, default_value = "60")]
        discovery_timeout: u64,
    },

    /// Interactive session: connect to rnsd and enter command loop
    Connect {
        /// Address of the rnsd to connect to (host:port)
        addr: String,

        /// Path to identity file (default: generate ephemeral)
        #[arg(long)]
        identity: Option<PathBuf>,
    },

    /// Collect a diagnostic bundle from a running lnsd (or rnsd) for bug reports
    ///
    /// Queries the shared-instance RPC for the daemon's live view (interface
    /// stats, path table, link count), bundles it with the secret-redacted
    /// config, versions, and system info. Prints to stdout by default.
    /// Secrets (IFAC passphrase/networkname) are redacted; the identity
    /// private key is never read or emitted. Unsupported queries (e.g. against
    /// rnsd) degrade gracefully instead of failing.
    Diag {
        /// Write the bundle to this path instead of stdout
        #[arg(long)]
        output: Option<PathBuf>,

        /// Shared-instance name to query (default: from config, else "default")
        #[arg(long)]
        instance_name: Option<String>,

        /// Tail this structured event-log file into the bundle
        #[arg(long)]
        event_log: Option<PathBuf>,

        /// Skip the daemon RPC queries; emit only config / versions / system
        #[arg(long)]
        no_rpc: bool,
    },
}

#[derive(Subcommand, Debug)]
enum IdentityAction {
    /// Generate a new identity
    Generate {
        /// Output file path
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Show identity information
    Show {
        /// Identity file path
        path: PathBuf,
    },
}

struct AnnounceInfo {
    app_data: String,
    hops: Option<u8>,
    /// X25519(32) + Ed25519(32), needed for /link
    public_key: [u8; 64],
}

struct SessionState {
    discovered: BTreeMap<String, AnnounceInfo>,
    active_link_id: Option<LinkId>,
    packet_sender: Option<PacketSender>,
    show_announces: bool,
}

fn print_prompt() {
    print!("> ");
    let _ = std::io::stdout().flush();
}

fn print_help() {
    println!("Commands:");
    println!("  /peers           List discovered destinations");
    println!("  /link <hash>     Initiate link to destination (32-char hex)");
    println!("  /target <hash>   Set single-packet destination (32-char hex)");
    println!("  /untarget        Clear single-packet target");
    println!("  /send <msg>      Send data on active link or to target");
    println!("  /close           Close active link");
    println!("  /announce        Re-announce this destination");
    println!("  /quiet           Hide announce/path messages");
    println!("  /verbose         Show announce/path messages");
    println!("  /status          Show node status");
    println!("  /help            Show this help");
    println!("  /quit            Exit");
    println!("  <bare text>      Send as data on active link or to target");
}

async fn run_connect(
    addr: String,
    identity_path: Option<PathBuf>,
    corrupt_every: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let socket_addr: SocketAddr = addr.parse().map_err(|e| format!("invalid address: {e}"))?;

    // Verify TCP connectivity before building the node, the driver silently
    // ignores connection failures, leaving the node running with no interfaces.
    tokio::net::TcpStream::connect(socket_addr)
        .await
        .map_err(|e| format!("cannot connect to {socket_addr}: {e}"))?;

    // Load or generate identity private key bytes, so we can create two Identity
    // instances (one for the builder, one for the destination. Identity is not Clone).
    let private_key_bytes = if let Some(path) = &identity_path {
        let bytes = std::fs::read(path)?;
        let id =
            Identity::from_private_key_bytes(&bytes).map_err(|e| format!("bad identity: {e}"))?;
        id.private_key_bytes().map_err(|e| e.to_string())?
    } else {
        use rand_core::OsRng;
        let id = Identity::generate(&mut OsRng);
        id.private_key_bytes().map_err(|e| e.to_string())?
    };

    let node_identity =
        Identity::from_private_key_bytes(&private_key_bytes).map_err(|e| e.to_string())?;
    let dest_identity =
        Identity::from_private_key_bytes(&private_key_bytes).map_err(|e| e.to_string())?;

    let identity_hash = hex_encode(node_identity.hash());

    // Build and start node
    let mut builder = ReticulumNodeBuilder::new()
        .identity(node_identity)
        .enable_transport(false)
        .add_tcp_client(socket_addr);
    if let Some(n) = corrupt_every {
        builder = builder.corrupt_every(Some(n));
    }
    let mut node = builder.build().await?;

    node.start().await?;

    // Create destination and register it
    let mut dest = Destination::new(
        Some(dest_identity),
        Direction::In,
        DestinationType::Single,
        "lnstest",
        &["connect"],
    )
    .map_err(|e| format!("destination error: {e}"))?;
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    let dest_hash_hex = hex_encode(dest_hash.as_bytes());
    node.register_destination(dest);

    // Announce ourselves
    if let Err(e) = node
        .announce_destination(&dest_hash, Some(b"lnstest-cli"))
        .await
    {
        eprintln!("announce failed: {e}");
    }

    println!("Identity: {identity_hash}");
    println!("Destination: {dest_hash_hex}");
    println!("Announced as lnstest-cli");
    println!("Type /help for commands.");
    print_prompt();

    // Take event receiver
    let event_rx = node
        .take_event_receiver()
        .ok_or("event receiver already taken")?;

    let state = Arc::new(Mutex::new(SessionState {
        discovered: BTreeMap::new(),
        active_link_id: None,
        packet_sender: None,
        show_announces: false,
    }));

    // Spawn event task
    let event_state = Arc::clone(&state);
    let event_task = tokio::spawn(event_loop(event_rx, event_state));

    // Input loop (main task)
    let mut stream: Option<LinkHandle> = None;
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break, // EOF
            Err(e) => {
                eprintln!("stdin error: {e}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            print_prompt();
            continue;
        }

        if trimmed.starts_with('/') {
            let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
            let cmd = parts[0];
            let arg = parts.get(1).map(|s| s.trim());

            match cmd {
                "/quit" => break,

                "/help" => {
                    print_help();
                }

                "/peers" => {
                    let st = state.lock().expect("lock poisoned");
                    if st.discovered.is_empty() {
                        println!("No peers discovered yet.");
                    } else {
                        for (hash, info) in &st.discovered {
                            let hops = info
                                .hops
                                .map(|h| format!("{h} hops"))
                                .unwrap_or_else(|| "? hops".to_string());
                            println!("  {hash}  {hops}  app_data: {}", info.app_data);
                        }
                    }
                }

                "/link" => {
                    let Some(hash_str) = arg else {
                        eprintln!("usage: /link <32-char hex destination hash>");
                        print_prompt();
                        continue;
                    };

                    if hash_str.len() != 32 {
                        eprintln!("destination hash must be 32 hex characters (16 bytes)");
                        print_prompt();
                        continue;
                    }

                    // Check if we already have an active link or target
                    {
                        let st = state.lock().expect("lock poisoned");
                        if st.active_link_id.is_some() {
                            eprintln!("close current link first (/close)");
                            print_prompt();
                            continue;
                        }
                        if st.packet_sender.is_some() {
                            eprintln!("clear single-packet target first (/untarget)");
                            print_prompt();
                            continue;
                        }
                    }

                    let hash_bytes = match hex_decode(hash_str) {
                        Ok(b) => b,
                        Err(e) => {
                            eprintln!("invalid hex: {e}");
                            print_prompt();
                            continue;
                        }
                    };

                    let mut dest_bytes = [0u8; 16];
                    dest_bytes.copy_from_slice(&hash_bytes);
                    let target_hash = DestinationHash::new(dest_bytes);

                    // Look up signing key from discovered announces
                    let signing_key = {
                        let st = state.lock().expect("lock poisoned");
                        st.discovered.get(hash_str).map(|info| {
                            let mut key = [0u8; 32];
                            key.copy_from_slice(&info.public_key[32..64]);
                            key
                        })
                    };

                    let Some(signing_key) = signing_key else {
                        eprintln!("destination not found in discovered peers, wait for announce");
                        print_prompt();
                        continue;
                    };

                    match node.connect(&target_hash, &signing_key).await {
                        Ok(s) => {
                            let link_id = *s.link_id();
                            {
                                let mut st = state.lock().expect("lock poisoned");
                                st.active_link_id = Some(link_id);
                            }
                            stream = Some(s);
                            println!("[linking] request sent for {hash_str}");
                        }
                        Err(e) => {
                            eprintln!("connect failed: {e}");
                        }
                    }
                }

                "/send" => {
                    let Some(msg) = arg else {
                        eprintln!("usage: /send <message>");
                        print_prompt();
                        continue;
                    };

                    if let Err(e) = try_send(&state, &stream, msg).await {
                        eprintln!("{e}");
                        if e.contains("closed by peer") {
                            stream = None;
                        }
                    }
                }

                "/close" => {
                    if let Some(mut s) = stream.take() {
                        if let Err(e) = s.close().await {
                            eprintln!("close error: {e}");
                        }
                        let mut st = state.lock().expect("lock poisoned");
                        st.active_link_id = None;
                        println!("[closed] link closed");
                    } else {
                        eprintln!("no active link");
                    }
                }

                "/target" => {
                    let Some(hash_str) = arg else {
                        eprintln!("usage: /target <32-char hex destination hash>");
                        print_prompt();
                        continue;
                    };

                    if hash_str.len() != 32 {
                        eprintln!("destination hash must be 32 hex characters (16 bytes)");
                        print_prompt();
                        continue;
                    }

                    {
                        let st = state.lock().expect("lock poisoned");
                        if st.active_link_id.is_some() {
                            eprintln!("close active link first (/close)");
                            print_prompt();
                            continue;
                        }
                    }

                    let hash_bytes = match hex_decode(hash_str) {
                        Ok(b) => b,
                        Err(e) => {
                            eprintln!("invalid hex: {e}");
                            print_prompt();
                            continue;
                        }
                    };

                    let mut dest_bytes = [0u8; 16];
                    dest_bytes.copy_from_slice(&hash_bytes);
                    let dest_hash = DestinationHash::new(dest_bytes);

                    {
                        let st = state.lock().expect("lock poisoned");
                        if !st.discovered.contains_key(hash_str) {
                            eprintln!(
                                "destination not found in discovered peers, wait for announce"
                            );
                            print_prompt();
                            continue;
                        }
                    }

                    {
                        let mut st = state.lock().expect("lock poisoned");
                        st.packet_sender = Some(node.packet_sender(&dest_hash));
                    }
                    println!("[target] sending to {hash_str}");
                }

                "/untarget" => {
                    let mut st = state.lock().expect("lock poisoned");
                    st.packet_sender = None;
                    println!("[untarget] single-packet target cleared");
                }

                "/announce" => match node
                    .announce_destination(&dest_hash, Some(b"lnstest-cli"))
                    .await
                {
                    Ok(()) => println!("[announced] {dest_hash_hex}"),
                    Err(e) => eprintln!("announce failed: {e}"),
                },

                "/quiet" => {
                    let mut st = state.lock().expect("lock poisoned");
                    st.show_announces = false;
                    println!("Announce/path messages hidden. Use /verbose to restore.");
                }

                "/verbose" => {
                    let mut st = state.lock().expect("lock poisoned");
                    st.show_announces = true;
                    println!("Announce/path messages visible.");
                }

                "/status" => {
                    let st = state.lock().expect("lock poisoned");
                    println!("Identity:    {identity_hash}");
                    println!("Destination: {dest_hash_hex}");
                    println!(
                        "Active link: {}",
                        st.active_link_id
                            .as_ref()
                            .map(|id| format!("{id}"))
                            .unwrap_or_else(|| "none".to_string())
                    );
                    println!(
                        "Target:      {}",
                        st.packet_sender
                            .as_ref()
                            .map(|ep| hex_encode(ep.dest_hash().as_bytes()))
                            .unwrap_or_else(|| "none".to_string())
                    );
                    println!("Paths:       {}", node.path_count());
                    println!("Transport:   {}", node.is_transport_enabled());
                    println!("Peers:       {}", st.discovered.len());
                }

                _ => {
                    eprintln!("unknown command: {cmd} (type /help)");
                }
            }
        } else {
            // Bare text → send as data if link is active
            if let Err(e) = try_send(&state, &stream, trimmed).await {
                eprintln!("{e}");
                if e.contains("closed by peer") {
                    stream = None;
                }
            }
        }

        print_prompt();
    }

    // Cleanup
    event_task.abort();
    if let Some(mut s) = stream.take() {
        let _ = s.close().await;
    }
    node.stop().await?;

    Ok(())
}

/// Try to send data on the active connection or to the single-packet target.
/// Returns Err with a user-facing message on failure.
async fn try_send(
    state: &Arc<Mutex<SessionState>>,
    stream: &Option<LinkHandle>,
    msg: &str,
) -> Result<(), String> {
    let (link_alive, has_endpoint) = {
        let st = state.lock().expect("lock poisoned");
        (st.active_link_id.is_some(), st.packet_sender.is_some())
    };

    // Prefer link if active
    if link_alive {
        let Some(s) = stream else {
            return Err("no active link, use /link first".to_string());
        };

        if msg.len() > 458 {
            return Err(format!("message too long ({} bytes, max 458)", msg.len()));
        }

        return s.try_send(msg.as_bytes()).await.map_err(|e| {
            tracing::debug!(error = %e, msg_len = msg.len(), "try_send: stream.try_send() failed");
            format!("send failed: {e}")
        });
    }

    // Check if peer closed the link but we still hold the stream
    if stream.is_some() {
        return Err("[closed] link closed by peer".to_string());
    }

    // Try single-packet target
    if has_endpoint {
        let ep = {
            let st = state.lock().expect("lock poisoned");
            st.packet_sender.clone().unwrap()
        };
        return ep
            .send(msg.as_bytes())
            .await
            .map(|_hash| ())
            .map_err(|e| format!("send failed: {e}"));
    }

    Err("no active link or target, use /link or /target first".to_string())
}

/// Event loop task: reads NodeEvents and prints them, updating shared state.
async fn event_loop(mut event_rx: EventReceiver, state: Arc<Mutex<SessionState>>) {
    use tokio::io::AsyncWriteExt;
    let mut stdout = tokio::io::stdout();

    while let Some(event) = event_rx.recv().await {
        // Build output message and state updates for each event
        let msg = match event {
            NodeEvent::AnnounceReceived {
                announce,
                interface_index: _,
            } => {
                let hash = hex_encode(announce.destination_hash().as_bytes());
                let app_data = display_app_data(announce.app_data());
                let public_key = *announce.public_key();

                let mut st = state.lock().expect("lock poisoned");
                let entry = st.discovered.entry(hash.clone()).or_insert(AnnounceInfo {
                    app_data: app_data.clone(),
                    hops: None,
                    public_key,
                });
                // Update on re-announce
                entry.app_data = app_data.clone();
                entry.public_key = public_key;

                if !st.show_announces {
                    continue;
                }
                format!("[announce] {hash}  app_data: {app_data}")
            }

            NodeEvent::PathFound {
                destination_hash,
                hops,
                interface_index: _,
            } => {
                let hash = hex_encode(destination_hash.as_bytes());

                let mut st = state.lock().expect("lock poisoned");
                let info = st.discovered.entry(hash.clone()).or_insert(AnnounceInfo {
                    app_data: String::new(),
                    hops: None,
                    public_key: [0u8; 64],
                });
                info.hops = Some(hops);

                if !st.show_announces {
                    continue;
                }
                format!("[path] {hash}  {hops} hops")
            }

            NodeEvent::LinkEstablished {
                link_id,
                is_initiator,
                ..
            } => {
                // Incoming links are auto-accepted and proved by the core (Python
                // parity), so a responder just observes this event; there is no
                // separate request/accept step.
                if is_initiator {
                    format!("[connected] link {link_id} (initiator)")
                } else {
                    format!("[connected] link {link_id} (incoming, auto-accepted)")
                }
            }

            NodeEvent::LinkDataReceived { link_id: _, data } => {
                let text = String::from_utf8_lossy(&data);
                format!("[received] {text}")
            }

            NodeEvent::MessageReceived {
                link_id: _,
                msgtype,
                sequence: _,
                data,
            } => {
                let text = String::from_utf8_lossy(&data);
                format!("[message type={msgtype}] {text}")
            }

            NodeEvent::LinkClosed {
                link_id, reason, ..
            } => {
                {
                    let mut st = state.lock().expect("lock poisoned");
                    if st.active_link_id == Some(link_id) {
                        st.active_link_id = None;
                    }
                }
                format!("[closed] link {link_id}: {reason:?}")
            }

            NodeEvent::LinkStale { link_id } => {
                format!("[stale] link {link_id}")
            }

            NodeEvent::LinkRecovered { link_id } => {
                format!("[recovered] link {link_id}")
            }

            NodeEvent::PacketReceived {
                destination: _,
                data,
                ..
            } => {
                let text = String::from_utf8_lossy(&data);
                format!("[packet] {text}")
            }

            _ => continue, // Ignore other events
        };

        // Print the event on its own line, then re-print the prompt
        let output = format!("\r{msg}\n> ");
        let _ = stdout.write_all(output.as_bytes()).await;
        let _ = stdout.flush().await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Initialize logging: RUST_LOG env takes precedence, then -v flag
    let default_filter = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_target(true)
        .init();

    if let Some(n) = args.corrupt_every {
        eprintln!("WARNING: Fault injection active: corrupting ~1 byte per {n} bytes");
    }

    match args.command {
        Commands::Identity { action } => match action {
            IdentityAction::Generate { output } => {
                use rand_core::OsRng;

                let identity = Identity::generate(&mut OsRng);
                let hash = identity.hash();
                let hash_hex = hex_encode(hash);

                println!("Generated new identity");
                println!("Hash: {hash_hex}");

                if let Some(path) = output {
                    let key_bytes = identity.private_key_bytes().map_err(|e| e.to_string())?;
                    std::fs::write(&path, key_bytes)?;
                    println!("Saved to: {}", path.display());
                } else {
                    let pub_key = identity.public_key_bytes();
                    let pub_hex = hex_encode(&pub_key);
                    println!("Public key: {pub_hex}");
                }
            }

            IdentityAction::Show { path } => {
                let key_bytes = std::fs::read(&path)?;
                let identity =
                    Identity::from_private_key_bytes(&key_bytes).map_err(|e| e.to_string())?;
                let hash = identity.hash();
                let hash_hex = hex_encode(hash);
                let pub_key = identity.public_key_bytes();
                let pub_hex = hex_encode(&pub_key);

                println!("Identity: {}", path.display());
                println!("Hash: {hash_hex}");
                println!("Public key: {pub_hex}");
            }
        },

        Commands::Selftest {
            targets,
            duration,
            rate,
            mode,
            discovery_timeout,
        } => {
            selftest::run_selftest(
                targets,
                duration,
                rate,
                &mode,
                args.corrupt_every,
                discovery_timeout,
                args.config,
            )
            .await?;
        }

        Commands::Connect { addr, identity } => {
            run_connect(addr, identity, args.corrupt_every).await?;
        }

        Commands::Diag {
            output,
            instance_name,
            event_log,
            no_rpc,
        } => {
            diag::run(
                diag::DiagOptions {
                    config_dir: args.config,
                    instance_name,
                    event_log,
                    no_rpc,
                },
                output,
            )
            .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_app_data_plain_utf8() {
        assert_eq!(display_app_data(b"FK_Nomadek"), "FK_Nomadek");
        assert_eq!(display_app_data(b"MNTL"), "MNTL");
        assert_eq!(
            display_app_data(b"g00n Cloud (Dallas)"),
            "g00n Cloud (Dallas)"
        );
    }

    #[test]
    fn test_display_app_data_msgpack_bin8_then_fixstr() {
        // msgpack: fixarray(2), bin8(len=2, [0xFF, 0xFE]), fixstr(len=5, "Hello")
        let data = [
            0x92, // fixarray(2)
            0xc4, 0x02, 0xFF, 0xFE, // bin8, len=2, data
            0xa5, b'H', b'e', b'l', b'l', b'o', // fixstr(5)
        ];
        assert_eq!(display_app_data(&data), "Hello");
    }

    #[test]
    fn test_display_app_data_msgpack_bin8_then_str8() {
        // msgpack: fixarray(2), bin8(len=1, [0xAA]), str8(len=4, "Test")
        let data = [
            0x92, // fixarray(2)
            0xc4, 0x01, 0xAA, // bin8, len=1
            0xd9, 0x04, b'T', b'e', b's', b't', // str8, len=4
        ];
        assert_eq!(display_app_data(&data), "Test");
    }

    #[test]
    fn test_display_app_data_empty() {
        assert_eq!(display_app_data(b""), "");
    }

    #[test]
    fn test_display_app_data_msgpack_fallback_on_invalid() {
        // Starts with 0x92 but structure is broken, should fall back
        let data = [0x92, 0xFF];
        let result = display_app_data(&data);
        // Should not panic, 0xFF is negative fixint, not printable
        assert!(result.is_empty());
    }

    #[test]
    fn test_display_app_data_realistic_lxmf() {
        // Simulates LXMF-style: [bin(18 bytes hash), str("Meteo Bot - MSG ME!")]
        let name = b"Meteo Bot - MSG ME!";
        let mut data = vec![0x92]; // fixarray(2)
        data.push(0xc4); // bin8
        data.push(18); // length
        data.extend_from_slice(&[0x01; 18]); // 18 bytes of binary
        data.push(0xa0 | (name.len() as u8)); // fixstr(19)
        data.extend_from_slice(name);
        assert_eq!(display_app_data(&data), "Meteo Bot - MSG ME!");
    }

    #[test]
    fn test_display_app_data_name_in_first_element_bin8() {
        // Real pattern: [bin8("screwpress"), bin8(binary_hash)]
        // Display name is in the FIRST element as bin8 with valid UTF-8
        let name = b"screwpress";
        let mut data = vec![0x92]; // fixarray(2)
        data.push(0xc4); // bin8
        data.push(name.len() as u8);
        data.extend_from_slice(name);
        data.push(0xc4); // bin8
        data.push(4); // short binary hash
        data.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0xFC]);
        assert_eq!(display_app_data(&data), "screwpress");
    }

    #[test]
    fn test_display_app_data_picks_longest_utf8() {
        // Two valid UTF-8 strings, should pick the longer one
        let mut data = vec![0x92]; // fixarray(2)
        data.push(0xa2); // fixstr(2) "Hi"
        data.extend_from_slice(b"Hi");
        data.push(0xa5); // fixstr(5) "World"
        data.extend_from_slice(b"World");
        assert_eq!(display_app_data(&data), "World");
    }

    #[test]
    fn test_display_app_data_fixmap_extracts_value() {
        // fixmap(2): {"protocol": "rcav", "name": "RRC Beleth"}
        let mut data = vec![0x82]; // fixmap(2)
                                   // key 1: fixstr(8) "protocol"
        data.push(0xa8);
        data.extend_from_slice(b"protocol");
        // value 1: fixstr(4) "rcav"
        data.push(0xa4);
        data.extend_from_slice(b"rcav");
        // key 2: fixstr(4) "name"
        data.push(0xa4);
        data.extend_from_slice(b"name");
        // value 2: fixstr(10) "RRC Beleth"
        data.push(0xaa);
        data.extend_from_slice(b"RRC Beleth");
        assert_eq!(display_app_data(&data), "RRC Beleth");
    }

    #[test]
    fn test_display_app_data_fixmap_skips_short_values() {
        // fixmap(1): {"c": "AB"}, value too short to be a name, but returned
        // since it's the only text in the map
        let data = [0x81, 0xa1, b'c', 0xa2, b'A', b'B'];
        assert_eq!(display_app_data(&data), "AB");
    }

    #[test]
    fn test_display_app_data_pure_binary() {
        // 16 bytes of non-UTF-8 binary, should show empty, not garbled
        let data = [
            0xFF, 0xFE, 0x01, 0x02, 0x69, 0xAB, 0x5B, 0x71, 0xBC, 0xCD, 0xDE, 0x28, 0xEF, 0xF0,
            0x12, 0x34,
        ];
        let result = display_app_data(&data);
        // No garbled replacement characters, either empty or short printable run
        assert!(!result.contains('\u{FFFD}'));
    }

    #[test]
    fn test_display_app_data_binary_with_embedded_text() {
        // Binary blob with "RRC Beleth" embedded, should extract it
        let mut data = vec![0xFF, 0x82, 0xAB];
        data.extend_from_slice(b"RRC Beleth");
        data.extend_from_slice(&[0xFF, 0xFE, 0xFD]);
        assert_eq!(display_app_data(&data), "RRC Beleth");
    }

    #[test]
    fn test_display_app_data_short_binary_no_text() {
        // Short binary with only 1-2 char printable runs, shows empty
        let data = [0xFF, b'a', 0xFE, b'b', 0xFD];
        assert_eq!(display_app_data(&data), "");
    }
}
