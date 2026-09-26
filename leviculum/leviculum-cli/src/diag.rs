//! `lnstest diag` — collect a self-contained diagnostic bundle from a running
//! `lnsd` (or Python `rnsd` — drop-in compatible).
//!
//! The bundle is a single annotated UTF-8 text blob: versions/build, the
//! (secret-redacted) config, the configured interfaces, the daemon's live
//! view via the shared-instance RPC (interface stats, path table, link
//! count), best-effort system info, and an event-log pointer. It is written
//! to stdout by default, or to a file with `--output`.
//!
//! Secrets are redacted: IFAC `passphrase` and `networkname` from the config
//! never appear; the node identity private key (`storage/transport_identity`)
//! is opened only to derive the RPC authkey (`SHA256(file)`) and its bytes
//! never enter the bundle. If a daemon query fails (daemon down, or `rnsd`
//! lacking a query), that section degrades gracefully — it never aborts the
//! bundle.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use leviculum_std::config::{Config, InterfaceConfig};

use crate::daemon_rpc::{resolve_authkey, resolve_instance_name};

/// Placeholder substituted for redacted secret values.
const REDACTED: &str = "<redacted>";

/// Options parsed from the `lnstest diag` subcommand.
pub struct DiagOptions {
    /// Config directory (the global `-c/--config`); `None` ⇒ default lookup.
    pub config_dir: Option<PathBuf>,
    /// Shared-instance name override; `None` ⇒ value from config, else `default`.
    pub instance_name: Option<String>,
    /// If set, tail this structured event-log file into the bundle.
    pub event_log: Option<PathBuf>,
    /// Skip the daemon RPC queries entirely (local/static parts only).
    pub no_rpc: bool,
}

/// Run `lnstest diag`: build the bundle, write it to `output` (or stdout).
pub async fn run(
    opts: DiagOptions,
    output: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bundle = build_bundle(&opts).await;
    match output {
        Some(path) => {
            std::fs::write(&path, bundle.as_bytes())?;
            eprintln!(
                "wrote diagnostic bundle to {} ({} bytes)",
                path.display(),
                bundle.len()
            );
        }
        None => {
            print!("{bundle}");
        }
    }
    Ok(())
}

/// Assemble the full diagnostic bundle as a string.
pub async fn build_bundle(opts: &DiagOptions) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "===== Leviculum diagnostic bundle =====");
    let _ = writeln!(out);

    // --- Versions / build ---
    section_header(&mut out, "Versions / build");
    let _ = writeln!(out, "lnstest version: {}", env!("LEVICULUM_VERSION"));
    let _ = writeln!(
        out,
        "build profile: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    let _ = writeln!(
        out,
        "target: {} / {}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    let _ = writeln!(
        out,
        "daemon version: not exposed by the shared-instance RPC (no precedent in \
         Python rnsd) — check the daemon's own startup log if needed"
    );
    let _ = writeln!(out);

    // --- Config ---
    let config_dir = opts
        .config_dir
        .clone()
        .unwrap_or_else(Config::default_config_dir);
    let config_file = config_dir.join("config");
    section_header(&mut out, "Config");
    let _ = writeln!(out, "config dir:  {}", config_dir.display());
    let _ = writeln!(out, "config file: {}", config_file.display());
    let loaded_config: Option<Config> = if config_file.exists() {
        match Config::load(&config_file) {
            Ok(c) => {
                let _ = writeln!(out, "config file: present, parsed OK");
                Some(c)
            }
            Err(e) => {
                let _ = writeln!(out, "config file: present but FAILED to parse: {e}");
                None
            }
        }
    } else {
        let _ = writeln!(
            out,
            "config file: NOT present — daemon is using built-in defaults"
        );
        None
    };
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Effective config (TOML, secrets redacted; the raw file is NOT included \
         because it may contain secrets):"
    );
    let effective = match &loaded_config {
        Some(c) => redact_config(c),
        None => Config::default(),
    };
    let _ = writeln!(out, "{}", render_config_toml(&effective));
    let _ = writeln!(out);

    // --- Interfaces (from config) ---
    section_header(&mut out, "Interfaces (configured)");
    if effective.interfaces.is_empty() {
        let _ = writeln!(out, "(none configured)");
    } else {
        let mut names: Vec<&String> = effective.interfaces.keys().collect();
        names.sort();
        for name in names {
            let iface = &effective.interfaces[name];
            let _ = writeln!(out, "- {name}");
            render_interface_config(&mut out, iface);
        }
    }
    let _ = writeln!(out);

    // --- Daemon view (RPC) ---
    section_header(&mut out, "Daemon view (shared-instance RPC)");
    let instance_name =
        resolve_instance_name(opts.instance_name.as_deref(), loaded_config.as_ref());
    let _ = writeln!(out, "instance name: {instance_name}");
    let _ = writeln!(out, "RPC socket:    \\0rns/{instance_name}/rpc");
    if opts.no_rpc {
        let _ = writeln!(out, "(skipped: --no-rpc)");
    } else {
        match resolve_authkey(&config_dir, loaded_config.as_ref()) {
            Ok((authkey, id_path)) => {
                let _ = writeln!(
                    out,
                    "authkey:       derived from {} (not shown)",
                    id_path.display()
                );
                let _ = writeln!(out);
                append_rpc_section(&mut out, &instance_name, &authkey).await;
            }
            Err(msg) => {
                let _ = writeln!(out, "cannot derive RPC authkey: {msg}");
                let _ = writeln!(out, "(daemon queries skipped)");
            }
        }
    }
    let _ = writeln!(out);

    // --- System ---
    section_header(&mut out, "System");
    let _ = writeln!(out, "{}", system_info());
    let _ = writeln!(out);

    // --- Event log ---
    section_header(&mut out, "Recent events");
    let _ = writeln!(out, "{}", event_log_section(opts.event_log.as_deref()));
    let _ = writeln!(out);

    let _ = writeln!(out, "===== end of diagnostic bundle =====");
    out
}

