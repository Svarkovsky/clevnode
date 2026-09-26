//! A minimal harness that spawns the shared Python RNS test daemon
//! (`scripts/test_daemon.py`) and drives it over its JSON-RPC command port,
//! reusing the exact protocol of `leviculum-std`'s `rnsd_interop` harness.
//!
//! Tests use this to assert that the C API interoperates with the Python
//! Reticulum reference. If Python RNS is not available the daemon exits and
//! [`PyDaemon::start`] returns `None`, so the test skips cleanly.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use leviculum_std::process::spawn_supervised;

use super::port_alloc;

/// Path to the shared daemon script (sibling `scripts/` of the repo root).
fn daemon_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("scripts")
        .join("test_daemon.py")
}

pub struct PyDaemon {
    child: Child,
    pub rns_port: u16,
    pub cmd_port: u16,
}

impl PyDaemon {
    /// Spawn the daemon and wait for its READY handshake. Returns `None`
    /// (the test should skip) if Python RNS is unavailable or it never readies.
    pub fn start() -> Option<PyDaemon> {
        Self::start_with_args(&[])
    }

    /// Like [`start`], with extra daemon flags (e.g. `--echo-channel`).
    pub fn start_with_args(extra: &[&str]) -> Option<PyDaemon> {
        // The ports cross a process boundary through argv, so the daemon
        // cannot bind `:0` and hand a listener back; the numbers come from
        // the host-wide allocator band above the ephemeral range instead
        // (Codeberg #221 class 2 — class-1 conversion would need the daemon
        // to report its RNS listener's kernel-assigned port). The command
        // port additionally self-heals: the script falls back to `:0` when
        // its requested port is taken and reports the actual port in the
        // READY line, which `start_with_args` reads back below.
        let rns_port = port_alloc::free_tcp_port();
        let cmd_port = port_alloc::free_tcp_port();
        let script = daemon_script();
        if !script.exists() {
            eprintln!("skipping interop: {} not found", script.display());
            return None;
        }

        let mut cmd = Command::new("python3");
        cmd.arg(&script)
            .arg("--rns-port")
            .arg(rns_port.to_string())
            .arg("--cmd-port")
            .arg(cmd_port.to_string())
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // Supervised: the kernel kills this daemon when the test binary dies,
        // whatever it dies of. `Drop` below is the polite path on top.
        let mut child = spawn_supervised(cmd).ok()?;

        // Read stdout on a thread, signalling when "READY" appears or stdout
        // closes (the daemon exited, e.g. RNS not importable). READY carries
        // `READY <rns_port> <cmd_port>` — the ports the daemon actually uses,
        // which for cmd_port may differ from the request (see above).
        let stdout = child.stdout.take()?;
        let (tx, rx) = mpsc::channel::<Option<(u16, u16)>>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) if l.starts_with("READY ") => {
                        let mut fields = l.split_whitespace().skip(1);
                        let rns = fields.next().and_then(|f| f.parse().ok());
                        let cmd = fields.next().and_then(|f| f.parse().ok());
                        let _ = tx.send(rns.zip(cmd));
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = tx.send(None);
        });

        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Some((rns_port, cmd_port))) => {
                // Let interfaces settle, matching the reference harness.
                std::thread::sleep(Duration::from_millis(300));
                Some(PyDaemon {
                    child,
                    rns_port,
                    cmd_port,
                })
            }
            _ => {
                eprintln!("skipping interop: Python RNS daemon did not become ready");
                let _ = child.kill();
                None
            }
        }
    }

    /// Send one JSON-RPC command and return the parsed response value.
    ///
    /// Panics on any I/O or parse failure, which is what a test wants
    /// *during* a test. It is emphatically not what a destructor wants — see
    /// [`try_shutdown`](Self::try_shutdown) and the `Drop` below.
    fn query(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.try_query(method, params)
            .unwrap_or_else(|e| panic!("daemon {method}: {e}"))
    }

    /// The same round trip with every failure returned rather than raised.
    fn try_query(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let cmd = serde_json::json!({ "method": method, "params": params });
        let mut stream = TcpStream::connect(("127.0.0.1", self.cmd_port))?;
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        stream.write_all(cmd.to_string().as_bytes())?;
        stream.shutdown(std::net::Shutdown::Write)?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf)?;
        Ok(serde_json::from_slice(&buf)?)
    }

    /// Ask the daemon to stop, and swallow every way that can fail.
    ///
    /// A daemon that has already exited refuses the connection; one that is
    /// mid-shutdown answers with an empty body, which is not JSON. Neither is
    /// news to a destructor whose next line kills the process anyway.
    fn try_shutdown(&self) {
        let _ = self.try_query("shutdown", serde_json::json!({}));
    }

    fn result(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let v = self.query(method, params);
        if let Some(err) = v.get("error") {
            panic!("daemon {method} error: {err}");
        }
        v.get("result").cloned().unwrap_or(serde_json::Value::Null)
    }

    pub fn rns_addr(&self) -> String {
        format!("127.0.0.1:{}", self.rns_port)
    }

    /// Register a destination on the daemon; returns (hash, signing_key) as hex.
    pub fn register_destination(&self, app_name: &str, aspects: &[&str]) -> (String, String) {
        let r = self.result(
            "register_destination",
            serde_json::json!({ "app_name": app_name, "aspects": aspects }),
        );
        (
            r["hash"].as_str().unwrap().to_string(),
            r["signing_key"].as_str().unwrap().to_string(),
        )
    }

    pub fn announce_destination(&self, hash_hex: &str, app_data_hex: &str) {
        self.result(
            "announce_destination",
            serde_json::json!({ "hash": hash_hex, "app_data": app_data_hex }),
        );
    }

    pub fn has_path(&self, hash_hex: &str) -> bool {
        self.result("has_path", serde_json::json!({ "hash": hash_hex }))
            .as_bool()
            .unwrap_or(false)
    }

    /// Set the resource acceptance strategy for one of the daemon's
    /// destinations (`"accept_all"` / `"accept_none"`); apply before a link is
    /// established.
    pub fn set_resource_strategy(&self, dest_hash: &str, strategy: &str) {
        self.result(
            "set_resource_strategy",
            serde_json::json!({ "dest_hash": dest_hash, "strategy": strategy }),
        );
    }

    /// The resources the daemon has received, each a JSON object with `data`
    /// (hex), `metadata`, and `status`.
    pub fn received_resources(&self) -> Vec<serde_json::Value> {
        self.result("get_received_resources", serde_json::json!({}))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Verify an Ed25519 signature with the reference implementation. All
    /// arguments are hex; the public key is the 64-byte identity public key.
    pub fn verify_signature(&self, public_key: &str, message: &str, signature: &str) -> bool {
        self.result(
            "verify_signature",
            serde_json::json!({
                "public_key": public_key,
                "message": message,
                "signature": signature,
            }),
        )
        .get("valid")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    }

    /// Link hashes currently known to the daemon (hex).
    pub fn link_hashes(&self) -> Vec<String> {
        let r = self.result("get_links", serde_json::json!({}));
        r.as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Hex payloads of packets the daemon received over links.
    pub fn received_link_packets(&self) -> Vec<String> {
        let r = self.result("get_received_packets", serde_json::json!({}));
        r.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p["data"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn send_on_link(&self, link_hash_hex: &str, data_hex: &str) {
        self.result(
            "send_on_link",
            serde_json::json!({ "link_hash": link_hash_hex, "data": data_hex }),
        );
    }
}

/// A `Drop` that owns an external process must reach the kill on **every**
/// path.
///
/// This one did not, and it is the second, independent reason the daemon leaked
/// on 2026-08-07. `query("shutdown")` panicked on the empty response a
/// mid-shutdown daemon returns — `serde_json::from_slice(&[])` is an error and
/// the old code `expect`ed it — and a panic *inside a destructor that is itself
/// running during unwinding* is a non-unwinding panic: the process aborts, so
/// the `child.kill()` two lines down never ran, and neither did any other
/// destructor in the test. Seven orphaned daemons, the oldest four hours old,
/// and a gate held open for two.
///
/// So: try the polite shutdown, ignore every error it can produce, then kill
/// unconditionally. This pairs with `PR_SET_PDEATHSIG` rather than replacing
/// it — the kernel covers "the parent died", this covers "the parent lived and
/// its cleanup threw".
impl Drop for PyDaemon {
    fn drop(&mut self) {
        self.try_shutdown();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
