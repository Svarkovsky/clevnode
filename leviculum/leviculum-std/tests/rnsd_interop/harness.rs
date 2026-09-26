//! Test harness for spawning and managing the Python test daemon.
//!
//! This module provides infrastructure for running interop tests without
//! requiring a manually-started rnsd instance. Instead, it spawns a custom
//! Python test daemon that provides:
//!
//! 1. A Reticulum TCPServerInterface for packet exchange
//! 2. A JSON-RPC command interface for querying internal state
//!
//! # Example
//!
//! ```ignore
//! #[tokio::test]
//! async fn test_announce_creates_path() {
//!     let daemon = TestDaemon::start().await.expect("Failed to start daemon");
//!
//!     // Connect to Reticulum interface
//!     let mut stream = TcpStream::connect(daemon.rns_addr()).await.unwrap();
//!
//!     // Send announce
//!     let (raw, dest_hash, _) = build_announce_raw("test", &["echo"], b"data");
//!     send_framed(&mut stream, &raw).await;
//!
//!     // Query state directly
//!     tokio::time::sleep(Duration::from_millis(100)).await;
//!     assert!(daemon.has_path(&dest_hash).await);
//! }
//! ```

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Every daemon this module starts outlives the call that started it, so every
/// one of them goes through the supervised spawn: the kernel `SIGKILL`s it when
/// this test binary dies, whatever the test binary dies of. The `Drop`s below
/// are the polite path on top of that, not the guarantee. See
/// `leviculum_std::process` and `docs/src/concepts/checks-and-citations.md`.
use leviculum_std::process::spawn_supervised;

/// Host-wide listener-port allocator, shared with the `mvr` suite and with
/// `tests/discovery_autoconnect.rs`.
///
/// The counter behind it lives in a file rather than in this process, because
/// `cargo test --workspace` runs test binaries in parallel and a per-process
/// counter hands two of them the same numbers in the same order. The module
/// documents the handoff window and why the alternatives were rejected.
#[path = "../support/port_alloc.rs"]
#[allow(dead_code)]
pub mod port_alloc;

/// Error type for test harness operations
#[derive(Debug)]
pub enum HarnessError {
    /// Failed to spawn the daemon process
    SpawnFailed(std::io::Error),
    /// Daemon did not become ready in time
    StartupTimeout,
    /// Failed to parse daemon output
    ParseError(String),
    /// JSON-RPC command failed
    CommandFailed(String),
    /// Connection to daemon failed
    ConnectionFailed(std::io::Error),
    /// Failed to initialize git submodule
    SubmoduleInitFailed(String),
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HarnessError::SpawnFailed(e) => write!(f, "Failed to spawn daemon: {}", e),
            HarnessError::StartupTimeout => write!(f, "Daemon did not become ready in time"),
            HarnessError::ParseError(s) => write!(f, "Failed to parse daemon output: {}", s),
            HarnessError::CommandFailed(s) => write!(f, "JSON-RPC command failed: {}", s),
            HarnessError::ConnectionFailed(e) => write!(f, "Connection to daemon failed: {}", e),
            HarnessError::SubmoduleInitFailed(s) => {
                write!(f, "Failed to initialize Reticulum submodule: {}", s)
            }
        }
    }
}

impl std::error::Error for HarnessError {}

/// Ensure the Reticulum submodule is initialized if it exists.
///
/// This checks for the reference/Reticulum directory and initializes the submodule
/// if it exists but the RNS module is missing (indicating uninitialized state).
fn ensure_reticulum_submodule() -> Result<(), HarnessError> {
    // Skip if RETICULUM_PATH is set (user override)
    if std::env::var("RETICULUM_PATH").is_ok() {
        return Ok(());
    }

    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let vendor_dir = project_root.join("reference/Reticulum");
    let rns_path = vendor_dir.join("RNS");

    if rns_path.exists() {
        // Already initialized
        return Ok(());
    }

    if vendor_dir.exists() {
        // Directory exists but RNS not present - need to init submodule
        eprintln!("Initializing Reticulum submodule...");
        let output = Command::new("git")
            .current_dir(&project_root)
            .args(["submodule", "update", "--init", "reference/Reticulum"])
            .output()
            .map_err(|e| HarnessError::SubmoduleInitFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(HarnessError::SubmoduleInitFailed(format!(
                "git submodule update failed: {}",
                stderr
            )));
        }

        // Verify initialization succeeded
        if !rns_path.exists() {
            return Err(HarnessError::SubmoduleInitFailed(
                "Submodule initialized but RNS directory not found".to_string(),
            ));
        }
    }
    // If vendor dir doesn't exist at all, test_daemon.py will handle the error

    Ok(())
}

/// A handle to a running test daemon.
///
/// The daemon is automatically killed when this handle is dropped.
pub struct TestDaemon {
    process: Child,
    rns_port: u16,
    cmd_port: u16,
    /// UDP listen port (None if daemon has no UDP interface)
    udp_listen_port: Option<u16>,
    /// UDP forward port (None if daemon has no UDP interface)
    udp_forward_port: Option<u16>,
    /// Probe destination hash (hex, set when --respond-to-probes is used)
    probe_dest_hash: Option<String>,
    /// Snapshot of `len(Transport.interfaces)` taken once startup
    /// (READY + 200 ms sleep + cmd-port ping) completes — i.e. the
    /// daemon's "no peers yet" baseline before any test connects.
    /// Used by [`wait_for_peer_count`](Self::wait_for_peer_count) to
    /// wait for an *increase* rather than an absolute count, so the
    /// helper stays correct as new TestDaemon variants add interfaces
    /// (UDP, AutoInterface, future LoRa).
    initial_peer_count: usize,
}

impl TestDaemon {
    /// Path to the test daemon script
    const DAEMON_SCRIPT: &'static str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/test_daemon.py");

    /// Timeout for daemon startup
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

    /// Extract the actually-bound cmd port from the daemon's READY line.
    /// The daemon falls back to a kernel-allocated port when the requested
    /// port was stolen in the find_*_available_ports TOCTOU window, so the
    /// harness reads the post-bind value from the READY line rather than
    /// trusting the pre-spawn `cmd_port` argument.
    fn parse_ready_cmd_port(ready_line: &str) -> Result<u16, HarnessError> {
        let parts: Vec<&str> = ready_line.split_whitespace().collect();
        if parts.len() < 3 {
            return Err(HarnessError::ParseError(format!(
                "Invalid READY line: {ready_line}"
            )));
        }
        parts[2].parse::<u16>().map_err(|_| {
            HarnessError::ParseError(format!("Invalid cmd_port in READY line: {ready_line}"))
        })
    }

    /// Maximum retries for daemon startup (handles port race conditions)
    const MAX_STARTUP_RETRIES: u32 = 3;

    /// Start a new test daemon instance.
    ///
    /// This spawns the Python test daemon with dynamically allocated ports,
    /// waits for it to signal readiness, and returns a handle for interaction.
    ///
    /// If the Reticulum submodule exists but is not initialized, this will
    /// automatically run `git submodule update --init` first.
    ///
    /// Allocates ports via the host-wide counter in [`port_alloc`] (each call
    /// receives a number no other caller on this host will be given) and
    /// spawns the daemon. The retry loop remains as defence in depth against
    /// an occupant that never asked the allocator for anything; the TOCTOU
    /// race that previously caused intermittent `249/250` failures is
    /// eliminated by the allocation, not by retries.
    pub async fn start() -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;

        let mut last_error = HarnessError::StartupTimeout;

        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;