fn section_header(out: &mut String, title: &str) {
    let _ = writeln!(out, "----- {title} -----");
}

// ---------------------------------------------------------------------------
// Config redaction
// ---------------------------------------------------------------------------

/// Return a clone of `config` with all secret fields replaced by [`REDACTED`].
///
/// Secrets in the config file: per-interface IFAC `passphrase` and
/// `networkname` (the two halves of the IFAC pre-shared key). Everything else
/// in `Config`/`ReticulumConfig`/`InterfaceConfig` is non-sensitive
/// connection/tuning state. The node identity private key is not in the config
/// file at all (it lives in `storage/transport_identity`), so nothing here
/// touches it.
pub fn redact_config(config: &Config) -> Config {
    let mut c = config.clone();
    for iface in c.interfaces.values_mut() {
        if iface.passphrase.is_some() {
            iface.passphrase = Some(REDACTED.to_string());
        }
        if iface.networkname.is_some() {
            iface.networkname = Some(REDACTED.to_string());
        }
    }
    c
}

fn render_config_toml(config: &Config) -> String {
    match toml::to_string_pretty(config) {
        Ok(s) => s,
        Err(e) => format!("<failed to serialise config: {e}>"),
    }
}

fn render_interface_config(out: &mut String, iface: &InterfaceConfig) {
    let _ = writeln!(out, "    type:    {}", iface.interface_type);
    let _ = writeln!(out, "    enabled: {}", iface.enabled);
    let mut kv = |k: &str, v: String| {
        let _ = writeln!(out, "    {k}: {v}");
    };
    if let (Some(ip), Some(port)) = (&iface.listen_ip, iface.listen_port) {
        kv("listen", format!("{ip}:{port}"));
    } else if let Some(port) = iface.listen_port {
        kv("listen_port", port.to_string());
    }
    if let (Some(h), Some(p)) = (&iface.target_host, iface.target_port) {
        kv("target", format!("{h}:{p}"));
    }
    if let (Some(ip), Some(p)) = (&iface.forward_ip, iface.forward_port) {
        kv("forward", format!("{ip}:{p}"));
    }
    if let Some(p) = &iface.port {
        kv("serial port", p.clone());
    }
    if let Some(s) = iface.speed {
        kv("serial speed", s.to_string());
    }
    if let Some(g) = &iface.group_id {
        kv("group_id", g.clone());
    }
    if let Some(s) = &iface.discovery_scope {
        kv("discovery_scope", s.clone());
    }
    if let Some(d) = &iface.devices {
        kv("devices", d.clone());
    }
    // RNode / LoRa radio settings
    if let Some(f) = iface.frequency {
        kv("frequency_hz", f.to_string());
    }
    if let Some(b) = iface.bandwidth {
        kv("bandwidth_hz", b.to_string());
    }
    if let Some(sf) = iface.spreading_factor {
        kv("spreading_factor", sf.to_string());
    }
    if let Some(cr) = iface.coding_rate {
        kv("coding_rate", cr.to_string());
    }
    if let Some(tp) = iface.tx_power {
        kv("tx_power_dbm", tp.to_string());
    }
    if let Some(fc) = iface.flow_control {
        kv("flow_control", fc.to_string());
    }
    if let Some(c) = iface.csma_enabled {
        kv("csma_enabled", c.to_string());
    }
    // IFAC: presence only — values are redacted in the TOML dump above.
    if iface.networkname.is_some() || iface.passphrase.is_some() {
        kv(
            "ifac",
            "configured (passphrase/networkname redacted)".to_string(),
        );
    }
}

// ---------------------------------------------------------------------------
// RPC / daemon view
// ---------------------------------------------------------------------------

async fn append_rpc_section(out: &mut String, instance_name: &str, authkey: &[u8; 32]) {
    // interface_stats
    let _ = writeln!(out, "## interface_stats");
    match leviculum_std::rpc_query(instance_name, authkey, "interface_stats").await {
        Ok(v) => {
            render_interface_stats_summary(out, &v);
            let _ = writeln!(out);
            let _ = writeln!(out, "raw:");
            let _ = writeln!(out, "{}", pretty_json(&v));
        }
        Err(e) => {
            let _ = writeln!(out, "<unavailable: {e}>");
            let _ = writeln!(
                out,
                "(is lnsd running with `shared_instance = true`, and is this the \
                 right --instance-name / -c config dir?)"
            );
        }
    }
    let _ = writeln!(out);

    // path_table
    let _ = writeln!(out, "## path_table");
    match leviculum_std::rpc_query(instance_name, authkey, "path_table").await {
        Ok(v) => {
            let n = v.as_array().map(|a| a.len()).unwrap_or(0);
            let _ = writeln!(out, "known paths: {n}");
            let _ = writeln!(out, "{}", pretty_json(&v));
        }
        Err(e) => {
            let _ = writeln!(out, "<unavailable: {e}>");
        }
    }
    let _ = writeln!(out);

    // link_count (the Python-compat scalar — also available against rnsd).
    // It counts the TRANSPORT link table: links this node RELAYS, which is a
    // different set from the `link_table` section below (links it terminates).
    let _ = writeln!(out, "## link_count");
    match leviculum_std::rpc_query(instance_name, authkey, "link_count").await {
        Ok(v) => {
            let _ = writeln!(out, "relayed links: {}", pretty_json(&v));
        }
        Err(e) => {
            let _ = writeln!(out, "<unavailable: {e}>");
        }
    }
    let _ = writeln!(out);

    // link_table (Leviculum-only extension; Python rnsd rejects it with an
    // "unknown get command" error, which surfaces here as `<unavailable: …>`.
    // Not the same table as `link_count` above: these are the links this node
    // TERMINATES, that one counts the links it relays).
    let _ = writeln!(out, "## link_table");
    match leviculum_std::rpc_query(instance_name, authkey, "link_table").await {
        Ok(v) => {
            render_link_table_summary(out, &v);
            let _ = writeln!(out);
            let _ = writeln!(out, "raw:");
            let _ = writeln!(out, "{}", pretty_json(&v));
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "<unavailable: {e}>  (Leviculum-only RPC; expected against Python rnsd)"
            );
        }
    }
}