            match Self::start_with_ports(rns_port, cmd_port).await {
                Ok(daemon) => return Ok(daemon),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_error)
    }

    /// Start a daemon with specific ports (useful for debugging).
    pub async fn start_with_ports(rns_port: u16, cmd_port: u16) -> Result<Self, HarnessError> {
        Self::start_with_ports_and_options(rns_port, cmd_port, Vec::new()).await
    }

    /// Start a daemon with remote management (`rnstatus -R`) enabled, allowing
    /// the given hex identity hash to query `/status`. Mirrors Python's
    /// `enable_remote_management = yes` + `remote_management_allowed = <hash>`
    /// config (`Reticulum.py:548-561`, `Transport.py:253-259`). The daemon's own
    /// transport identity hash (needed to derive the management destination)
    /// is read via [`Self::get_transport_status`].
    pub async fn start_with_remote_management(allowed_hash: &str) -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;
        let extra = vec![
            "--enable-remote-management".to_string(),
            "--remote-management-allowed".to_string(),
            allowed_hash.to_string(),
        ];
        let mut last_error = HarnessError::StartupTimeout;
        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;
            match Self::start_with_ports_and_options(rns_port, cmd_port, extra.clone()).await {
                Ok(daemon) => return Ok(daemon),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_error)
    }

    /// Start a daemon with an override of Python's
    /// `Transport.mgmt_announce_interval`. Used by parity tests that need
    /// to observe the keepalive mechanism without waiting the default two
    /// hours. The interval is applied to the Python class attribute before
    /// `RNS.Reticulum` is constructed, so the first ~15 s initial-delay
    /// fire is unaffected and subsequent fires use the reduced cadence.
    pub async fn start_with_mgmt_interval(interval_secs: u32) -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;
        let mut last_error = HarnessError::StartupTimeout;
        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;
            let extra = vec![
                "--mgmt-announce-interval-seconds".to_string(),
                interval_secs.to_string(),
            ];
            match Self::start_with_ports_and_options(rns_port, cmd_port, extra).await {
                Ok(daemon) => return Ok(daemon),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_error)
    }

    /// Retry-wrapped start with arbitrary extra daemon args. Allocates a fresh
    /// `(rns_port, cmd_port)` pair per attempt.
    async fn start_with_retry_args(extra_args: Vec<String>) -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;
        let mut last_error = HarnessError::StartupTimeout;
        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;
            match Self::start_with_ports_and_options(rns_port, cmd_port, extra_args.clone()).await {
                Ok(daemon) => return Ok(daemon),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_error)
    }

    /// Common discovery args: a discoverable interface named `name` with the
    /// default stamp value (14) and a short announcer job interval so the REAL
    /// `RNS.Discovery.InterfaceAnnouncer` fires promptly (Codeberg #32).
    fn discovery_args(name: &str) -> Vec<String> {
        vec![
            "--discoverable".to_string(),
            "--discovery-name".to_string(),
            name.to_string(),
            "--discovery-stamp-value".to_string(),
            "14".to_string(),
            "--discovery-job-interval".to_string(),
            "2".to_string(),
        ]
    }

    /// Start a daemon whose main TCP server is discoverable (Codeberg #32). Its
    /// `reachable_on`/port advertise the main `rns_port` endpoint.
    pub async fn start_discoverable(name: &str) -> Result<Self, HarnessError> {
        Self::start_with_retry_args(Self::discovery_args(name)).await
    }

    /// Start a daemon that advertises encrypted discovery announces keyed by the
    /// shared 64-byte network identity at `network_identity_path` (Codeberg #32,
    /// sub-task d). Python generates the identity file if it does not yet exist.
    pub async fn start_discoverable_encrypted(
        name: &str,
        network_identity_path: &str,
    ) -> Result<Self, HarnessError> {
        let mut extra = Self::discovery_args(name);
        extra.push("--discovery-encrypt".to_string());
        extra.push("--network-identity".to_string());
        extra.push(network_identity_path.to_string());
        Self::start_with_retry_args(extra).await
    }

    /// Start a daemon with a discoverable SECOND TCP server on a distinct port,
    /// returned alongside the daemon. The main `rns_port` is a plain bootstrap
    /// link a peer connects to to hear the announce; the announce advertises the
    /// returned port, so an auto-connecting peer opens a genuinely new link to
    /// it. Mirrors the two-port topology of the Rust<->Rust auto-connect test.
    pub async fn start_discoverable_backbone(name: &str) -> Result<(Self, u16), HarnessError> {
        ensure_reticulum_submodule()?;
        let mut last_error = HarnessError::StartupTimeout;
        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;
            let backbone_port = pick_free_tcp_port()?;
            let mut extra = Self::discovery_args(name);
            extra.push("--discovery-port".to_string());
            extra.push(backbone_port.to_string());
            match Self::start_with_ports_and_options(rns_port, cmd_port, extra).await {
                Ok(daemon) => return Ok((daemon, backbone_port)),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_error)
    }

    /// Like [`start_discoverable_backbone`](Self::start_discoverable_backbone),
    /// but the discoverable backbone server is IFAC-protected with
    /// `netname`/`passphrase` and publishes them in its discovery announce
    /// (`publish_ifac = yes`, Codeberg #151). The main bootstrap server stays
    /// open, so a peer can hear the announce without holding the keys.
    pub async fn start_discoverable_backbone_ifac(
        name: &str,
        netname: &str,
        passphrase: &str,
    ) -> Result<(Self, u16), HarnessError> {
        ensure_reticulum_submodule()?;
        let mut last_error = HarnessError::StartupTimeout;
        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;
            let backbone_port = pick_free_tcp_port()?;
            let mut extra = Self::discovery_args(name);
            extra.push("--discovery-port".to_string());
            extra.push(backbone_port.to_string());
            extra.push("--ifac-netname".to_string());
            extra.push(netname.to_string());
            extra.push("--ifac-passphrase".to_string());
            extra.push(passphrase.to_string());
            extra.push("--discovery-publish-ifac".to_string());
            match Self::start_with_ports_and_options(rns_port, cmd_port, extra).await {
                Ok(daemon) => return Ok((daemon, backbone_port)),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_error)
    }

    /// Start a daemon running the InterfaceDiscovery *listener* (reverse
    /// direction): it populates its own discovered-interface registry from
    /// received discovery announces (`get_discovered_interfaces`). Plaintext.
    pub async fn start_discovering() -> Result<Self, HarnessError> {
        Self::start_with_retry_args(vec!["--discover-interfaces".to_string()]).await
    }

    /// Start a discovery listener whose stamp gate is `required_value` instead
    /// of the vendored default.
    ///
    /// This is how a *newer* Python listener is reproduced against the 1.3.5
    /// tree we vendor: RNS 1.5.0's discovery change is the default of the
    /// `required_discovery_value` config key (14 -> 16), and the stamp
    /// verification either side of that default is unchanged between the two
    /// versions. Setting the key explicitly therefore yields a listener that
    /// gates exactly as 1.5.0 does (Codeberg #328).
    pub async fn start_discovering_at_value(required_value: u32) -> Result<Self, HarnessError> {
        Self::start_with_retry_args(vec![
            "--discover-interfaces".to_string(),
            "--required-discovery-value".to_string(),
            required_value.to_string(),
        ])
        .await
    }

    /// Start a daemon running the InterfaceDiscovery listener keyed by the
    /// shared 64-byte network identity at `network_identity_path` (Codeberg
    /// #107, encrypted-reverse). Only encrypted announces sealed for this
    /// identity decrypt and surface; Python generates the identity file if it
    /// does not yet exist, so the Rust node can load the same file to encrypt.
    pub async fn start_discovering_encrypted(
        network_identity_path: &str,
    ) -> Result<Self, HarnessError> {
        Self::start_with_retry_args(vec![
            "--discover-interfaces".to_string(),
            "--network-identity".to_string(),
            network_identity_path.to_string(),
        ])
        .await
    }

    async fn start_with_ports_and_options(
        rns_port: u16,
        cmd_port: u16,
        extra_args: Vec<String>,
    ) -> Result<Self, HarnessError> {
        let mut args: Vec<String> = vec![
            Self::DAEMON_SCRIPT.to_string(),
            "--rns-port".to_string(),
            rns_port.to_string(),
            "--cmd-port".to_string(),
            cmd_port.to_string(),
        ];
        args.extend(extra_args);
        let mut cmd = Command::new("python3");
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut process = spawn_supervised(cmd).map_err(HarnessError::SpawnFailed)?;

        // Wait for "READY <rns_port> <cmd_port>" line
        let stdout = process.stdout.take().expect("stdout should be captured");
        let reader = BufReader::new(stdout);

        let ready_result = tokio::task::spawn_blocking(move || {
            for line in reader.lines() {
                match line {
                    Ok(line) if line.starts_with("READY ") => {
                        return Ok(line);
                    }
                    Ok(_) => continue,
                    Err(e) => return Err(HarnessError::ParseError(e.to_string())),
                }
            }
            Err(HarnessError::StartupTimeout)
        });

        let ready_line = timeout(Self::STARTUP_TIMEOUT, ready_result)
            .await
            .map_err(|_| HarnessError::StartupTimeout)?
            .map_err(|_| HarnessError::StartupTimeout)??;

        let cmd_port = Self::parse_ready_cmd_port(&ready_line)?;

        // Wait briefly for interfaces to fully initialize
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify we can connect to the command port
        let mut daemon = Self {
            process,
            rns_port,
            cmd_port,
            udp_listen_port: None,
            udp_forward_port: None,
            probe_dest_hash: None,
            initial_peer_count: 0,
        };

        // Ping to verify daemon is responsive
        match daemon.ping().await {
            Ok(_) => {
                daemon.snapshot_initial_peer_count().await;
                Ok(daemon)
            }
            Err(e) => {
                // Daemon didn't respond, clean up
                drop(daemon);
                Err(e)
            }
        }
    }

    /// Retry wrapper shared by the #102 serial-family starters. Allocates a
    /// fresh `(rns_port, cmd_port)` pair per attempt (the daemon keeps its TCP
    /// server + JSON-RPC port alongside the serial interface) and forwards the
    /// serial `extra_args` to the Python daemon.
    async fn start_serial_family(extra_args: Vec<String>) -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;

        let mut last_error = None;
        for _ in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;
            match Self::start_with_ports_and_options(rns_port, cmd_port, extra_args.clone()).await {
                Ok(daemon) => return Ok(daemon),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or(HarnessError::StartupTimeout))
    }

    /// Start a Python daemon whose only over-the-air interface is a
    /// `KISSInterface` bound to `pty` (Codeberg #102). The Rust lnsd holds the
    /// other pty end of the socat pair.
    pub async fn start_with_kiss_serial(pty: &str, speed: u32) -> Result<Self, HarnessError> {
        Self::start_serial_family(vec![
            "--serial-kind".to_string(),
            "kiss".to_string(),
            "--serial-port".to_string(),
            pty.to_string(),
            "--serial-speed".to_string(),
            speed.to_string(),
        ])
        .await
    }

    /// Start a Python daemon with an `AX25KISSInterface` bound to `pty`
    /// (Codeberg #102). `callsign`/`ssid` set the AX.25 source address; the
    /// tocall is Python's fixed `APZRNS-0`.
    pub async fn start_with_ax25_serial(
        pty: &str,
        speed: u32,
        callsign: &str,
        ssid: u8,
    ) -> Result<Self, HarnessError> {
        Self::start_serial_family(vec![
            "--serial-kind".to_string(),
            "ax25kiss".to_string(),
            "--serial-port".to_string(),
            pty.to_string(),
            "--serial-speed".to_string(),
            speed.to_string(),
            "--ax25-callsign".to_string(),
            callsign.to_string(),
            "--ax25-ssid".to_string(),
            ssid.to_string(),
        ])
        .await
    }

    /// Start a Python daemon with a `PipeInterface` running `command`
    /// (Codeberg #102). Pipe is a subprocess bridge, not a serial port, so this
    /// does not use a pty: `command` should be a stdio<->TCP bridge that meets
    /// the Rust lnsd's mirror bridge over loopback.
    pub async fn start_with_pipe(command: &str) -> Result<Self, HarnessError> {
        Self::start_serial_family(vec![
            "--serial-kind".to_string(),
            "pipe".to_string(),
            "--pipe-command".to_string(),
            command.to_string(),
        ])
        .await
    }

    /// Start a daemon with a UDP interface in addition to TCP.
    ///
    /// The daemon binds its UDP interface on `udp_listen_port` and forwards
    /// outgoing UDP packets to `udp_forward_port`. The Rust test should bind
    /// its UDP interface on `udp_forward_port` and forward to `udp_listen_port`.
    pub async fn start_with_udp() -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;

        let mut last_error = HarnessError::StartupTimeout;

        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port, udp_listen_port, udp_forward_port) =
                find_four_available_ports()?;

            match Self::start_with_udp_ports(rns_port, cmd_port, udp_listen_port, udp_forward_port)
                .await
            {
                Ok(daemon) => return Ok(daemon),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_error)
    }

    /// Start a daemon with specific TCP and UDP ports.
    pub async fn start_with_udp_ports(
        rns_port: u16,
        cmd_port: u16,
        udp_listen_port: u16,
        udp_forward_port: u16,
    ) -> Result<Self, HarnessError> {
        let mut cmd = Command::new("python3");
        cmd.args([
            Self::DAEMON_SCRIPT,
            "--rns-port",
            &rns_port.to_string(),
            "--cmd-port",
            &cmd_port.to_string(),
            "--udp-listen-port",
            &udp_listen_port.to_string(),
            "--udp-forward-port",
            &udp_forward_port.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
        let mut process = spawn_supervised(cmd).map_err(HarnessError::SpawnFailed)?;

        let stdout = process.stdout.take().expect("stdout should be captured");
        let reader = BufReader::new(stdout);

        let ready_result = tokio::task::spawn_blocking(move || {
            for line in reader.lines() {
                match line {
                    Ok(line) if line.starts_with("READY ") => {
                        return Ok(line);
                    }
                    Ok(_) => continue,
                    Err(e) => return Err(HarnessError::ParseError(e.to_string())),
                }
            }
            Err(HarnessError::StartupTimeout)
        });

        let ready_line = timeout(Self::STARTUP_TIMEOUT, ready_result)
            .await
            .map_err(|_| HarnessError::StartupTimeout)?
            .map_err(|_| HarnessError::StartupTimeout)??;

        let cmd_port = Self::parse_ready_cmd_port(&ready_line)?;

        // Wait briefly for interfaces to fully initialize
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut daemon = Self {
            process,
            rns_port,
            cmd_port,
            udp_listen_port: Some(udp_listen_port),
            udp_forward_port: Some(udp_forward_port),
            probe_dest_hash: None,
            initial_peer_count: 0,
        };

        match daemon.ping().await {
            Ok(_) => {
                daemon.snapshot_initial_peer_count().await;
                Ok(daemon)
            }
            Err(e) => {
                drop(daemon);
                Err(e)
            }
        }
    }

    /// Start a daemon with shared instance enabled (local Unix socket).
    ///
    /// The daemon will have a TCPServerInterface plus a LocalServerInterface
    /// listening on abstract Unix socket `\0rns/{instance_name}`.
    pub async fn start_with_shared_instance(instance_name: &str) -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;

        let mut last_error = HarnessError::StartupTimeout;

        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;

            match Self::start_with_shared_instance_ports(rns_port, cmd_port, instance_name).await {
                Ok(daemon) => return Ok(daemon),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_error)
    }

    /// Start a shared instance daemon on specific ports.
    pub async fn start_with_shared_instance_ports(
        rns_port: u16,
        cmd_port: u16,
        instance_name: &str,
    ) -> Result<Self, HarnessError> {
        Self::start_with_shared_instance_ports_opts(rns_port, cmd_port, instance_name, false).await
    }

    /// Start a shared instance daemon with echo-channel enabled.
    pub async fn start_with_shared_instance_echo(
        rns_port: u16,
        cmd_port: u16,
        instance_name: &str,
    ) -> Result<Self, HarnessError> {
        Self::start_with_shared_instance_ports_opts(rns_port, cmd_port, instance_name, true).await
    }

    async fn start_with_shared_instance_ports_opts(
        rns_port: u16,
        cmd_port: u16,
        instance_name: &str,
        echo_channel: bool,
    ) -> Result<Self, HarnessError> {
        let mut args = vec![
            Self::DAEMON_SCRIPT.to_string(),
            "--rns-port".to_string(),
            rns_port.to_string(),
            "--cmd-port".to_string(),
            cmd_port.to_string(),
            "--share-instance".to_string(),
            "--instance-name".to_string(),
            instance_name.to_string(),
        ];
        if echo_channel {
            args.push("--echo-channel".to_string());
        }
        let mut cmd = Command::new("python3");
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut process = spawn_supervised(cmd).map_err(HarnessError::SpawnFailed)?;

        let stdout = process.stdout.take().expect("stdout should be captured");
        let reader = BufReader::new(stdout);

        let ready_result = tokio::task::spawn_blocking(move || {
            for line in reader.lines() {
                match line {
                    Ok(line) if line.starts_with("READY ") => {
                        return Ok(line);
                    }
                    Ok(_) => continue,
                    Err(e) => return Err(HarnessError::ParseError(e.to_string())),
                }
            }
            Err(HarnessError::StartupTimeout)
        });

        let ready_line = timeout(Self::STARTUP_TIMEOUT, ready_result)
            .await
            .map_err(|_| HarnessError::StartupTimeout)?
            .map_err(|_| HarnessError::StartupTimeout)??;

        let cmd_port = Self::parse_ready_cmd_port(&ready_line)?;

        // Wait for local socket to be ready
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut daemon = Self {
            process,
            rns_port,
            cmd_port,
            udp_listen_port: None,
            udp_forward_port: None,
            probe_dest_hash: None,
            initial_peer_count: 0,
        };

        match daemon.ping().await {
            Ok(_) => {
                daemon.snapshot_initial_peer_count().await;
                Ok(daemon)
            }
            Err(e) => {
                drop(daemon);
                Err(e)
            }
        }
    }

    /// Start a daemon with `respond_to_probes` enabled.
    ///
    /// The daemon prints `PROBE_DEST:<hex>` to stdout before the READY line.
    /// The probe destination hash is stored and accessible via `probe_dest_hash()`.
    pub async fn start_with_probes() -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;

        let mut last_error = HarnessError::StartupTimeout;

        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;

            match Self::start_with_probes_ports(rns_port, cmd_port).await {
                Ok(daemon) => return Ok(daemon),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_error)
    }

    /// Start a daemon with `respond_to_probes` on specific ports.
    async fn start_with_probes_ports(rns_port: u16, cmd_port: u16) -> Result<Self, HarnessError> {
        let mut cmd = Command::new("python3");
        cmd.args([
            Self::DAEMON_SCRIPT,
            "--rns-port",
            &rns_port.to_string(),
            "--cmd-port",
            &cmd_port.to_string(),
            "--respond-to-probes",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
        let mut process = spawn_supervised(cmd).map_err(HarnessError::SpawnFailed)?;

        let stdout = process.stdout.take().expect("stdout should be captured");
        let reader = BufReader::new(stdout);

        // Parse both PROBE_DEST:<hex> and READY lines from stdout
        let ready_result = tokio::task::spawn_blocking(move || {
            let mut probe_hash = None;
            for line in reader.lines() {
                match line {
                    Ok(line) if line.starts_with("PROBE_DEST:") => {
                        probe_hash = Some(line["PROBE_DEST:".len()..].trim().to_string());
                    }
                    Ok(line) if line.starts_with("READY ") => {
                        return Ok((line, probe_hash));
                    }
                    Ok(_) => continue,
                    Err(e) => return Err(HarnessError::ParseError(e.to_string())),
                }
            }
            Err(HarnessError::StartupTimeout)
        });

        let (ready_line, probe_dest_hash) = timeout(Self::STARTUP_TIMEOUT, ready_result)
            .await
            .map_err(|_| HarnessError::StartupTimeout)?
            .map_err(|_| HarnessError::StartupTimeout)??;

        let cmd_port = Self::parse_ready_cmd_port(&ready_line)?;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut daemon = Self {
            process,
            rns_port,
            cmd_port,
            udp_listen_port: None,
            udp_forward_port: None,
            probe_dest_hash,
            initial_peer_count: 0,
        };

        match daemon.ping().await {
            Ok(_) => {
                daemon.snapshot_initial_peer_count().await;
                Ok(daemon)
            }
            Err(e) => {
                drop(daemon);
                Err(e)
            }
        }
    }

    /// Start a daemon whose TCPServerInterface is IFAC-protected with the
    /// given `network_name` / `passphrase` (and optional `ifac_size` in bits),
    /// with `respond_to_probes` on so the inbound announce direction is
    /// observable. Python derives the IFAC identity/key natively from these
    /// config keys, making this daemon the byte-for-byte reference for the
    /// Rust IFAC apply/verify path (Codeberg #90).
    pub async fn start_with_ifac(
        netname: &str,
        passphrase: &str,
        ifac_size_bits: Option<u32>,
    ) -> Result<Self, HarnessError> {
        ensure_reticulum_submodule()?;

        let mut last_error = HarnessError::StartupTimeout;

        for attempt in 0..Self::MAX_STARTUP_RETRIES {
            let (rns_port, cmd_port) = find_two_available_ports()?;

            match Self::start_with_ifac_ports(
                rns_port,
                cmd_port,
                netname,
                passphrase,
                ifac_size_bits,
            )
            .await
            {
                Ok(daemon) => return Ok(daemon),
                Err(HarnessError::StartupTimeout) => {
                    if attempt + 1 < Self::MAX_STARTUP_RETRIES {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    last_error = HarnessError::StartupTimeout;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_error)
    }

    async fn start_with_ifac_ports(
        rns_port: u16,
        cmd_port: u16,
        netname: &str,
        passphrase: &str,
        ifac_size_bits: Option<u32>,
    ) -> Result<Self, HarnessError> {
        let rns_port_s = rns_port.to_string();
        let cmd_port_s = cmd_port.to_string();
        let mut args: Vec<String> = vec![
            Self::DAEMON_SCRIPT.to_string(),
            "--rns-port".to_string(),
            rns_port_s,
            "--cmd-port".to_string(),
            cmd_port_s,
            "--respond-to-probes".to_string(),
            "--ifac-netname".to_string(),
            netname.to_string(),
            "--ifac-passphrase".to_string(),
            passphrase.to_string(),
        ];
        if let Some(bits) = ifac_size_bits {
            args.push("--ifac-size".to_string());
            args.push(bits.to_string());
        }

        let mut cmd = Command::new("python3");
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut process = spawn_supervised(cmd).map_err(HarnessError::SpawnFailed)?;

        let stdout = process.stdout.take().expect("stdout should be captured");
        let reader = BufReader::new(stdout);

        let ready_result = tokio::task::spawn_blocking(move || {
            let mut probe_hash = None;
            for line in reader.lines() {
                match line {
                    Ok(line) if line.starts_with("PROBE_DEST:") => {
                        probe_hash = Some(line["PROBE_DEST:".len()..].trim().to_string());
                    }
                    Ok(line) if line.starts_with("READY ") => {
                        return Ok((line, probe_hash));
                    }
                    Ok(_) => continue,
                    Err(e) => return Err(HarnessError::ParseError(e.to_string())),
                }
            }
            Err(HarnessError::StartupTimeout)
        });

        let (ready_line, probe_dest_hash) = timeout(Self::STARTUP_TIMEOUT, ready_result)
            .await
            .map_err(|_| HarnessError::StartupTimeout)?
            .map_err(|_| HarnessError::StartupTimeout)??;

        let cmd_port = Self::parse_ready_cmd_port(&ready_line)?;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut daemon = Self {
            process,
            rns_port,
            cmd_port,
            udp_listen_port: None,
            udp_forward_port: None,
            probe_dest_hash,
            initial_peer_count: 0,
        };

        match daemon.ping().await {
            Ok(_) => {
                daemon.snapshot_initial_peer_count().await;
                Ok(daemon)
            }
            Err(e) => {
                drop(daemon);
                Err(e)
            }
        }
    }

    /// Kill this daemon and restart it on the same ports.
    ///
    /// The new daemon is a fresh Python process with empty state, all
    /// previously registered destinations, established links, and path
    /// table entries are lost.  Any existing TCP streams from test code
    /// to the old daemon will be broken and must be reconnected.
    ///
    /// Both the RNS port and cmd port use SO_REUSEADDR, so the OS
    /// releases them immediately after the old process exits.
    pub async fn restart(&mut self) -> Result<(), HarnessError> {
        let rns_port = self.rns_port;
        let cmd_port = self.cmd_port;

        // Hard kill (simulates crash, no graceful shutdown RPC)
        let _ = self.process.kill();
        let _ = self.process.wait();

        // Brief pause for OS to release sockets
        tokio::time::sleep(Duration::from_millis(200)).await;

        // start_with_ports returns a full TestDaemon. We swap process
        // handles so the new_daemon's Drop runs on the old (dead) process
        //, harmless since it's already dead.
        let mut new_daemon = Self::start_with_ports(rns_port, cmd_port).await?;
        std::mem::swap(&mut self.process, &mut new_daemon.process);

        // new_daemon now holds the OLD (already dead) process handle.
        // Zero out its cmd_port so Drop's graceful-shutdown TCP connect
        // fails harmlessly instead of shutting down our new live daemon.
        new_daemon.cmd_port = 0;

        // The fresh daemon's baseline supersedes ours so a post-restart
        // wait_for_peer_count counts only NEW connections.
        self.initial_peer_count = new_daemon.initial_peer_count;

        Ok(())
    }

    /// Get the address for the Reticulum TCP interface.
    pub fn rns_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.rns_port))
    }

    /// Get the address for the JSON-RPC command interface.
    pub fn cmd_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.cmd_port))
    }

    /// Snapshot the daemon's `len(Transport.interfaces)` once startup
    /// has settled.  Called from each `start_*` constructor right after
    /// the cmd-port `ping` returns OK; the recorded value is the
    /// "no-peer" baseline that [`wait_for_peer_count`](Self::wait_for_peer_count)
    /// measures increases against.
    ///
    /// On JSON-RPC failure the baseline stays at 0 — the
    /// `wait_for_peer_count` helper then degrades to absolute-count
    /// semantics, which is harmless for the single-test-daemon
    /// topology this harness supports today.
    async fn snapshot_initial_peer_count(&mut self) {
        match self.get_interfaces().await {
            Ok(ifaces) => self.initial_peer_count = ifaces.len(),
            Err(e) => {
                tracing::debug!("snapshot_initial_peer_count: get_interfaces failed: {e:?}");
            }
        }
    }

    /// Wait until the daemon's `Transport.interfaces` list has grown
    /// by at least `n` entries since startup, or return
    /// [`HarnessError::StartupTimeout`] when `timeout` elapses.
    ///
    /// **Snapshot-and-wait-for-increase** semantics: the baseline is
    /// the snapshot taken at startup, NOT zero.  This makes the helper
    /// robust against TestDaemon variants that already have non-
    /// TCPServer interfaces (UDP, AutoInterface, future LoRa) — the
    /// caller asks "1 more peer than baseline" rather than "exactly 2
    /// total interfaces".
    ///
    /// Used to close the connect-vs-broadcast race on the test side
    /// (Codeberg #49 hypothesis E): after `node.start().await` plus
    /// `node.wait_for_interface_ready(...)`, call this with `n = 1`
    /// before the first `announce_destination` to guarantee the
    /// daemon's `RNS.Transport.interfaces` list contains the test's
    /// TCP-client peer (registered by the per-connection handler
    /// thread spawned by `ThreadingTCPServer`, see Phase A1 audit).
    pub async fn wait_for_peer_count(
        &self,
        n: usize,
        timeout: Duration,
    ) -> Result<(), HarnessError> {
        let target = self.initial_peer_count + n;
        let deadline = std::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(20);
        loop {
            if let Ok(ifaces) = self.get_interfaces().await {
                if ifaces.len() >= target {
                    return Ok(());
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(HarnessError::StartupTimeout);
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Get the daemon's UDP listen address (where it receives datagrams).
    pub fn udp_listen_addr(&self) -> Option<SocketAddr> {
        self.udp_listen_port
            .map(|p| SocketAddr::from(([127, 0, 0, 1], p)))
    }

    /// Get the daemon's UDP forward address (where it sends datagrams).
    /// This is where the Rust side should listen.
    pub fn udp_forward_addr(&self) -> Option<SocketAddr> {
        self.udp_forward_port
            .map(|p| SocketAddr::from(([127, 0, 0, 1], p)))
    }

    /// Get the RNS port number.
    pub fn rns_port(&self) -> u16 {
        self.rns_port
    }

    /// OS process id of the underlying `python3` daemon.
    ///
    /// Used by the TCP load test to sample the hub's `/proc/<pid>/status`
    /// VmRSS and `/proc/<pid>/fd` count when a Python `rnsd`-equivalent plays
    /// the hub role in the A/B comparison against our `lnsd`.
    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Get the probe destination hash (hex string), if available.
    ///
    /// Set when the daemon was started with `--respond-to-probes`.
    pub fn probe_dest_hash(&self) -> Option<&str> {
        self.probe_dest_hash.as_deref()
    }

    /// Send a JSON-RPC command to the daemon and return the result.
    async fn query(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, HarnessError> {
        let mut stream = TcpStream::connect(self.cmd_addr())
            .await
            .map_err(HarnessError::ConnectionFailed)?;

        let cmd = serde_json::json!({
            "method": method,
            "params": params
        });

        stream
            .write_all(cmd.to_string().as_bytes())
            .await
            .map_err(HarnessError::ConnectionFailed)?;

        stream
            .shutdown()
            .await
            .map_err(HarnessError::ConnectionFailed)?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(HarnessError::ConnectionFailed)?;

        let response: serde_json::Value = serde_json::from_slice(&response)
            .map_err(|e| HarnessError::ParseError(e.to_string()))?;

        if let Some(error) = response.get("error") {
            return Err(HarnessError::CommandFailed(error.to_string()));
        }

        Ok(response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Ping the daemon to verify it's responsive.
    pub async fn ping(&self) -> Result<(), HarnessError> {
        let result = self.query("ping", serde_json::json!({})).await?;
        if result == "pong" {
            Ok(())
        } else {
            Err(HarnessError::CommandFailed(format!(
                "Unexpected ping response: {}",
                result
            )))
        }
    }

    /// Check if a path exists to a destination.
    pub async fn has_path(&self, dest_hash: impl AsRef<[u8]>) -> bool {
        let hex_hash = hex::encode(dest_hash.as_ref());
        match self
            .query("has_path", serde_json::json!({"hash": hex_hash}))
            .await
        {
            Ok(serde_json::Value::Bool(b)) => b,
            _ => false,
        }
    }

    /// Get the path table from the daemon.
    pub async fn get_path_table(&self) -> Result<HashMap<String, PathEntry>, HarnessError> {
        let result = self.query("get_path_table", serde_json::json!({})).await?;
        let mut paths = HashMap::new();

        if let serde_json::Value::Object(map) = result {
            for (hash, entry) in map {
                let timestamp = entry.get("timestamp").and_then(|v| v.as_f64());
                let hops = entry.get("hops").and_then(|v| v.as_u64()).map(|v| v as u8);

                paths.insert(hash, PathEntry { timestamp, hops });
            }
        }

        Ok(paths)
    }

    /// Get interface information from the daemon.
    pub async fn get_interfaces(&self) -> Result<Vec<InterfaceInfo>, HarnessError> {
        let result = self.query("get_interfaces", serde_json::json!({})).await?;
        let mut interfaces = Vec::new();

        if let serde_json::Value::Array(arr) = result {
            for entry in arr {
                let name = entry
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let online = entry.get("online").and_then(|v| v.as_bool());
                let in_enabled = entry.get("IN").and_then(|v| v.as_bool());
                let out_enabled = entry.get("OUT").and_then(|v| v.as_bool());
                let held_announces = entry
                    .get("held_announces")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let burst_active = entry
                    .get("burst_active")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let bitrate = entry.get("bitrate").and_then(|v| v.as_u64());

                interfaces.push(InterfaceInfo {
                    name,
                    online,
                    in_enabled,
                    out_enabled,
                    held_announces,
                    burst_active,
                    bitrate,
                });
            }
        }

        Ok(interfaces)
    }

    /// Register a destination in the daemon that accepts links.
    ///
    /// Returns the destination hash and signing key.
    pub async fn register_destination(
        &self,
        app_name: &str,
        aspects: &[&str],
    ) -> Result<DestinationInfo, HarnessError> {
        let result = self
            .query(
                "register_destination",
                serde_json::json!({
                    "app_name": app_name,
                    "aspects": aspects,
                }),
            )
            .await?;

        let hash = result
            .get("hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing hash".to_string()))?
            .to_string();

        let public_key = result
            .get("public_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing public_key".to_string()))?
            .to_string();

        let signing_key = result
            .get("signing_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing signing_key".to_string()))?
            .to_string();

        Ok(DestinationInfo {
            hash,
            public_key,
            signing_key,
        })
    }

    /// Announce a registered destination.
    pub async fn announce_destination(
        &self,
        dest_hash: &str,
        app_data: &[u8],
    ) -> Result<(), HarnessError> {
        self.query(
            "announce_destination",
            serde_json::json!({
                "hash": dest_hash,
                "app_data": hex::encode(app_data),
            }),
        )
        .await?;
        Ok(())
    }

    /// Drive the REAL `RNS.Discovery.InterfaceAnnouncer` to emit one
    /// interface-discovery announce immediately (Codeberg #32). Returns the
    /// announcer's result map (`transport_id`, `network_id`, `bind_port`, ...).
    pub async fn emit_discovery_announce(&self) -> Result<serde_json::Value, HarnessError> {
        self.query("emit_discovery_announce", serde_json::json!({}))
            .await
    }

    /// Return this daemon's own discovered-interface registry (the `rnstatus -d`
    /// view), populated by its InterfaceDiscovery listener (Codeberg #32).
    pub async fn get_discovered_interfaces(&self) -> Result<Vec<serde_json::Value>, HarnessError> {
        let result = self
            .query("get_discovered_interfaces", serde_json::json!({}))
            .await?;
        match result {
            serde_json::Value::Array(a) => Ok(a),
            _ => Ok(Vec::new()),
        }
    }

    /// Get established links from the daemon.
    pub async fn get_links(&self) -> Result<HashMap<String, LinkInfo>, HarnessError> {
        let result = self.query("get_links", serde_json::json!({})).await?;
        let mut links = HashMap::new();

        if let serde_json::Value::Object(map) = result {
            for (hash, _entry) in map {
                links.insert(hash, LinkInfo);
            }
        }

        Ok(links)
    }

    /// Get packets received over links from the daemon.
    pub async fn get_received_packets(&self) -> Result<Vec<ReceivedPacket>, HarnessError> {
        let result = self
            .query("get_received_packets", serde_json::json!({}))
            .await?;
        let mut packets = Vec::new();

        if let serde_json::Value::Array(arr) = result {
            for entry in arr {
                let data = entry
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| hex::decode(s).unwrap_or_default())
                    .unwrap_or_default();

                packets.push(ReceivedPacket { data });
            }
        }

        Ok(packets)
    }

    /// Fetch the TCP load-test sink tally: for each source `client_id`, the
    /// number of distinct sequence numbers received and their min/max. Returns
    /// `(per_source, total)` where `per_source[client_id] = (distinct, min, max)`
    /// and `total` is the grand sum of distinct seqs across all sources.
    ///
    /// See `test_daemon.py::get_loadtest_stats`. Load-test single packets carry
    /// a `b"LT" + u32_le(client_id) + u32_le(seq)` plaintext header; the sink
    /// folds them into per-client seq sets rather than the unbounded
    /// `received_single_packets` list.
    pub async fn get_loadtest_stats(
        &self,
    ) -> Result<(HashMap<u32, (u32, Option<u32>, Option<u32>)>, u64), HarnessError> {
        let result = self
            .query("get_loadtest_stats", serde_json::json!({}))
            .await?;
        let mut per_source = HashMap::new();
        let total = result.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
        if let Some(serde_json::Value::Object(sources)) = result.get("sources") {
            for (id_str, entry) in sources {
                let Ok(client_id) = id_str.parse::<u32>() else {
                    continue;
                };
                let distinct = entry.get("distinct").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let min = entry.get("min").and_then(|v| v.as_u64()).map(|v| v as u32);
                let max = entry.get("max").and_then(|v| v.as_u64()).map(|v| v as u32);
                per_source.insert(client_id, (distinct, min, max));
            }
        }
        Ok((per_source, total))
    }

    /// Get single packets received at destinations (not via links).
    pub async fn get_received_single_packets(
        &self,
    ) -> Result<Vec<ReceivedSinglePacket>, HarnessError> {
        let result = self
            .query("get_received_single_packets", serde_json::json!({}))
            .await?;
        let mut packets = Vec::new();

        if let serde_json::Value::Array(arr) = result {
            for entry in arr {
                let dest_hash = entry
                    .get("dest_hash")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let data = entry
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| hex::decode(s).unwrap_or_default())
                    .unwrap_or_default();

                packets.push(ReceivedSinglePacket { dest_hash, data });
            }
        }

        Ok(packets)
    }

    /// Identify on a link (Python as initiator).
    ///
    /// Creates a fresh identity on Python side and calls link.identify().
    /// Returns the identity hash of the identity that was used.
    pub async fn identify_link(&self, link_hash: &str) -> Result<String, HarnessError> {
        let result = self
            .query(
                "identify_link",
                serde_json::json!({ "link_hash": link_hash }),
            )
            .await?;

        let identity_hash = result
            .get("identity_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing identity_hash".to_string()))?
            .to_string();

        Ok(identity_hash)
    }

    /// Get the remote identity for a link, if the peer has identified.
    ///
    /// Returns `Some(identity_hash_hex)` if the peer identified, `None` otherwise.
    pub async fn get_link_remote_identity(
        &self,
        link_hash: &str,
    ) -> Result<Option<String>, HarnessError> {
        let result = self
            .query(
                "get_link_remote_identity",
                serde_json::json!({ "link_hash": link_hash }),
            )
            .await?;

        if result.is_null() {
            return Ok(None);
        }

        let hash = result
            .get("identity_hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(hash)
    }

    /// Send a single (non-link) packet from Python to a remote destination.
    ///
    /// Requires that Python has already received an announce for the destination
    /// (so that `RNS.Identity.recall()` can find the identity).
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash (hex string)
    /// * `data` - The data to send (will be hex-encoded)
    pub async fn send_single_packet(
        &self,
        dest_hash: &str,
        data: &[u8],
    ) -> Result<(), HarnessError> {
        self.query(
            "send_single_packet",
            serde_json::json!({
                "dest_hash": dest_hash,
                "data": hex::encode(data),
            }),
        )
        .await?;
        Ok(())
    }

    /// Register a PLAIN destination (no identity, unencrypted broadcast).
    ///
    /// Returns the destination hash (hex string).
    pub async fn register_plain_destination(
        &self,
        app_name: &str,
        aspects: &[&str],
    ) -> Result<String, HarnessError> {
        let result = self
            .query(
                "register_plain_destination",
                serde_json::json!({
                    "app_name": app_name,
                    "aspects": aspects,
                }),
            )
            .await?;

        let hash = result
            .get("hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing hash".to_string()))?
            .to_string();

        Ok(hash)
    }

    /// Send a plain broadcast packet (unencrypted, no identity).
    pub async fn send_plain_packet(
        &self,
        app_name: &str,
        aspects: &[&str],
        data: &[u8],
    ) -> Result<(), HarnessError> {
        self.query(
            "send_plain_packet",
            serde_json::json!({
                "app_name": app_name,
                "aspects": aspects,
                "data": hex::encode(data),
            }),
        )
        .await?;
        Ok(())
    }

    /// Get plain broadcast packets received at PLAIN destinations.
    pub async fn get_received_plain_packets(
        &self,
    ) -> Result<Vec<ReceivedSinglePacket>, HarnessError> {
        let result = self
            .query("get_received_plain_packets", serde_json::json!({}))
            .await?;
        let mut packets = Vec::new();

        if let serde_json::Value::Array(arr) = result {
            for entry in arr {
                let dest_hash = entry
                    .get("dest_hash")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let data = entry
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| hex::decode(s).unwrap_or_default())
                    .unwrap_or_default();

                packets.push(ReceivedSinglePacket { dest_hash, data });
            }
        }

        Ok(packets)
    }

    /// Send data on an existing link (Python as sender).
    ///
    /// # Arguments
    /// * `link_hash` - The link hash (from create_link)
    /// * `data` - The data to send (will be hex-encoded)
    pub async fn send_on_link(&self, link_hash: &str, data: &[u8]) -> Result<(), HarnessError> {
        self.query(
            "send_on_link",
            serde_json::json!({
                "link_hash": link_hash,
                "data": hex::encode(data),
            }),
        )
        .await?;
        Ok(())
    }

    /// Create a link from this daemon to an external destination (Python as initiator).
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash (hex)
    /// * `dest_key` - The full 64-byte public key (hex)
    /// * `timeout` - Timeout in seconds for link establishment
    pub async fn create_link(
        &self,
        dest_hash: &str,
        dest_key: &str,
        timeout: u64,
    ) -> Result<String, HarnessError> {
        let result = self
            .query(
                "create_link",
                serde_json::json!({
                    "dest_hash": dest_hash,
                    "dest_key": dest_key,
                    "timeout": timeout,
                }),
            )
            .await?;

        let link_hash = result
            .get("link_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing link_hash".to_string()))?
            .to_string();

        Ok(link_hash)
    }

    /// Enable ratchets for a destination (forward secrecy).
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash to enable ratchets for
    pub async fn enable_ratchets(&self, dest_hash: &str) -> Result<RatchetInfo, HarnessError> {
        let result = self
            .query(
                "enable_ratchets",
                serde_json::json!({
                    "hash": dest_hash,
                }),
            )
            .await?;

        let enabled = result
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(RatchetInfo {
            enabled,
            count: None,
            latest_id: None,
        })
    }

    /// Enforce ratchets for a destination.
    ///
    /// When enforced, the destination rejects packets not encrypted with a ratchet key,
    /// even if they could be decrypted with the identity key.
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash to enforce ratchets for
    pub async fn enforce_ratchets(&self, dest_hash: &str) -> Result<bool, HarnessError> {
        let result = self
            .query(
                "enforce_ratchets",
                serde_json::json!({
                    "hash": dest_hash,
                }),
            )
            .await?;
        Ok(result
            .get("enforced")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    /// Get ratchet state for a destination.
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash to query
    pub async fn get_ratchet_info(&self, dest_hash: &str) -> Result<RatchetInfo, HarnessError> {
        let result = self
            .query(
                "get_ratchet_info",
                serde_json::json!({
                    "hash": dest_hash,
                }),
            )
            .await?;

        let enabled = result
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let count = result
            .get("count")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let latest_id = result
            .get("latest_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(RatchetInfo {
            enabled,
            count,
            latest_id,
        })
    }

    /// Add a TCPClientInterface to connect to another daemon.
    ///
    /// # Arguments
    /// * `target_ip` - The IP address to connect to
    /// * `target_port` - The port to connect to
    /// * `name` - Optional name for the interface
    pub async fn add_client_interface(
        &self,
        target_ip: &str,
        target_port: u16,
        name: Option<&str>,
    ) -> Result<ClientInterfaceInfo, HarnessError> {
        let mut params = serde_json::json!({
            "target_ip": target_ip,
            "target_port": target_port,
        });

        if let Some(n) = name {
            params["name"] = serde_json::json!(n);
        }

        let _result = self.query("add_client_interface", params).await?;

        Ok(ClientInterfaceInfo)
    }

    /// Add a Python `TCPClientInterface` with per-interface announce-rate
    /// limiting configured (Codeberg #92). Makes this daemon a Python reference
    /// relay whose rebroadcast rate limiting can be compared against our own.
    /// `target`/`grace`/`penalty` are seconds / count / seconds.
    pub async fn add_client_interface_rate_limited(
        &self,
        target_ip: &str,
        target_port: u16,
        name: Option<&str>,
        announce_rate_target: u32,
        announce_rate_grace: u32,
        announce_rate_penalty: u32,
    ) -> Result<ClientInterfaceInfo, HarnessError> {
        let mut params = serde_json::json!({
            "target_ip": target_ip,
            "target_port": target_port,
            "announce_rate_target": announce_rate_target,
            "announce_rate_grace": announce_rate_grace,
            "announce_rate_penalty": announce_rate_penalty,
        });

        if let Some(n) = name {
            params["name"] = serde_json::json!(n);
        }

        let _result = self.query("add_client_interface", params).await?;

        Ok(ClientInterfaceInfo)
    }

    /// Add a Python `TCPClientInterface` with a configured `bitrate` (Codeberg
    /// #93). Mirrors Python's `configured_bitrate` override
    /// (Reticulum.py:887): the value replaces the interface's medium default and
    /// is reported back via `get_interfaces`, giving a reference to compare our
    /// effective-bitrate handling against.
    pub async fn add_client_interface_with_bitrate(
        &self,
        target_ip: &str,
        target_port: u16,
        name: Option<&str>,
        bitrate: u64,
    ) -> Result<ClientInterfaceInfo, HarnessError> {
        let mut params = serde_json::json!({
            "target_ip": target_ip,
            "target_port": target_port,
            "bitrate": bitrate,
        });

        if let Some(n) = name {
            params["name"] = serde_json::json!(n);
        }

        let _result = self.query("add_client_interface", params).await?;

        Ok(ClientInterfaceInfo)
    }

    /// Add a real Python `BackboneClientInterface` connecting to another daemon's
    /// TCP listener (Codeberg #89). Backbone is wire-identical to TCP, so this
    /// drives the reference Backbone interface against our lnsd TCP server built
    /// from a `type = BackboneInterface` config.
    pub async fn add_backbone_client_interface(
        &self,
        target_ip: &str,
        target_port: u16,
        name: Option<&str>,
    ) -> Result<ClientInterfaceInfo, HarnessError> {
        let mut params = serde_json::json!({
            "target_ip": target_ip,
            "target_port": target_port,
        });

        if let Some(n) = name {
            params["name"] = serde_json::json!(n);
        }

        let _result = self.query("add_backbone_client_interface", params).await?;

        Ok(ClientInterfaceInfo)
    }

    /// Get transport/routing status.
    pub async fn get_transport_status(&self) -> Result<TransportStatus, HarnessError> {
        let result = self
            .query("get_transport_status", serde_json::json!({}))
            .await?;

        let enabled = result
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let identity_hash = result
            .get("identity_hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let path_table_size = result
            .get("path_table_size")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let interface_count = result
            .get("interface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        Ok(TransportStatus {
            enabled,
            identity_hash,
            path_table_size,
            interface_count,
        })
    }

    /// Get the link table from the daemon.
    pub async fn get_link_table(&self) -> Result<HashMap<String, LinkTableEntry>, HarnessError> {
        let result = self.query("get_link_table", serde_json::json!({})).await?;
        let mut link_table = HashMap::new();

        if let serde_json::Value::Object(map) = result {
            for (link_id, _entry) in map {
                link_table.insert(link_id, LinkTableEntry);
            }
        }

        Ok(link_table)
    }

    /// Fetch the Python transport tunnel table (Codeberg #64). Maps each
    /// `tunnel_id` (hex) to `{interface, path_count, expires}`. A non-empty
    /// entry proves Python validated a synthesize we sent and (re)established
    /// the tunnel.
    pub async fn get_tunnels(&self) -> Result<HashMap<String, serde_json::Value>, HarnessError> {
        let result = self.query("get_tunnels", serde_json::json!({})).await?;
        let mut tunnels = HashMap::new();
        if let serde_json::Value::Object(map) = result {
            for (tunnel_id, entry) in map {
                tunnels.insert(tunnel_id, entry);
            }
        }
        Ok(tunnels)
    }

    /// Rotate the ratchet for a destination.
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash to rotate ratchets for
    pub async fn rotate_ratchet(
        &self,
        dest_hash: &str,
    ) -> Result<RatchetRotationResult, HarnessError> {
        let result = self
            .query(
                "rotate_ratchet",
                serde_json::json!({
                    "hash": dest_hash,
                }),
            )
            .await?;

        let rotated = result
            .get("rotated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let ratchet_count = result
            .get("ratchet_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let new_ratchet_id = result
            .get("new_ratchet_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(RatchetRotationResult {
            rotated,
            ratchet_count,
            new_ratchet_id,
        })
    }

    /// Close a link gracefully via RPC.
    ///
    /// # Arguments
    /// * `link_hash` - The link hash (hex string)
    pub async fn close_link(&self, link_hash: &str) -> Result<String, HarnessError> {
        let result = self
            .query(
                "close_link",
                serde_json::json!({
                    "link_hash": link_hash,
                }),
            )
            .await?;

        let status = result
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(status)
    }

    /// Get detailed link status via RPC.
    ///
    /// # Arguments
    /// * `link_hash` - The link hash (hex string)
    pub async fn get_link_status(&self, link_hash: &str) -> Result<LinkStatusInfo, HarnessError> {
        let result = self
            .query(
                "get_link_status",
                serde_json::json!({
                    "link_hash": link_hash,
                }),
            )
            .await?;

        let status = result
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let link_hash_result = result
            .get("link_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let state = result
            .get("state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mtu = result.get("mtu").and_then(|v| v.as_u64()).map(|v| v as u32);
        let mdu = result.get("mdu").and_then(|v| v.as_u64()).map(|v| v as u32);

        Ok(LinkStatusInfo {
            status,
            link_hash: link_hash_result,
            state,
            mtu,
            mdu,
        })
    }

    /// Set proof strategy for a destination.
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash (hex string)
    /// * `strategy` - The proof strategy: "PROVE_NONE", "PROVE_APP", or "PROVE_ALL"
    pub async fn set_proof_strategy(
        &self,
        dest_hash: &str,
        strategy: &str,
    ) -> Result<(), HarnessError> {
        self.query(
            "set_proof_strategy",
            serde_json::json!({
                "hash": dest_hash,
                "strategy": strategy,
            }),
        )
        .await?;
        Ok(())
    }

    /// Get proof strategy for a destination.
    ///
    /// # Arguments
    /// * `dest_hash` - The destination hash (hex string)
    ///
    /// # Returns
    /// The proof strategy as a string: "PROVE_NONE", "PROVE_APP", or "PROVE_ALL"
    pub async fn get_proof_strategy(&self, dest_hash: &str) -> Result<String, HarnessError> {
        let result = self
            .query(
                "get_proof_strategy",
                serde_json::json!({
                    "hash": dest_hash,
                }),
            )
            .await?;

        let strategy = result
            .get("strategy")
            .and_then(|v| v.as_str())
            .unwrap_or("PROVE_NONE")
            .to_string();

        Ok(strategy)
    }

    /// Encrypt plaintext through a Python GROUP destination with a shared key.
    ///
    /// Drives the real Python GROUP branch (`Destination.encrypt` over an
    /// `RNS.Cryptography.Token`). `key` is the 64-byte shared symmetric key.
    /// Returns the RNS Token wire bytes.
    pub async fn group_encrypt(
        &self,
        key: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, HarnessError> {
        let result = self
            .query(
                "group_encrypt",
                serde_json::json!({
                    "key": hex::encode(key),
                    "plaintext": hex::encode(plaintext),
                }),
            )
            .await?;

        let ct_hex = result
            .get("ciphertext")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("missing ciphertext".into()))?;
        hex::decode(ct_hex).map_err(|e| HarnessError::ParseError(e.to_string()))
    }

    /// Decrypt an RNS Token through a Python GROUP destination with a shared key.
    ///
    /// Drives the real Python GROUP branch (`Destination.decrypt`). `key` is the
    /// 64-byte shared symmetric key. Returns the recovered plaintext, or a
    /// `CommandFailed` error if Python could not decrypt (bad HMAC / wrong key).
    pub async fn group_decrypt(
        &self,
        key: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, HarnessError> {
        let result = self
            .query(
                "group_decrypt",
                serde_json::json!({
                    "key": hex::encode(key),
                    "ciphertext": hex::encode(ciphertext),
                }),
            )
            .await?;

        let pt = result
            .get("plaintext")
            .ok_or_else(|| HarnessError::ParseError("missing plaintext".into()))?;
        if pt.is_null() {
            return Err(HarnessError::CommandFailed(
                "Python GROUP destination could not decrypt".into(),
            ));
        }
        let pt_hex = pt
            .as_str()
            .ok_or_else(|| HarnessError::ParseError("plaintext not a string".into()))?;
        hex::decode(pt_hex).map_err(|e| HarnessError::ParseError(e.to_string()))
    }

    /// Get detailed announce table entries from the daemon.
    ///
    /// Returns the full announce_table with rebroadcast info (timestamp,
    /// retransmit_timeout, retries, received_from, hops, local_rebroadcasts, etc.)
    pub async fn get_announce_table_detail(
        &self,
    ) -> Result<HashMap<String, AnnounceTableDetail>, HarnessError> {
        let result = self
            .query("get_announce_table_detail", serde_json::json!({}))
            .await?;
        let mut table = HashMap::new();

        if let serde_json::Value::Object(map) = result {
            for (hash, entry) in map {
                table.insert(
                    hash,
                    AnnounceTableDetail {
                        local_rebroadcasts: entry
                            .get("local_rebroadcasts")
                            .and_then(|v| v.as_u64()),
                        block_rebroadcasts: entry
                            .get("block_rebroadcasts")
                            .and_then(|v| v.as_bool()),
                    },
                );
            }
        }

        Ok(table)
    }

    /// Wait for a link to reach a specific state.
    ///
    /// # Arguments
    /// * `link_hash` - The link hash (hex string)
    /// * `state` - The expected state (e.g., "ACTIVE", "CLOSED")
    /// * `timeout_secs` - Maximum time to wait
    pub async fn wait_for_link_state(
        &self,
        link_hash: &str,
        state: &str,
        timeout_secs: u64,
    ) -> Result<WaitForLinkStateResult, HarnessError> {
        let result = self
            .query(
                "wait_for_link_state",
                serde_json::json!({
                    "link_hash": link_hash,
                    "state": state,
                    "timeout": timeout_secs,
                }),
            )
            .await?;

        let status = result
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let state_result = result
            .get("state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(WaitForLinkStateResult {
            status,
            state: state_result,
        })
    }

    /// Enable LRPROOF dropping on the relay.
    ///
    /// Monkey-patches `Transport.transmit` on the daemon to silently drop
    /// packets whose context byte is 0xFF (LRPROOF).
    pub async fn enable_lrproof_drop(&self) -> Result<(), HarnessError> {
        self.query("enable_lrproof_drop", serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Disable LRPROOF dropping on the relay.
    ///
    /// Restores the original `Transport.transmit` saved during enable.
    pub async fn disable_lrproof_drop(&self) -> Result<(), HarnessError> {
        self.query("disable_lrproof_drop", serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Get the list of LRPROOF packets that were dropped by the relay.
    pub async fn get_lrproof_drops(&self) -> Result<Vec<serde_json::Value>, HarnessError> {
        let result = self
            .query("get_lrproof_drops", serde_json::json!({}))
            .await?;

        match result {
            serde_json::Value::Array(arr) => Ok(arr),
            _ => Ok(vec![]),
        }
    }

    /// Set the resource acceptance strategy for a destination.
    ///
    /// Must be called BEFORE `create_link` so that `_on_link_established`
    /// picks up the strategy and configures the link.
    pub async fn set_resource_strategy(
        &self,
        dest_hash: &str,
        strategy: &str,
    ) -> Result<serde_json::Value, HarnessError> {
        self.query(
            "set_resource_strategy",
            serde_json::json!({
                "dest_hash": dest_hash,
                "strategy": strategy,
            }),
        )
        .await
    }

    /// Register an echo request handler on a destination.
    ///
    /// The handler echoes back whatever data was sent in the request.
    pub async fn register_echo_request_handler(
        &self,
        dest_hash: &str,
        path: &str,
    ) -> Result<serde_json::Value, HarnessError> {
        self.query(
            "register_echo_request_handler",
            serde_json::json!({
                "dest_hash": dest_hash,
                "path": path,
            }),
        )
        .await
    }

    /// Register the NomadNet-style page node on a destination: `/page/small.mu`
    /// (single-packet response), `/page/large.mu` (is_response Resource) and
    /// `/page/echo.mu` (echoes the request data). Mirrors NomadNet's
    /// `register_request_handler` page serving.
    pub async fn register_page_request_handler(
        &self,
        dest_hash: &str,
    ) -> Result<serde_json::Value, HarnessError> {
        self.query(
            "register_page_request_handler",
            serde_json::json!({ "dest_hash": dest_hash }),
        )
        .await
    }

    /// Fetch the exact bytes the page node serves for a path, so a client can
    /// assert byte-identity against what it fetched.
    pub async fn get_page_content(&self, path: &str) -> Result<Vec<u8>, HarnessError> {
        let result = self
            .query("get_page_content", serde_json::json!({ "path": path }))
            .await?;
        let hex_str = result
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing page content".to_string()))?;
        hex::decode(hex_str).map_err(|e| HarnessError::ParseError(format!("bad page hex: {e}")))
    }

    /// Send a resource over an established link.
    ///
    /// Returns the resource hash (hex string).
    pub async fn send_resource(
        &self,
        link_hash: &str,
        data: &[u8],
        metadata: Option<&[u8]>,
    ) -> Result<String, HarnessError> {
        let mut params = serde_json::json!({
            "link_hash": link_hash,
            "data": hex::encode(data),
        });
        if let Some(meta) = metadata {
            params["metadata"] = serde_json::Value::String(hex::encode(meta));
        }
        let result = self.query("send_resource", params).await?;

        let resource_hash = result
            .get("resource_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing resource_hash".to_string()))?
            .to_string();

        Ok(resource_hash)
    }

    /// Get all received resources (completed and failed).
    pub async fn get_received_resources(&self) -> Result<Vec<ReceivedResource>, HarnessError> {
        let result = self
            .query("get_received_resources", serde_json::json!({}))
            .await?;

        let arr = match result {
            serde_json::Value::Array(arr) => arr,
            _ => return Ok(vec![]),
        };

        let mut resources = Vec::new();
        for item in arr {
            let data_hex = item.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let data = hex::decode(data_hex).unwrap_or_default();
            let metadata = item
                .get("metadata")
                .and_then(|v| v.as_str())
                .map(|h| hex::decode(h).unwrap_or_default());
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            resources.push(ReceivedResource {
                data,
                metadata,
                status,
            });
        }

        Ok(resources)
    }

    /// Send `RNS.Packet(link, data)` from the Python side and keep the
    /// PacketReceipt for status polling. Returns `(receipt_id, packet_hash)`.
    pub async fn send_on_link_tracked(
        &self,
        link_hash: &str,
        data: &[u8],
    ) -> Result<(String, String), HarnessError> {
        let result = self
            .query(
                "send_on_link_tracked",
                serde_json::json!({
                    "link_hash": link_hash,
                    "data": hex::encode(data),
                }),
            )
            .await?;
        let receipt_id = result
            .get("receipt_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing receipt_id".to_string()))?
            .to_string();
        let packet_hash = result
            .get("packet_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing packet_hash".to_string()))?
            .to_string();
        Ok((receipt_id, packet_hash))
    }

    /// Poll the status of a tracked PacketReceipt: SENT / DELIVERED / FAILED.
    pub async fn get_receipt_status(&self, receipt_id: &str) -> Result<String, HarnessError> {
        let result = self
            .query(
                "get_receipt_status",
                serde_json::json!({ "receipt_id": receipt_id }),
            )
            .await?;
        result
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| HarnessError::ParseError("Missing status".to_string()))
    }

    /// Issue a Python-side `Link.request()` (non-blocking). `data` must be a
    /// single msgpack value (Python packs bytes via umsgpack before the call
    /// site, so pass the raw bytes here; the daemon sends them as `data=`).
    pub async fn send_link_request(
        &self,
        link_hash: &str,
        path: &str,
        data: Option<&[u8]>,
        timeout_secs: Option<f64>,
    ) -> Result<String, HarnessError> {
        let mut params = serde_json::json!({
            "link_hash": link_hash,
            "path": path,
        });
        if let Some(d) = data {
            params["data"] = serde_json::Value::String(hex::encode(d));
        }
        if let Some(t) = timeout_secs {
            params["timeout"] = serde_json::json!(t);
        }
        let result = self.query("send_link_request", params).await?;
        result
            .get("request_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| HarnessError::ParseError("Missing request_id".to_string()))
    }

    /// Poll a Python RequestReceipt: returns `(status, response_bytes)`.
    /// Status is FAILED / SENT / DELIVERED / RECEIVING / READY; the response
    /// is only present on READY (bytes responses come back verbatim).
    pub async fn get_request_status(
        &self,
        request_id: &str,
    ) -> Result<(String, Option<Vec<u8>>), HarnessError> {
        let result = self
            .query(
                "get_request_status",
                serde_json::json!({ "request_id": request_id }),
            )
            .await?;
        let status = result
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing status".to_string()))?
            .to_string();
        let response = result
            .get("response")
            .and_then(|v| v.as_str())
            .map(|h| hex::decode(h).unwrap_or_default());
        Ok((status, response))
    }

    /// Initialize a real Python LXMF client (LXMRouter + delivery identity)
    /// on this daemon. Returns the delivery destination info.
    pub async fn lxmf_init(
        &self,
        display_name: &str,
        stamp_cost: Option<u8>,
    ) -> Result<LxmfClientInfo, HarnessError> {
        let mut params = serde_json::json!({ "display_name": display_name });
        if let Some(cost) = stamp_cost {
            params["stamp_cost"] = serde_json::json!(cost);
        }
        let result = self.query("lxmf_init", params).await?;
        let delivery_hash = result
            .get("delivery_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HarnessError::ParseError("Missing delivery_hash".to_string()))?
            .to_string();
        Ok(LxmfClientInfo { delivery_hash })
    }

    /// Announce the Python LXMF delivery destination.
    pub async fn lxmf_announce(&self) -> Result<(), HarnessError> {
        self.query("lxmf_announce", serde_json::json!({})).await?;
        Ok(())
    }

    /// Send an LXMF message from the Python router. `method` is
    /// "opportunistic" | "direct" | "propagated"; `fields` is an optional
    /// msgpack-encoded map. Returns the message hash (hex).
    pub async fn lxmf_send(
        &self,
        dest_hash: &str,
        method: &str,
        content: &[u8],
        title: &[u8],
        fields: Option<&[u8]>,
    ) -> Result<String, HarnessError> {
        let mut params = serde_json::json!({
            "dest_hash": dest_hash,
            "method": method,
            "content": hex::encode(content),
            "title": hex::encode(title),
        });
        if let Some(f) = fields {
            params["fields"] = serde_json::Value::String(hex::encode(f));
        }
        let result = self.query("lxmf_send", params).await?;
        result
            .get("message_hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| HarnessError::ParseError("Missing message_hash".to_string()))
    }

    /// Poll the Python-side outbound LXMessage state (OUTBOUND / SENT /
    /// DELIVERED / FAILED / ...).
    pub async fn lxmf_get_outbound_status(
        &self,
        message_hash: &str,
    ) -> Result<String, HarnessError> {
        let result = self
            .query(
                "lxmf_get_outbound_status",
                serde_json::json!({ "message_hash": message_hash }),
            )
            .await?;
        result
            .get("state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| HarnessError::ParseError("Missing state".to_string()))
    }

    /// Codeberg #156 latency probe: outbound LXMessage state plus every
    /// timestamped state transition recorded on the Python side (epoch
    /// seconds), and Python's current wall clock for skew checks.
    pub async fn lxmf_get_outbound_probe(
        &self,
        message_hash: &str,
    ) -> Result<serde_json::Value, HarnessError> {
        self.query(
            "lxmf_get_outbound_probe",
            serde_json::json!({ "message_hash": message_hash }),
        )
        .await
    }

    /// Fetch all LXMF messages the Python router has delivered so far.
    pub async fn lxmf_get_received(&self) -> Result<Vec<LxmfReceived>, HarnessError> {
        let result = self
            .query("lxmf_get_received", serde_json::json!({}))
            .await?;
        let arr = match result {
            serde_json::Value::Array(arr) => arr,
            _ => return Ok(vec![]),
        };
        let mut messages = Vec::new();
        for item in arr {
            let hex_field = |key: &str| -> Vec<u8> {
                item.get(key)
                    .and_then(|v| v.as_str())
                    .map(|h| hex::decode(h).unwrap_or_default())
                    .unwrap_or_default()
            };
            messages.push(LxmfReceived {
                message_hash: item
                    .get("message_hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                content: hex_field("content"),
                title: hex_field("title"),
                fields: hex_field("fields"),
                source_hash: item
                    .get("source_hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                destination_hash: item
                    .get("destination_hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                timestamp: item
                    .get("timestamp")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                signature_validated: item
                    .get("signature_validated")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                stamp_valid: item.get("stamp_valid").and_then(|v| v.as_bool()),
                stamp_value: item.get("stamp_value").and_then(|v| v.as_u64()),
                method: item
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(messages)
    }

    /// Turn this daemon's LXMRouter into a propagation node (the same
    /// `enable_propagation()` path `lxmd` drives). Returns the propagation
    /// destination hash (hex).
    pub async fn lxmf_enable_propagation(&self) -> Result<String, HarnessError> {
        let result = self
            .query("lxmf_enable_propagation", serde_json::json!({}))
            .await?;
        result
            .get("propagation_hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| HarnessError::ParseError("Missing propagation_hash".to_string()))
    }

    /// Announce the propagation node destination.
    pub async fn lxmf_announce_propagation_node(&self) -> Result<(), HarnessError> {
        self.query("lxmf_announce_propagation_node", serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Point the Python LXMF client at a propagation node (by dest hash hex).
    /// Errors until the node's identity is known from an announce.
    pub async fn lxmf_set_propagation_node(&self, dest_hash: &str) -> Result<(), HarnessError> {
        self.query(
            "lxmf_set_propagation_node",
            serde_json::json!({ "dest_hash": dest_hash }),
        )
        .await?;
        Ok(())
    }
}

/// Python LXMF client info returned by `lxmf_init`.
#[derive(Debug, Clone)]
pub struct LxmfClientInfo {
    /// Delivery destination hash (hex).
    pub delivery_hash: String,
}

/// One LXMF message recorded by the Python delivery callback.
#[derive(Debug, Clone)]
pub struct LxmfReceived {
    pub message_hash: String,
    pub content: Vec<u8>,
    pub title: Vec<u8>,
    /// msgpack-encoded fields map (umsgpack.packb of the delivered dict).
    pub fields: Vec<u8>,
    pub source_hash: String,
    pub destination_hash: String,
    pub timestamp: f64,
    pub signature_validated: bool,
    pub stamp_valid: Option<bool>,
    pub stamp_value: Option<u64>,
    pub method: String,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        // Try graceful shutdown first
        if let Ok(mut stream) = StdTcpStream::connect(self.cmd_addr()) {
            let cmd = r#"{"method":"shutdown"}"#;
            let _ = std::io::Write::write_all(&mut stream, cmd.as_bytes());
        }

        // Give it a moment to shut down
        std::thread::sleep(Duration::from_millis(100));

        // Force kill if still running
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// A linked pseudo-terminal pair created by `socat` (Codeberg #102).
///
/// Running `socat -d -d pty,raw,echo=0 pty,raw,echo=0` opens two `/dev/pts/N`
/// devices and bridges them: bytes written to one appear on the other. This is
/// the serial-cable stand-in for the KISS / AX.25 interop tests: the Python
/// daemon opens one end, our lnsd opens the other. Dropping the pair kills the
/// `socat` process, tearing both ends down.
pub struct SocatPtyPair {
    process: Child,
    /// First pty device path (e.g. `/dev/pts/3`).
    pub end_a: String,
    /// Second pty device path (e.g. `/dev/pts/4`).
    pub end_b: String,
}

impl SocatPtyPair {
    /// Spawn `socat` and return the two linked pty paths once both are open.
    ///
    /// `socat -d -d` logs each `N PTY is /dev/pts/M` line to stderr; we parse
    /// the two device paths from there. Returns an error if `socat` is missing
    /// or does not report two ptys within the timeout.
    pub async fn spawn() -> Result<Self, HarnessError> {
        let mut cmd = Command::new("socat");
        cmd.args(["-d", "-d", "pty,raw,echo=0", "pty,raw,echo=0"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut process = spawn_supervised(cmd).map_err(HarnessError::SpawnFailed)?;

        let stderr = process.stderr.take().expect("stderr should be captured");

        let parse = tokio::task::spawn_blocking(move || {
            let reader = BufReader::new(stderr);
            let mut ends: Vec<String> = Vec::with_capacity(2);
            for line in reader.lines() {
                let line = line.map_err(|e| HarnessError::ParseError(e.to_string()))?;
                if let Some(idx) = line.find("/dev/pts/") {
                    let path: String = line[idx..]
                        .chars()
                        .take_while(|c| !c.is_whitespace())
                        .collect();
                    ends.push(path);
                    if ends.len() == 2 {
                        return Ok((ends[0].clone(), ends[1].clone()));
                    }
                }
            }
            Err(HarnessError::StartupTimeout)
        });

        let (end_a, end_b) = timeout(Duration::from_secs(10), parse)
            .await
            .map_err(|_| HarnessError::StartupTimeout)?
            .map_err(|_| HarnessError::StartupTimeout)??;

        // socat needs a beat after logging the paths before the ptys are fully
        // wired; a short settle avoids an open() racing the link setup.
        tokio::time::sleep(Duration::from_millis(200)).await;

        Ok(Self {
            process,
            end_a,
            end_b,
        })
    }
}

impl Drop for SocatPtyPair {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Entry in the path table.
#[derive(Debug, Clone)]
pub struct PathEntry {
    pub timestamp: Option<f64>,
    pub hops: Option<u8>,
}

/// A resource received by a Python test daemon.
#[derive(Debug, Clone)]
pub struct ReceivedResource {
    pub data: Vec<u8>,
    pub metadata: Option<Vec<u8>>,
    pub status: String,
}

/// Information about a Reticulum interface.
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    pub name: String,
    pub online: Option<bool>,
    pub in_enabled: Option<bool>,
    pub out_enabled: Option<bool>,
    /// Codeberg #87: ingress-limited announces currently held on this interface
    /// (Python len(Interface.held_announces)); 0 when the daemon predates the
    /// field or the interface has none held.
    pub held_announces: usize,
    /// Codeberg #87: whether the announce ingress burst limiter is active
    /// (Python ic_burst_active).
    pub burst_active: bool,
    /// Codeberg #93: the interface's effective bitrate in bits per second
    /// (Python `interface.bitrate`), or `None` when the daemon predates the
    /// field. A configured `bitrate` overrides the medium default here.
    pub bitrate: Option<u64>,
}

/// Information about a registered destination.
#[derive(Debug, Clone)]
pub struct DestinationInfo {
    pub hash: String,
    /// Full 64-byte public key (X25519 + Ed25519)
    pub public_key: String,
    /// Ed25519 signing key (last 32 bytes of public_key)
    pub signing_key: String,
}

/// Information about an established link.
#[derive(Debug, Clone)]
pub struct LinkInfo;

/// A packet received over a link.
#[derive(Debug, Clone)]
pub struct ReceivedPacket {
    pub data: Vec<u8>,
}

/// A single (non-link) packet received at a destination.
#[derive(Debug, Clone)]
pub struct ReceivedSinglePacket {
    pub dest_hash: Option<String>,
    pub data: Vec<u8>,
}

/// Information about ratchet state for a destination.
#[derive(Debug, Clone)]
pub struct RatchetInfo {
    pub enabled: bool,
    pub count: Option<usize>,
    pub latest_id: Option<String>,
}

/// Information about transport status.
#[derive(Debug, Clone)]
pub struct TransportStatus {
    pub enabled: bool,
    pub identity_hash: Option<String>,
    pub path_table_size: usize,
    pub interface_count: usize,
}

/// Information about a link table entry.
#[derive(Debug, Clone)]
pub struct LinkTableEntry;

/// Information about a client interface.
#[derive(Debug, Clone)]
pub struct ClientInterfaceInfo;

/// Result from rotating a ratchet.
#[derive(Debug, Clone)]
pub struct RatchetRotationResult {
    pub rotated: bool,
    pub ratchet_count: usize,
    pub new_ratchet_id: Option<String>,
}

/// Detailed status of a link.
#[derive(Debug, Clone)]
pub struct LinkStatusInfo {
    pub status: String,
    pub link_hash: String,
    pub state: Option<String>,
    pub mtu: Option<u32>,
    pub mdu: Option<u32>,
}

/// Result from waiting for a link state.
#[derive(Debug, Clone)]
pub struct WaitForLinkStateResult {
    pub status: String,
    pub state: Option<String>,
}

/// Detailed announce table entry from the Python daemon.
#[derive(Debug, Clone)]
pub struct AnnounceTableDetail {
    pub local_rebroadcasts: Option<u64>,
    pub block_rebroadcasts: Option<bool>,
}

/// Find two distinct available TCP ports.
///
/// This function binds to two ports simultaneously before releasing them,
/// ensuring we get two distinct ports that were both available at the same time.
///
/// Prefers high ports (49152-65535) to avoid conflicts with any running rnsd
/// (which typically uses port 4242) or other well-known services.
fn find_two_available_ports() -> Result<(u16, u16), HarnessError> {
    let port1 = pick_free_tcp_port()?;
    let port2 = pick_free_tcp_port()?;
    Ok((port1, port2))
}

/// Pick the next port from the host-wide counter that the OS confirms is
/// currently bindable on 127.0.0.1, retrying past any external occupant.
/// Returns `Err(SpawnFailed)` only if the entire band is exhausted (extremely
/// unlikely in practice).
pub fn pick_free_tcp_port() -> Result<u16, HarnessError> {
    port_alloc::try_free_tcp_port().ok_or_else(|| {
        HarnessError::SpawnFailed(std::io::Error::other("exhausted test port range"))
    })
}

fn pick_free_udp_port() -> Result<u16, HarnessError> {
    port_alloc::try_free_udp_port().ok_or_else(|| {
        HarnessError::SpawnFailed(std::io::Error::other("exhausted test port range"))
    })
}

/// Find four distinct available ports (TCP rns, TCP cmd, UDP listen, UDP forward).
///
/// UDP ports are allocated via UDP bind to avoid collisions with TCP-only allocation.
fn find_four_available_ports() -> Result<(u16, u16, u16, u16), HarnessError> {
    let port1 = pick_free_tcp_port()?;
    let port2 = pick_free_tcp_port()?;
    let port3 = pick_free_udp_port()?;
    let port4 = pick_free_udp_port()?;
    Ok((port1, port2, port3, port4))
}

/// Returned by [`find_available_ports`] alongside the port array. Used to be
/// a mutex guard; now an empty marker because the counter-based allocator
/// no longer needs serialisation. Kept in the signature so callers continue
/// to bind the value (and document the alloc → bind handoff intent), with
/// the option to put real state back into it later if the strategy changes.
#[must_use = "PortAllocation marks the alloc → bind handoff window"]
pub struct PortAllocation;

/// Find N distinct available ports (first 2 via TCP, rest via UDP), drawn
/// from the host-wide counter in [`port_alloc`]. No two callers ever receive
/// the same port number — including callers in another test process, which is
/// what makes `cargo test --workspace` safe. Returns the ports together with
/// an empty [`PortAllocation`] marker.
pub async fn find_available_ports<const N: usize>(
) -> Result<([u16; N], PortAllocation), HarnessError> {
    assert!(N >= 2, "need at least 2 ports");
    let mut ports = [0u16; N];

    for port in ports.iter_mut().take(2) {
        *port = pick_free_tcp_port()?;
    }
    for port in ports.iter_mut().skip(2) {
        *port = pick_free_udp_port()?;
    }

    Ok((ports, PortAllocation))
}

// =========================================================================
// DaemonTopology - Manages multiple connected Python daemons
// =========================================================================

/// Manages multiple connected Python daemons for multi-hop testing.
///
/// This struct creates a topology of daemons where each daemon connects
/// to the previous one via TCPClientInterface, forming a linear chain.
///
/// # Example Topology
///
/// For a 3-daemon topology: D0 <- D1 <- D2
/// - D0 is the entry point (only TCPServerInterface)
/// - D1 connects to D0 (TCPClientInterface -> D0's TCPServerInterface)
/// - D2 connects to D1 (TCPClientInterface -> D1's TCPServerInterface)
///
/// # Usage
///
/// ```ignore
/// let topology = DaemonTopology::linear(3).await?;
/// let entry = topology.entry_daemon();  // D0
/// let exit = topology.exit_daemon();    // D2 (or D1 for 2-daemon)
/// ```
pub struct DaemonTopology {
    daemons: Vec<TestDaemon>,
}

impl DaemonTopology {
    /// Create a linear topology with the specified number of daemons.
    ///
    /// The topology forms a chain: D0 <- D1 <- D2 <- ... <- D(n-1)
    /// where each daemon (except D0) connects to the previous daemon's TCPServerInterface.
    ///
    /// # Arguments
    /// * `count` - Number of daemons to create (must be >= 2)
    ///
    /// # Returns
    /// A DaemonTopology with all daemons connected in a linear chain.
    pub async fn linear(count: usize) -> Result<Self, HarnessError> {
        if count < 2 {
            return Err(HarnessError::ParseError(
                "DaemonTopology requires at least 2 daemons".to_string(),
            ));
        }

        let mut daemons = Vec::with_capacity(count);

        // Start the first daemon (entry point)
        let entry_daemon = TestDaemon::start().await?;
        daemons.push(entry_daemon);

        // Start subsequent daemons and connect them to the previous one
        for i in 1..count {
            let daemon = TestDaemon::start().await?;

            // Get the RNS port of the previous daemon
            let prev_rns_port = daemons[i - 1].rns_port();

            // Connect this daemon to the previous daemon's TCPServerInterface
            let interface_name = format!("LinkTo_D{}", i - 1);
            daemon
                .add_client_interface("127.0.0.1", prev_rns_port, Some(&interface_name))
                .await?;

            daemons.push(daemon);
        }

        // Wait for connections to stabilize
        tokio::time::sleep(Duration::from_millis(500)).await;

        Ok(Self { daemons })
    }

    /// Get the entry daemon (first in the chain).
    ///
    /// This is where Rust A would connect to inject packets into the network.
    pub fn entry_daemon(&self) -> &TestDaemon {
        &self.daemons[0]
    }

    /// Get the exit daemon (last in the chain).
    ///
    /// This is where Rust B would connect to receive packets from the network.
    pub fn exit_daemon(&self) -> &TestDaemon {
        &self.daemons[self.daemons.len() - 1]
    }

    /// Get a daemon by index.
    pub fn daemon(&self, index: usize) -> Option<&TestDaemon> {
        self.daemons.get(index)
    }

    /// Get the number of daemons in the topology.
    pub fn len(&self) -> usize {
        self.daemons.len()
    }

    /// True if no daemons have been added yet.  Pairs with [`Self::len`]
    /// to satisfy `clippy::len_without_is_empty`.  Currently unused by
    /// callers — the `#[allow]` keeps the lint quiet for the rnsd_interop
    /// test binary (where harness is included without `#[allow(dead_code)]`).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.daemons.is_empty()
    }
}

/// Captured outcome of a vendored `rnstatus` subprocess run.
pub struct RnstatusRemoteResult {
    /// Process exit status (`None` exit code means killed by the wall-clock
    /// `timeout` guard or a signal).
    pub status: std::process::ExitStatus,
    /// Everything the tool printed on stdout (JSON status bundle on success
    /// with `-j`).
    pub stdout: String,
    /// Everything the tool printed on stderr (RNS log lines, tracebacks).
    pub stderr: String,
}

impl RnstatusRemoteResult {
    /// Convenience: the exit code, or `None` if the process was signalled /
    /// killed by the wall-clock guard.
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }
}

/// Drive the vendored Python `rnstatus -R <hash> -i <identity> [-l] -w <t>`
/// against a Rust server (our `lnsd`) reachable at `server_addr`.
///
/// This is the Python-client mirror of [`fetch_remote_status`]: instead of our
/// own Rust client, the real Python status CLI queries our daemon's
/// `rnstransport.remote.management` destination. It proves wire + semantic
/// compatibility of our `/status` responder against the reference tool.
///
/// Wiring:
/// * A temporary RNS config dir is written whose ONLY interface is a
///   `TCPClientInterface` targeting `server_addr` — the same shared TCP
///   segment our lnsd listens on. rnstatus builds a standalone Reticulum from
///   that dir (`require_shared_instance=False` under `-R`), forms a link to our
///   server, and requests `/status`.
/// * The management identity (`management_identity_prv`, the 64 raw private-key
///   bytes from [`Identity::private_key_bytes`]) is written to a file rnstatus
///   loads with `-i`. Its hash must be on the server's
///   `remote_management_allowed` list for the request to be authorised. The
///   Python and Rust identity file formats are identical (X25519 priv ‖ Ed25519
///   priv, 64 bytes), so the same bytes yield the same identity hash on both
///   sides.
///
/// `-j` is always passed so a successful run prints the status bundle as JSON
/// on stdout, which the caller parses to assert a populated status came back.
///
/// The whole invocation is wrapped in the `timeout(1)` coreutil (`-w` plus a
/// guard) so a dropped/unanswered request (e.g. a non-allowed identity) can
/// never hang the test binary; it terminates with a non-zero status instead.
///
/// This is a blocking call (spawns a subprocess and waits); run it from a
/// `spawn_blocking` context inside async tests.
pub fn run_python_rnstatus_remote(
    server_addr: SocketAddr,
    server_transport_hash: &[u8],
    management_identity_prv: &[u8],
    include_lstats: bool,
    timeout_secs: u64,
) -> Result<RnstatusRemoteResult, HarnessError> {
    ensure_reticulum_submodule()?;

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let vendor_dir = manifest_dir.join("../reference/Reticulum");
    let rnstatus_script = vendor_dir.join("RNS/Utilities/rnstatus.py");

    // Isolated config + identity dir for this run; kept alive until the
    // subprocess returns (it runs synchronously below), then dropped.
    let workdir = tempfile::Builder::new()
        .prefix("rnstatus_remote_")
        .tempdir()
        .map_err(HarnessError::SpawnFailed)?;

    // Config: a single TCPClientInterface onto our lnsd's shared TCP segment.
    // enable_transport = no — rnstatus is a leaf client, not a router.
    let config = format!(
        "[reticulum]\n  \
           enable_transport = no\n  \
           share_instance = no\n  \
           panic_on_interface_error = no\n\n\
         [interfaces]\n  \
           [[Rust Server Link]]\n    \
             type = TCPClientInterface\n    \
             enabled = yes\n    \
             target_host = {ip}\n    \
             target_port = {port}\n",
        ip = server_addr.ip(),
        port = server_addr.port(),
    );
    let config_path = workdir.path().join("config");
    std::fs::write(&config_path, config).map_err(HarnessError::SpawnFailed)?;

    // Management identity file: the raw 64-byte private key, byte-identical to
    // what Python's `Identity.to_file` writes and `Identity.from_file` reads.
    let identity_path = workdir.path().join("management_identity");
    std::fs::write(&identity_path, management_identity_prv).map_err(HarnessError::SpawnFailed)?;

    let hash_hex = hex_lower_bytes(server_transport_hash);

    // Wall-clock guard above the query timeout so a hung request can't wedge
    // the test. RNS's own request timeout fires the failure callback well
    // inside this window on the reject path.
    let guard_secs = timeout_secs + 15;

    let mut cmd = Command::new("timeout");
    cmd.arg(guard_secs.to_string())
        .arg("python3")
        .arg(&rnstatus_script)
        .arg("--config")
        .arg(workdir.path())
        .arg("-j")
        .arg("-w")
        .arg(timeout_secs.to_string())
        .arg("-i")
        .arg(&identity_path)
        .arg("-R")
        .arg(&hash_hex);
    if include_lstats {
        cmd.arg("-l");
    }
    // rnstatus does a bare `import RNS`; point Python at the vendored tree.
    cmd.env("PYTHONPATH", &vendor_dir);

    let output = cmd.output().map_err(HarnessError::SpawnFailed)?;

    Ok(RnstatusRemoteResult {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Lowercase hex of a byte slice (no external deps).
fn hex_lower_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_daemon_starts_and_responds() {
        let daemon = TestDaemon::start().await.expect("Failed to start daemon");

        // Verify ping works
        daemon.ping().await.expect("Ping failed");

        // Verify we can get interfaces
        let interfaces = daemon
            .get_interfaces()
            .await
            .expect("Failed to get interfaces");
        assert!(!interfaces.is_empty(), "Should have at least one interface");

        // Verify the TCP server interface is present and online
        let tcp_interface = interfaces
            .iter()
            .find(|i| i.name.contains("Test TCP Server"))
            .expect("TCP server interface not found");
        assert_eq!(tcp_interface.online, Some(true));
        assert_eq!(tcp_interface.in_enabled, Some(true));
        assert_eq!(tcp_interface.out_enabled, Some(true));
    }

    #[tokio::test]
    async fn test_register_destination() {
        let daemon = TestDaemon::start().await.expect("Failed to start daemon");

        let dest = daemon
            .register_destination("test", &["echo"])
            .await
            .expect("Failed to register destination");

        assert_eq!(dest.hash.len(), 32, "Hash should be 16 bytes hex-encoded");
        assert_eq!(
            dest.signing_key.len(),
            64,
            "Signing key should be 32 bytes hex-encoded"
        );
    }

    #[tokio::test]
    async fn test_daemon_restart() {
        let mut daemon = TestDaemon::start().await.expect("Failed to start daemon");

        // Register a destination (proves daemon has state)
        let dest = daemon
            .register_destination("test", &["restart"])
            .await
            .expect("Failed to register destination");
        assert!(!dest.hash.is_empty());

        // Kill and restart
        daemon.restart().await.expect("Failed to restart daemon");

        // Verify new daemon responds
        daemon.ping().await.expect("Ping after restart failed");

        // Verify state is gone (fresh daemon has no destinations)
        let interfaces = daemon
            .get_interfaces()
            .await
            .expect("Failed to get interfaces");
        assert!(
            !interfaces.is_empty(),
            "Should have interfaces after restart"
        );
    }
}