fn render_link_table_summary(out: &mut String, v: &serde_json::Value) {
    let Some(list) = v.as_array() else {
        let _ = writeln!(out, "(no link_table array in response)");
        return;
    };
    let _ = writeln!(out, "links ({}):", list.len());
    if list.is_empty() {
        return;
    }
    for link in list {
        let link_id = link.get("link_id").and_then(|x| x.as_str()).unwrap_or("?");
        let state = link.get("state").and_then(|x| x.as_str()).unwrap_or("?");
        let peer = link
            .get("destination_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("?");
        let age = link
            .get("age")
            .and_then(|x| x.as_f64())
            .map(|s| format!("{s:.0}s"))
            .unwrap_or_else(|| "n/a".to_string());
        let iface = link.get("interface").and_then(|x| x.as_str()).unwrap_or("");
        let _ = writeln!(
            out,
            "  - {link_id}  state={state}  peer={peer}  age={age}  iface={iface}"
        );
    }
}

fn render_interface_stats_summary(out: &mut String, v: &serde_json::Value) {
    if let Some(tid) = v.get("transport_id").and_then(|x| x.as_str()) {
        let _ = writeln!(out, "transport id: {tid}");
    }
    if let Some(up) = v.get("transport_uptime").and_then(|x| x.as_f64()) {
        let _ = writeln!(out, "daemon uptime: {} ({:.0}s)", format_duration(up), up);
    }
    let ifaces = v.get("interfaces").and_then(|x| x.as_array());
    match ifaces {
        Some(list) => {
            let _ = writeln!(out, "interfaces ({}):", list.len());
            for iface in list {
                let name = iface.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                let itype = iface.get("type").and_then(|x| x.as_str()).unwrap_or("?");
                let status = iface
                    .get("status")
                    .and_then(|x| x.as_bool())
                    .map(|b| if b { "up" } else { "down" })
                    .unwrap_or("?");
                let rxb = iface.get("rxb").and_then(|x| x.as_i64()).unwrap_or(0);
                let txb = iface.get("txb").and_then(|x| x.as_i64()).unwrap_or(0);
                let peers = iface.get("peers").and_then(|x| x.as_i64());
                let clients = iface.get("clients").and_then(|x| x.as_i64());
                let mut extra = String::new();
                if let Some(p) = peers {
                    let _ = write!(extra, " peers={p}");
                }
                if let Some(c) = clients {
                    let _ = write!(extra, " clients={c}");
                }
                let _ = writeln!(
                    out,
                    "  - {name}  type={itype} status={status} rxb={rxb} txb={txb}{extra}"
                );
            }
        }
        None => {
            let _ = writeln!(out, "(no `interfaces` array in response)");
        }
    }
}

fn pretty_json(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|e| format!("<failed to render JSON: {e}>"))
}

fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "?".to_string();
    }
    let total = secs as u64;
    let d = total / 86_400;
    let h = (total % 86_400) / 3_600;
    let m = (total % 3_600) / 60;
    let s = total % 60;
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(format!("{d}d"));
    }
    if h > 0 || d > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 || h > 0 || d > 0 {
        parts.push(format!("{m}m"));
    }
    parts.push(format!("{s}s"));
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// System / event log
// ---------------------------------------------------------------------------

fn system_info() -> String {
    let mut s = String::new();
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "?".to_string());
    let _ = writeln!(s, "os: {}  kernel: {kernel}", std::env::consts::OS);
    if let Ok(osr) = std::fs::read_to_string("/etc/os-release") {
        if let Some(line) = osr.lines().find(|l| l.starts_with("PRETTY_NAME=")) {
            let _ = writeln!(
                s,
                "distro: {}",
                line.trim_start_matches("PRETTY_NAME=").trim_matches('"')
            );
        }
    }
    // Best-effort daemon process info (works only if running as the same user / root).
    match daemon_pid() {
        Some(pid) => {
            let _ = writeln!(s, "lnsd pid: {pid}");
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
                if let Some(l) = status.lines().find(|l| l.starts_with("VmRSS:")) {
                    let _ = writeln!(s, "lnsd VmRSS: {}", l.trim_start_matches("VmRSS:").trim());
                }
            }
            if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
                let _ = writeln!(s, "lnsd open fds: {}", rd.count());
            }
        }
        None => {
            let _ = writeln!(
                s,
                "lnsd pid: not found (no /proc match for an `lnsd` process — daemon down, \
                 running under a different name, or not visible to this user)"
            );
        }
    }
    s.trim_end().to_string()
}

/// Best-effort: find the PID of a running `lnsd` by scanning `/proc/*/comm`.
/// Returns `Some(pid)` only if exactly one match is found.
fn daemon_pid() -> Option<u32> {
    let mut found: Option<u32> = None;
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let comm = match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if comm.trim() == "lnsd" {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(pid);
        }
    }
    found
}

const EVENT_LOG_TAIL_LINES: usize = 500;

fn event_log_section(event_log: Option<&Path>) -> String {
    match event_log {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(EVENT_LOG_TAIL_LINES);
                let tail = lines[start..].join("\n");
                format!(
                    "event log: {} (last {} of {} lines)\n{tail}",
                    path.display(),
                    lines.len() - start,
                    lines.len()
                )
            }
            Err(e) => format!("event log: {} — could not read: {e}", path.display()),
        },
        None => "No structured event-log file specified (pass --event-log <path> if lnsd was \
                 started with LEVICULUM_EVENT_LOG set). Otherwise attach the daemon's log: under \
                 systemd run `journalctl -u lnsd --since '1 hour ago'`, else the daemon's stderr."
            .to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A config-dir-less, daemon-less options struct with a unique instance
    /// name so it can never accidentally hit a real `lnsd` on the host.
    fn opts_for(config_dir: Option<PathBuf>, no_rpc: bool) -> DiagOptions {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        DiagOptions {
            config_dir,
            instance_name: Some(format!(
                "lnstest-diag-unittest-{}-{}",
                std::process::id(),
                n
            )),
            event_log: None,
            no_rpc,
        }
    }

    fn sample_config() -> Config {
        let toml_src = r#"
[reticulum]
enable_transport = true
instance_name = "myinst"

[interfaces.tcpout]
type = "TCPClientInterface"
target_host = "198.51.100.7"
target_port = 4242
networkname = "supersecret-net"
passphrase = "hunter2-the-passphrase"

[interfaces.rnode0]
type = "RNodeInterface"
port = "/dev/ttyACM0"
frequency = 867200000
bandwidth = 125000
spreading_factor = 8
coding_rate = 5
tx_power = 7
"#;
        toml::from_str(toml_src).expect("sample config parses")
    }

    #[test]
    fn redact_config_removes_secrets_keeps_the_rest() {
        let original = sample_config();
        let redacted = redact_config(&original);

        let tcp = redacted.interfaces.get("tcpout").unwrap();
        assert_eq!(tcp.passphrase.as_deref(), Some(REDACTED));
        assert_eq!(tcp.networkname.as_deref(), Some(REDACTED));
        // Non-secret fields untouched.
        assert_eq!(tcp.target_host.as_deref(), Some("198.51.100.7"));
        assert_eq!(tcp.target_port, Some(4242));
        assert_eq!(tcp.interface_type, "TCPClientInterface");

        let rnode = redacted.interfaces.get("rnode0").unwrap();
        assert_eq!(rnode.port.as_deref(), Some("/dev/ttyACM0"));
        assert_eq!(rnode.frequency, Some(867_200_000));
        assert_eq!(rnode.spreading_factor, Some(8));
        // No IFAC fields ⇒ stays None, not "<redacted>".
        assert_eq!(rnode.passphrase, None);
        assert_eq!(rnode.networkname, None);

        // The original is not mutated.
        assert_eq!(
            original
                .interfaces
                .get("tcpout")
                .unwrap()
                .passphrase
                .as_deref(),
            Some("hunter2-the-passphrase")
        );
    }

    #[tokio::test]
    async fn bundle_redacts_secrets_and_has_all_sections() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), {
            // Re-serialise the sample config so the file is real on disk.
            toml::to_string_pretty(&sample_config()).unwrap()
        })
        .unwrap();

        let bundle = build_bundle(&opts_for(Some(dir.path().to_path_buf()), false)).await;

        // Secret values must appear nowhere.
        assert!(
            !bundle.contains("hunter2-the-passphrase"),
            "IFAC passphrase leaked into the bundle"
        );
        assert!(
            !bundle.contains("supersecret-net"),
            "IFAC networkname leaked into the bundle"
        );
        assert!(bundle.contains(REDACTED), "redaction marker missing");

        // Non-secret config survives.
        assert!(bundle.contains("198.51.100.7"));
        assert!(bundle.contains("/dev/ttyACM0"));

        // All section headers present.
        for header in [
            "Versions / build",
            "Config",
            "Interfaces (configured)",
            "Daemon view (shared-instance RPC)",
            "System",
            "Recent events",
        ] {
            assert!(bundle.contains(header), "missing section: {header}");
        }
        // No daemon ⇒ graceful degradation, not a crash.
        assert!(bundle.contains("<unavailable") || bundle.contains("cannot derive RPC authkey"));
    }

    #[tokio::test]
    async fn identity_private_key_never_enters_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("storage")).unwrap();
        // Distinctive 64-byte "identity" — bytes 0..=63.
        let id_bytes: Vec<u8> = (0u8..64).collect();
        std::fs::write(
            dir.path().join("storage").join("transport_identity"),
            &id_bytes,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("config"),
            "[reticulum]\nshared_instance = true\n",
        )
        .unwrap();

        // no_rpc = false ⇒ the collector reads the identity file to derive the
        // authkey, then tries (and fails — no daemon) the RPC. The bytes (and
        // their hex, and the derived authkey) must not appear in the output.
        let bundle = build_bundle(&opts_for(Some(dir.path().to_path_buf()), false)).await;

        let id_hex: String = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!bundle.contains(&id_hex), "identity hex leaked");

        use sha2::Digest;
        let authkey_hex: String = sha2::Sha256::digest(&id_bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(!bundle.contains(&authkey_hex), "derived authkey leaked");

        // Raw bytes can't appear in a UTF-8 String literally, but check anyway.
        assert!(
            !bundle
                .as_bytes()
                .windows(id_bytes.len())
                .any(|w| w == id_bytes.as_slice()),
            "raw identity bytes leaked"
        );
        // It should at least acknowledge the identity file (path, not contents).
        assert!(bundle.contains("transport_identity"));
    }

    #[tokio::test]
    async fn no_rpc_skips_the_daemon_section() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("storage")).unwrap();
        std::fs::write(
            dir.path().join("storage").join("transport_identity"),
            vec![7u8; 64],
        )
        .unwrap();
        std::fs::write(dir.path().join("config"), "[reticulum]\n").unwrap();

        let bundle = build_bundle(&opts_for(Some(dir.path().to_path_buf()), true)).await;
        assert!(bundle.contains("(skipped: --no-rpc)"));
        // With --no-rpc we don't even read the identity file.
        assert!(!bundle.contains("authkey:"));
    }

    #[tokio::test]
    async fn missing_config_uses_defaults_and_still_produces_a_bundle() {
        let dir = tempfile::tempdir().unwrap(); // empty — no config file
        let bundle = build_bundle(&opts_for(Some(dir.path().to_path_buf()), true)).await;
        assert!(bundle.contains("config file: NOT present"));
        assert!(bundle.contains("===== end of diagnostic bundle ====="));
    }

    #[tokio::test]
    async fn event_log_tail_is_included_when_given() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), "[reticulum]\n").unwrap();
        let log_path = dir.path().join("events.log");
        let mut content = String::new();
        for i in 0..(EVENT_LOG_TAIL_LINES + 50) {
            content.push_str(&format!("EVENT_{i} node=local t={i}\n"));
        }
        std::fs::write(&log_path, content).unwrap();

        let mut opts = opts_for(Some(dir.path().to_path_buf()), true);
        opts.event_log = Some(log_path);
        let bundle = build_bundle(&opts).await;
        // Last line present, an early (truncated) line absent.
        assert!(bundle.contains(&format!("EVENT_{}", EVENT_LOG_TAIL_LINES + 49)));
        assert!(!bundle.contains("EVENT_0 node=local"));
    }

    #[test]
    fn format_duration_is_sane() {
        assert_eq!(format_duration(0.0), "0s");
        assert_eq!(format_duration(59.0), "59s");
        assert_eq!(format_duration(61.0), "1m 1s");
        assert_eq!(format_duration(3661.0), "1h 1m 1s");
        assert_eq!(format_duration(90_061.0), "1d 1h 1m 1s");
        assert_eq!(format_duration(-1.0), "?");
    }

    /// `render_link_table_summary` formats a JSON array of per-link rows
    /// (matching the `link_table` RPC response decoded by `rpc_query`) into
    /// the one-line-per-link summary in the bundle's `## link_table` section.
    #[test]
    fn render_link_table_summary_renders_per_link_lines() {
        let v = serde_json::json!([
            {
                "link_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "state": "active",
                "destination_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "age": 42.7,
                "interface": "TCPInterface[example/1.2.3.4:4242]"
            },
            {
                "link_id": "cccccccccccccccccccccccccccccccc",
                "state": "pending",
                "destination_hash": "dddddddddddddddddddddddddddddddd",
                "age": null,
                "interface": ""
            }
        ]);
        let mut out = String::new();
        render_link_table_summary(&mut out, &v);
        assert!(out.contains("links (2):"), "got: {out}");
        assert!(
            out.contains(
                "  - aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  state=active  \
                 peer=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  age=43s  \
                 iface=TCPInterface[example/1.2.3.4:4242]"
            ),
            "active link row missing or wrong, got: {out}"
        );
        assert!(
            out.contains(
                "  - cccccccccccccccccccccccccccccccc  state=pending  \
                 peer=dddddddddddddddddddddddddddddddd  age=n/a  iface="
            ),
            "pending link row missing or wrong, got: {out}"
        );

        // Empty array path renders just the header, no rows.
        let mut empty_out = String::new();
        render_link_table_summary(&mut empty_out, &serde_json::json!([]));
        assert_eq!(empty_out, "links (0):\n");
    }
}
