//! Hardened rnstatus/lnstatus drop-in parity suite (Codeberg #86 client render,
//! #67 daemon stats, #87 ingress hold, #88 blackhole).
//!
//! ## The 2x2 matrix
//!
//! Two REAL daemons are spawned as subprocesses from the SAME Python-style
//! config template (only instance name and TCP port differ), each with a
//! pre-seeded transport identity so the shared-instance RPC authkey is known
//! up front:
//!
//! ```text
//!             |  lnsd (our binary)      |  rnsd (vendor Python)
//!   ----------+-------------------------+------------------------
//!   lnstatus  |  full Rust stack (sane) |  drop-in client vs rnsd
//!   rnstatus  |  drop-in client vs lnsd |  reference baseline
//! ```
//!
//! Both daemons receive BYTE-IDENTICAL controlled traffic (the frames are
//! built once and replayed into both TCP servers with identical pacing), so
//! comparing their stats afterwards is a valid A/B:
//!
//!   * lnstatus vs rnstatus against the SAME daemon = CLIENT render parity
//!     (#86): the two clients read the same frozen daemon state, so their
//!     -j JSON must be structurally identical and their text output byte
//!     identical after normalizing only the documented volatile parts.
//!   * rnstatus against lnsd vs rnstatus against rnsd = DAEMON stats parity
//!     (#67): the same reference client reads both daemons after identical
//!     traffic; the per-interface stats of the traffic-bearing interface
//!     must agree field by field.
//!
//! ## Controlled traffic script (identical for both daemons)
//!
//!   1. STEADY: 6 announces of distinct identities, paced 1.5 s apart (well
//!      below the 3 Hz new-interface ingress threshold) -> 6 paths, real
//!      incoming_announce_frequency.
//!   2. PATH REQUESTS: 4 path requests (3 known + 1 unknown destination),
//!      paced 1.5 s -> real incoming_pr_frequency, outgoing path responses.
//!   3. BURST: after a >10 s quiet gap (so earlier samples are past the
//!      AR_FREQ_DECAY window, see PRE_BURST_DECAY_GAP), 20 fresh-identity
//!      announces at 25 Hz -> ingress burst activates on both stacks
//!      (Interface.py IC_BURST_FREQ_NEW = 3 Hz, IC_BURST_MIN_SAMPLES = 6),
//!      excess announces are HELD. Sampled DURING the burst (inside the 15 s
//!      IC_BURST_PENALTY window, before the first release) and again AFTER
//!      the drain (held back to 0). Covers #87.
//!   4. BLACKHOLE: the victim identity is blackholed via the REAL vendor
//!      rnpath.py -B against each daemon's own RPC; its re-announce is
//!      dropped on both stacks while a sentinel announce passes; -U lifts
//!      the blackhole and the victim announces normally again. Covers #88.
//!   5. FREEZE: traffic quiesced, then both daemons are polled until every
//!      volatile stat has settled (see below) before the comparison matrix.
//!
//! ## Volume guard (A/B discipline)
//!
//! Before ANY cross-daemon comparison the suite counts event volumes on both
//! daemons and fails loudly on mismatch:
//!
//!   * learned path SET is compared EXACTLY (same destination hashes on both
//!     daemons, equal to the script's expectation), not "within a few
//!     percent" - the traffic is byte-identical, so any missing path is a
//!     real processing divergence;
//!   * per-interface rxb/txb are compared within 5% across daemons. On RX
//!     the two stacks count at different framing layers - we count the
//!     on-wire HDLC bytes, Python counts the de-framed payload
//!     (`process_incoming` is handed the de-framed buffer,
//!     TCPInterface.py:306) - a stable ~2% delta on this script. On TX
//!     they count the SAME layer: Python reassigns `data` to the framed
//!     buffer before `self.txb += len(data)` (TCPInterface.py:327), as we
//!     do in `interfaces/tcp.rs`. A txb delta is therefore a real
//!     difference in what was transmitted, not a framing allowance;
//!   * and because it is real, txb is not compared as a percentage alone.
//!     The injector connection is each daemon's only non-local interface,
//!     so everything it reads back IS that interface's tx (the capture
//!     total is asserted against the reported txb). Every frame is decoded
//!     and the two multisets compared EXACTLY - see the TX frame census
//!     below. That is how Codeberg #192 was resolved: the 4.3-5.8% txb
//!     delta that tripped the byte guard about one run in four was three
//!     duplicate PATH_RESPONSE announces, from a path-response entry that
//!     started its retry count at a received announce's value.
//!
//! ## Freeze discipline (why the frozen comparisons cannot flake)
//!
//! Python's announce/PR frequency read (Interface.incoming_announce_frequency)
//! returns n/span over a timestamp deque and POPS one decayed sample per call
//! once the span exceeds 10 s; when the deque is down to 2 samples it returns
//! exactly 0. Our implementation mirrors that. The freeze loop polls
//! interface_stats until, on BOTH daemons, every frequency reads exactly 0,
//! rxs/txs read exactly 0, held_announces is 0 and no burst flag is set. The
//! polling itself drains the deques, so the loop always converges, and after
//! it the ONLY values that can differ between two reads of the same daemon are
//! transport_uptime and (on rnsd) the process rss. Every other field is
//! bit-stable, which is what makes EXACT comparison possible instead of
//! "almost".
//!
//! One of those values needs more than a predicate. rxs/txs are not computed
//! on read: a 1 Hz sampler turns byte-counter deltas into speeds on both
//! stacks, so a speed of 0 means EITHER "idle" OR "received but not sampled
//! yet", and the second reading flips positive for a whole second at the next
//! tick. The freeze therefore requires the quiet state to hold across more
//! than one sampling period with the byte counters unchanged
//! (`FREEZE_SETTLE`), which is the only thing that tells the two zeros apart.
//! Codeberg #195 was that tick landing between two client reads.
//!
//! ## Known, pinned cross-stack divergences (asserted, not ignored)
//!
//! The daemon parity comparison does not silently normalize structural gaps;
//! it asserts them EXACTLY so any drift (vendor bump, our refactor) fails:
//!
//!   * interface list shape: IDENTICAL since Codeberg #177 - both daemons
//!     list Shared Instance + TCP listener + spawned connection + one
//!     LocalClientInterface while the sampling rnstatus is connected. The
//!     whole-inventory comparison lives in
//!     `status_inventory_parity_across_daemons`;
//!   * per-interface key set: ours adds announce_queue/peers/tx_queue_drops
//!     plus the post-1.3.5 upstream key set (txdrp and its siblings, added
//!     for current-rnstatus drop-in compatibility — see the note above
//!     `row_fields` in rpc/handlers.rs) that the 1.3.5 vendor rnsd here
//!     predates; nothing is Python-only;
//!   * top-level key set: ours additionally carries the post-1.3.5 upstream
//!     totals/queue keys, pinned exactly below;
//!   * top-level rss: Python reports the daemon process RSS, ours reports
//!     null;
//!   * interface identity fields carry per-daemon values (instance name,
//!     listen port) and are therefore compared per daemon here; their
//!     cross-daemon comparison, templated, is the inventory test's job.
//!
//! Marked #[ignore]: spawns Python daemons and tooling. Run with:
//!
//! ```sh
//! cargo build -p leviculum-cli --bin lnsd --bin lnstatus
//! cargo test --package leviculum-std --test rnsd_interop \
//!     status_parity -- --include-ignored --test-threads=1 --nocapture
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rand_core::OsRng;
use regex::Regex;
use serde_json::Value;
use tokio::net::TcpStream;

use leviculum_core::constants::{MTU, TRUNCATED_HASHBYTES};
use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};
use leviculum_std::process::spawn_supervised;

use crate::common::{build_path_request_raw_with_tag, init_tracing, now_ms};
use crate::harness::find_available_ports;
use crate::rpc_interop_tests::{run_python_tool, RNPATH_PY};

const RNSD_PY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../reference/Reticulum/RNS/Utilities/rnsd.py"
);
const RNSTATUS_PY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../reference/Reticulum/RNS/Utilities/rnstatus.py"
);

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

// =========================================================================
// Binary discovery (real lnsd / lnstatus binaries, freshness-checked)
// =========================================================================

/// Directory that holds the workspace binaries for the active target: the
/// test executable lives in `<target>/<triple>/debug/deps/`, the CLI
/// binaries one level up.
fn bin_dir() -> PathBuf {
    let mut dir = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("deps dir")
        .to_path_buf();
    dir.pop();
    dir
}

/// Newest mtime of any .rs file under `dir` (recursive). Used to reject
/// stale CLI binaries: a binary older than the newest source would silently
/// test yesterday's code (the #53 stale-binary lesson).
fn newest_source_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let m = if path.is_dir() {
            newest_source_mtime(&path)
        } else if path.extension().is_some_and(|e| e == "rs") {
            entry.metadata().ok().and_then(|m| m.modified().ok())
        } else {
            None
        };
        if let Some(m) = m {
            if newest.map(|n| m > n).unwrap_or(true) {
                newest = Some(m);
            }
        }
    }
    newest
}

/// Resolve a prebuilt CLI binary, panicking with a build instruction when it
/// is missing or older than the CLI crate's sources.
fn cli_bin(name: &str) -> PathBuf {
    let bin = bin_dir().join(name);
    let build_hint = format!(
        "run `cargo build -p leviculum-cli --bin lnsd --bin lnstatus` first \
         (expected at {})",
        bin.display()
    );
    let meta = match std::fs::metadata(&bin) {
        Ok(m) => m,
        Err(_) => panic!("{name} binary not found; {build_hint}"),
    };
    let cli_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../leviculum-cli/src");
    if let (Ok(bin_mtime), Some(src_mtime)) = (meta.modified(), newest_source_mtime(&cli_src)) {
        assert!(
            bin_mtime >= src_mtime,
            "{name} binary is STALE (older than leviculum-cli sources); {build_hint}"
        );
    }
    bin
}

// =========================================================================
// Daemon under test
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stack {
    Lnsd,
    Rnsd,
}

impl Stack {
    fn label(self) -> &'static str {
        match self {
            Stack::Lnsd => "lnsd",
            Stack::Rnsd => "rnsd",
        }
    }
}

/// A real daemon subprocess (our lnsd binary or the vendor Python rnsd),
/// started from a shared config template with a pre-seeded transport
/// identity. Both stacks parse the same Python-style INI config, listen on
/// one TCPServerInterface and expose the shared-instance RPC socket
/// `\0rns/<instance>/rpc` with authkey SHA256(transport_identity bytes).
struct ParityDaemon {
    stack: Stack,
    config_dir: PathBuf,
    instance_name: String,
    authkey: [u8; 32],
    /// Listen ports of the configured TCP server interfaces. `tcp_port` is the
    /// first; the multi-interface parity test uses all of them.
    tcp_ports: Vec<u16>,
    tcp_port: u16,
    child: Child,
}

impl ParityDaemon {
    async fn start(stack: Stack) -> ParityDaemon {
        Self::start_n(stack, 1).await
    }

    /// Start a daemon with `n_interfaces` TCP server interfaces (1 or 2). The
    /// single-interface form is what the 2x2 matrix uses; the two-interface
    /// form backs the multi-interface sort/`-a` parity test, where more than
    /// one visible interface is required for ordering to be observable.
    async fn start_n(stack: Stack, n_interfaces: usize) -> ParityDaemon {
        assert!(
            (1..=2).contains(&n_interfaces),
            "parity daemon supports 1 or 2 interfaces"
        );
        let test_id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let instance_name = format!(
            "parity_{}_{}_{}",
            stack.label(),
            std::process::id(),
            test_id
        );
        // The harness allocator hands out a minimum of two ports; the
        // single-interface form uses only the first.
        let (ports, _alloc) = find_available_ports::<2>().await.expect("allocate port");
        let tcp_ports: Vec<u16> = ports[..n_interfaces].to_vec();
        let tcp_port = tcp_ports[0];

        let config_dir = std::env::temp_dir().join(format!("status_parity_{instance_name}"));
        let _ = std::fs::remove_dir_all(&config_dir);
        std::fs::create_dir_all(config_dir.join("storage")).expect("create config dir");

        // Pre-seed the transport identity so the RPC authkey
        // (SHA256 of the 64 private key bytes) is known before startup.
        let identity = Identity::generate(&mut OsRng);
        let identity_bytes = identity.private_key_bytes().expect("private key bytes");
        std::fs::write(
            config_dir.join("storage").join("transport_identity"),
            identity_bytes,
        )
        .expect("write transport_identity");
        use sha2::Digest;
        let mut authkey = [0u8; 32];
        authkey.copy_from_slice(&sha2::Sha256::digest(identity_bytes));

        // One config template for BOTH stacks: transport node, shared
        // instance, one or two TCP servers. No `mode` override so both stacks
        // report the default MODE_FULL.
        let mut iface_blocks = String::new();
        for (idx, port) in tcp_ports.iter().enumerate() {
            iface_blocks.push_str(&format!(
                "\x20 [[Parity TCP Server {idx}]]\n\
                 \x20   type = TCPServerInterface\n\
                 \x20   enabled = yes\n\
                 \x20   listen_ip = 127.0.0.1\n\
                 \x20   listen_port = {port}\n"
            ));
        }
        let config = format!(
            "[reticulum]\n\
             \x20 enable_transport = yes\n\
             \x20 share_instance = yes\n\
             \x20 instance_name = {instance_name}\n\
             \x20 panic_on_interface_error = no\n\
             \n\
             [logging]\n\
             \x20 loglevel = 3\n\
             \n\
             [interfaces]\n\
             {iface_blocks}"
        );
        std::fs::write(config_dir.join("config"), config).expect("write config");

        let log = std::fs::File::create(config_dir.join("daemon.log")).expect("create log");
        let log_err = log.try_clone().expect("clone log handle");
        let mut cmd = match stack {
            Stack::Lnsd => {
                let mut cmd = Command::new(cli_bin("lnsd"));
                cmd.arg("--config").arg(&config_dir);
                cmd
            }
            Stack::Rnsd => {
                let mut cmd = Command::new("python3");
                cmd.arg(RNSD_PY).arg("--config").arg(&config_dir).env(
                    "PYTHONPATH",
                    concat!(env!("CARGO_MANIFEST_DIR"), "/../reference/Reticulum"),
                );
                cmd
            }
        };
        cmd.stdout(Stdio::from(log)).stderr(Stdio::from(log_err));
        let child = spawn_supervised(cmd).expect("spawn the parity daemon");

        let daemon = ParityDaemon {
            stack,
            config_dir,
            instance_name,
            authkey,
            tcp_ports,
            tcp_port,
            child,
        };

        // Readiness: the RPC socket answering interface_stats is the real
        // "daemon is up" condition for every consumer this suite uses (both
        // clients and the introspection polls all go through it).
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if daemon.try_stats().await.is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{} did not become RPC-ready within 30 s (log: {})",
                daemon.stack.label(),
                daemon.config_dir.join("daemon.log").display()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        daemon
    }

    async fn try_stats(&self) -> Option<Value> {
        leviculum_std::rpc_query(&self.instance_name, &self.authkey, "interface_stats")
            .await
            .ok()
    }

    /// interface_stats via the same shared-instance RPC the clients use.
    async fn stats(&self) -> Value {
        self.try_stats()
            .await
            .unwrap_or_else(|| panic!("interface_stats RPC failed on {}", self.stack.label()))
    }

    /// Set of learned destination hashes (hex) from the path_table RPC.
    async fn path_hashes(&self) -> BTreeSet<String> {
        let table = leviculum_std::rpc_query(&self.instance_name, &self.authkey, "path_table")
            .await
            .unwrap_or_else(|e| panic!("path_table RPC failed on {}: {e}", self.stack.label()));
        table
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("hash").and_then(|h| h.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn blackholed(&self) -> Value {
        leviculum_std::rpc_query(&self.instance_name, &self.authkey, "blackholed_identities")
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "blackholed_identities RPC failed on {}: {e}",
                    self.stack.label()
                )
            })
    }
}

impl Drop for ParityDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// =========================================================================
// Status clients (the actual matrix axes)
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusClient {
    Rnstatus,
    Lnstatus,
}

impl StatusClient {
    fn label(self) -> &'static str {
        match self {
            StatusClient::Rnstatus => "rnstatus",
            StatusClient::Lnstatus => "lnstatus",
        }
    }
}

/// Run one status client against one daemon (drop-in style: the client gets
/// the DAEMON's own config directory, exactly like an operator would) and
/// return stdout. Panics with full stderr on a non-zero exit.
async fn run_status(client: StatusClient, daemon: &ParityDaemon, args: &[&str]) -> String {
    let output = match client {
        StatusClient::Rnstatus => run_python_tool(RNSTATUS_PY, args, &daemon.config_dir).await,
        StatusClient::Lnstatus => tokio::process::Command::new(cli_bin("lnstatus"))
            .arg("--config")
            .arg(&daemon.config_dir)
            .args(args)
            .output()
            .await
            .expect("spawn lnstatus"),
    };
    assert!(
        output.status.success(),
        "{} {:?} against {} exited with {:?}\nstdout:\n{}\nstderr:\n{}",
        client.label(),
        args,
        daemon.stack.label(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// rnstatus connects to the daemon as a full shared-instance client, so its
/// own LocalClientInterface appears in the stats while it runs (observer
/// footprint). Wait until that footprint is gone before the next sample, so
/// sequential samples always read the same client-free state.
///
/// Applies to BOTH daemons: rnstatus is a shared-instance client of `lnsd`
/// exactly as it is of `rnsd` (it fails to bind the instance socket and falls
/// back to LocalClientInterface, Reticulum.py:418-425), so once lnsd reports
/// its accepted IPC clients (Codeberg #177) the footprint is symmetric.
async fn wait_observer_gone(daemon: &ParityDaemon) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let stats = daemon.stats().await;
        let leftovers = ifaces(&stats)
            .iter()
            .filter(|i| i["type"] == "LocalClientInterface")
            .count();
        if leftovers == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} still lists {leftovers} LocalClientInterface entries 15 s after the client exited",
            daemon.stack.label()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// =========================================================================
// Controlled traffic script
// =========================================================================

/// Steady-phase announce count. 6 paced announces both create paths and fill
/// the announce deque past IC_DEQUE_MIN_SAMPLE so the frequency reads real.
const STEADY_N: usize = 6;
/// Burst announce count: comfortably past the ~6 announces the limiter needs
/// to detect the burst (IC_BURST_MIN_SAMPLES plus decayed-sample pops), far
/// below the 256 held cap, and small enough that the post-penalty drain (15 s
/// + 5 s per held announce) finishes inside the drain deadline.
const BURST_N: usize = 20;
/// Pacing that keeps a phase below the 3 Hz new-interface ingress threshold.
const SLOW_PACE: Duration = Duration::from_millis(1500);
/// Burst pacing: ~25 Hz, far above the 3 Hz threshold.
const BURST_PACE: Duration = Duration::from_millis(40);
/// Quiet gap before the burst, sized past the 10 s frequency decay window
/// (Interface.py AR_FREQ_DECAY). Both stacks compute the announce frequency
/// as n/(now - oldest) over a 48-sample deque and pop only ONE decayed
/// sample per read once the span exceeds 10 s. Without the gap the
/// steady-phase samples pin the span at ~9 s (not yet decay-eligible) and
/// [`BURST_N`] frames only reach ~2.3 Hz - NEITHER stack activates during
/// the burst. After the gap every pre-burst sample is decay-eligible, the
/// burst's per-frame frequency reads pop them one per frame, and the
/// threshold is crossed at (nearly) the same frame on both stacks.
const PRE_BURST_DECAY_GAP: Duration = Duration::from_secs(12);

/// Build a signed announce frame for a caller-owned identity. Every call
/// yields a fresh packet hash (random announce blob), so re-announces are
/// not deduplicated; the same returned bytes are replayed into BOTH daemons.
fn build_announce_for(identity: &Identity, aspect: &str) -> (Vec<u8>, DestinationHash) {
    let mut dest = Destination::new(
        Some(identity.clone()),
        Direction::In,
        DestinationType::Single,
        "status_parity",
        &[aspect],
    )
    .expect("create destination");
    let packet = dest
        .announce(Some(b"parity"), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("create announce");
    let mut raw = [0u8; MTU];
    let size = packet.pack(&mut raw).expect("pack announce");
    (raw[..size].to_vec(), *dest.hash())
}

/// The full pre-built traffic script. All frames are generated ONCE so both
/// daemons receive byte-identical traffic (a hard prerequisite for the
/// volume guard and every cross-daemon comparison).
struct TrafficScript {
    steady: Vec<(Vec<u8>, DestinationHash)>,
    path_requests: Vec<Vec<u8>>,
    burst: Vec<(Vec<u8>, DestinationHash)>,
    victim_identity_hex: String,
    victim_hash: DestinationHash,
    victim_reannounce: Vec<u8>,
    victim_after_unblackhole: Vec<u8>,
    sentinel: (Vec<u8>, DestinationHash),
}

fn build_script() -> TrafficScript {
    let steady_ids: Vec<Identity> = (0..STEADY_N)
        .map(|_| Identity::generate(&mut OsRng))
        .collect();
    let steady: Vec<_> = steady_ids
        .iter()
        .enumerate()
        .map(|(i, id)| build_announce_for(id, &format!("s{i}")))
        .collect();

    // Path requests with fixed tags: byte-identical frames on both daemons.
    // Three for known destinations (the daemon answers with a path response)
    // and one for an unknown destination (ignored, still counts as an
    // incoming path request).
    let mut path_requests = Vec::new();
    for (i, target) in [
        steady[1].1,
        steady[2].1,
        DestinationHash::new([0xEEu8; TRUNCATED_HASHBYTES]),
        steady[3].1,
    ]
    .iter()
    .enumerate()
    {
        let tag = [i as u8 + 1; TRUNCATED_HASHBYTES];
        path_requests.push(build_path_request_raw_with_tag(target.as_bytes(), &tag));
    }

    let burst: Vec<_> = (0..BURST_N)
        .map(|i| {
            let id = Identity::generate(&mut OsRng);
            build_announce_for(&id, &format!("b{i}"))
        })
        .collect();

    let victim = &steady_ids[0];
    let victim_hash = steady[0].1;
    let (victim_reannounce, vh2) = build_announce_for(victim, "s0");
    let (victim_after_unblackhole, vh3) = build_announce_for(victim, "s0");
    assert_eq!(victim_hash, vh2);
    assert_eq!(victim_hash, vh3);

    let sentinel_id = Identity::generate(&mut OsRng);
    let sentinel = build_announce_for(&sentinel_id, "sentinel");

    TrafficScript {
        steady,
        path_requests,
        burst,
        victim_identity_hex: hex::encode(victim.hash()),
        victim_hash,
        victim_reannounce,
        victim_after_unblackhole,
        sentinel,
    }
}

/// One frame a daemon transmitted back on the injector connection, stamped
/// with the offset from the moment the injector connected.
#[derive(Clone)]
struct CapturedFrame {
    at: Duration,
    bytes: Vec<u8>,
}

/// What the reader task accumulates for one injector connection.
#[derive(Default)]
struct Capture {
    frames: Vec<CapturedFrame>,
    /// Raw bytes read off the socket, i.e. HDLC-FRAMED bytes - the same
    /// quantity both stacks add to the spawned interface's `txb`
    /// (TCPInterface.py:327, `interfaces/tcp.rs`). Comparing this against
    /// the reported txb is what makes the census a measurement of the
    /// volume guard's own number rather than of a parallel one.
    wire_bytes: u64,
}

/// One injector per daemon: a single raw RNS/TCP connection, kept open for
/// the whole scenario so each daemon sees exactly one stable traffic
/// interface whose ingress state is never reset by reconnects.
///
/// The read half is drained by a background task that deframes and keeps
/// every frame the daemon sent back. That connection is the daemon's ONLY
/// non-local interface, so its capture is the complete record of what the
/// traffic interface transmitted - the content behind the volume guard's
/// txb number (Codeberg #192).
struct Injector {
    tx: tokio::net::tcp::OwnedWriteHalf,
    capture: std::sync::Arc<std::sync::Mutex<Capture>>,
    _reader: tokio::task::JoinHandle<()>,
}

impl Injector {
    fn attach(stream: TcpStream) -> Injector {
        let (mut rx, tx) = stream.into_split();
        let capture = std::sync::Arc::new(std::sync::Mutex::new(Capture::default()));
        let sink = capture.clone();
        let started = Instant::now();
        let reader = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut deframer = leviculum_std::interfaces::hdlc::Deframer::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = match rx.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let at = started.elapsed();
                let mut guard = sink.lock().expect("capture lock");
                guard.wire_bytes += n as u64;
                for result in deframer.process(&buf[..n]) {
                    if let leviculum_std::interfaces::hdlc::DeframeResult::Frame(bytes) = result {
                        guard.frames.push(CapturedFrame { at, bytes });
                    }
                }
            }
        });
        Injector {
            tx,
            capture,
            _reader: reader,
        }
    }

    async fn send(&mut self, raw: &[u8]) {
        use tokio::io::AsyncWriteExt;
        let mut framed = Vec::new();
        leviculum_std::interfaces::hdlc::frame(raw, &mut framed);
        self.tx.write_all(&framed).await.expect("injector write");
        self.tx.flush().await.expect("injector flush");
    }

    fn captured(&self) -> Vec<CapturedFrame> {
        self.capture.lock().expect("capture lock").frames.clone()
    }

    fn wire_bytes(&self) -> u64 {
        self.capture.lock().expect("capture lock").wire_bytes
    }
}

struct Injectors {
    lnsd: Injector,
    rnsd: Injector,
}

impl Injectors {
    async fn connect(lnsd: &ParityDaemon, rnsd: &ParityDaemon) -> Injectors {
        Injectors {
            lnsd: Injector::attach(connect_injector(lnsd).await),
            rnsd: Injector::attach(connect_injector(rnsd).await),
        }
    }

    /// Send the same frame to both daemons back to back. Loopback TCP writes
    /// complete in microseconds, so both daemons observe the same pacing.
    async fn send_both(&mut self, frame: &[u8]) {
        self.lnsd.send(frame).await;
        self.rnsd.send(frame).await;
    }

    /// Send a paced sequence of frames to both daemons: `pace` BEFORE each
    /// frame keeps every phase entry below the ingress threshold as well.
    async fn send_paced(&mut self, frames: &[&[u8]], pace: Duration) {
        for frame in frames {
            tokio::time::sleep(pace).await;
            self.send_both(frame).await;
        }
    }
}

// =========================================================================
// TX frame census (Codeberg #192)
// =========================================================================

/// One transmitted frame reduced to what identifies it on the wire: the
/// fourth wire-field question (docs/src/concepts/wire-field-semantics.md)
/// asks whether we emit a frame at all, so the census keys on the fields a
/// peer decides from, not on the byte count.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TxFrame {
    packet_type: String,
    context: String,
    dest_hash: String,
    hops: u8,
    size: usize,
}

impl std::fmt::Display for TxFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<9} ctx={:<12} dest={} hops={} size={}",
            self.packet_type, self.context, self.dest_hash, self.hops, self.size
        )
    }
}

fn decode_tx(frame: &CapturedFrame) -> TxFrame {
    let packet = leviculum_core::packet::Packet::unpack(&frame.bytes)
        .unwrap_or_else(|e| panic!("captured frame is not a valid RNS packet: {e:?}"));
    TxFrame {
        packet_type: format!("{:?}", packet.flags.packet_type),
        context: format!("{:?}", packet.context),
        dest_hash: hex::encode(packet.destination_hash),
        hops: packet.hops,
        size: frame.bytes.len(),
    }
}

/// A daemon's transmitted frames, in order, with their capture offsets.
fn census(frames: &[CapturedFrame]) -> Vec<(Duration, TxFrame)> {
    frames.iter().map(|f| (f.at, decode_tx(f))).collect()
}

fn render_census(rows: &[(Duration, TxFrame)]) -> String {
    rows.iter()
        .map(|(at, f)| format!("    t={:>7.2}s {f}", at.as_secs_f64()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Multiset difference `left - right` on decoded frames, ignoring the
/// per-frame timing and the destination hash's identity: two daemons fed the
/// same script emit frames for the same destinations, so what a surplus is
/// made of is (type, context, hops, size) plus which destination it names.
fn frame_surplus(left: &[(Duration, TxFrame)], right: &[(Duration, TxFrame)]) -> Vec<TxFrame> {
    let mut pool: Vec<TxFrame> = right.iter().map(|(_, f)| f.clone()).collect();
    let mut surplus = Vec::new();
    for (_, f) in left {
        match pool.iter().position(|c| c == f) {
            Some(i) => {
                pool.remove(i);
            }
            None => surplus.push(f.clone()),
        }
    }
    surplus.sort();
    surplus
}

// =========================================================================
// Stats JSON accessors and freeze predicate
// =========================================================================

fn ifaces(stats: &Value) -> Vec<Value> {
    stats
        .get("interfaces")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn num(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn boolean(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|b| b.as_bool()).unwrap_or(false)
}

fn held_total(stats: &Value) -> u64 {
    ifaces(stats)
        .iter()
        .map(|i| num(i, "held_announces") as u64)
        .sum()
}

fn any_burst(stats: &Value) -> bool {
    ifaces(stats).iter().any(|i| boolean(i, "burst_active"))
}

/// True for a listener row (shared-instance server, TCP server listener): the
/// reference gives exactly those a `clients` count and leaves it None on every
/// interface that carries packets itself (Reticulum.py:1337-1340).
fn is_listener(iface: &Value) -> bool {
    iface.get("clients").is_some_and(|c| c.is_number())
}

/// The traffic-bearing interface: on both daemons the spawned per-connection
/// entry. Selected as the non-local, non-listener entry with the highest rxb,
/// which is unambiguous because exactly one interface ever receives the
/// injected frames. Listeners are excluded explicitly: they aggregate their
/// children's bytes, so the highest-rxb row would otherwise be ambiguous
/// between a listener and its single child.
fn traffic_iface(stats: &Value) -> Value {
    ifaces(stats)
        .into_iter()
        .filter(|i| {
            let t = i.get("type").and_then(|t| t.as_str()).unwrap_or("");
            t != "LocalServerInterface" && t != "LocalClientInterface" && !is_listener(i)
        })
        .max_by_key(|i| num(i, "rxb") as u64)
        .expect("no traffic interface in stats")
}

/// True once every volatile per-interface stat has settled to its exact
/// frozen value (see the module doc for why polling guarantees convergence).
fn is_quiet(stats: &Value) -> bool {
    let ifs = ifaces(stats);
    let iface_quiet = ifs.iter().all(|i| {
        num(i, "rxs") == 0.0
            && num(i, "txs") == 0.0
            && num(i, "incoming_announce_frequency") == 0.0
            && num(i, "outgoing_announce_frequency") == 0.0
            && num(i, "incoming_pr_frequency") == 0.0
            && num(i, "outgoing_pr_frequency") == 0.0
            && num(i, "held_announces") == 0.0
            && !boolean(i, "burst_active")
            && !boolean(i, "pr_burst_active")
    });
    iface_quiet && num(stats, "rxs") == 0.0 && num(stats, "txs") == 0.0
}

/// One tick of the 1 Hz traffic counter, on BOTH stacks: ours samples the byte
/// counters once a second and caches `(delta * 8) / elapsed`
/// (`spawn_traffic_counter`, leviculum-std/src/interfaces/mod.rs:238-256),
/// Python's `Transport.count_traffic_loop` does the same.
const SPEED_SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// How long [`is_quiet`] must hold CONTINUOUSLY, over unchanged byte counters,
/// before a daemon counts as frozen.
///
/// **Why a duration and not just a predicate (Codeberg #195).** `rxs == 0` has
/// two meanings: "this interface is idle" and "bytes have arrived but the 1 Hz
/// sampler has not run since". They are the same number
/// (`leviculum-std/src/interfaces/mod.rs`, unit test
/// `a_zero_speed_reading_does_not_mean_the_bytes_have_been_accounted`), so a
/// predicate reading one snapshot cannot tell them apart — and in the second
/// case the next tick turns the speed positive for one whole sampling period.
/// A comparison of two sequential client reads that straddles that tick then
/// sees `0 bps` in one render and `64 bps` in the other, with nothing wrong on
/// either side.
///
/// What separates the two states is time, and only time: once the byte
/// counters stop moving, at most ONE further tick can report a delta. Watching
/// the quiet state hold across a window that must contain more than one tick
/// therefore proves the sampler has already consumed the final byte count, and
/// that every later read will be the same one. Two-and-a-half periods leaves
/// room for the sampler thread's own drift.
const FREEZE_SETTLE: Duration =
    Duration::from_millis(SPEED_SAMPLE_PERIOD.as_millis() as u64 * 5 / 2);

/// `(interface name, rxb, txb)` for every reported interface, sorted.
type ByteCounters = Vec<(String, u64, u64)>;

/// The byte counters of every reported interface, keyed by name: the input the
/// speed sampler works from, and what has to stand still for the freeze to
/// mean anything.
fn byte_counters(stats: &Value) -> ByteCounters {
    let mut rows: ByteCounters = ifaces(stats)
        .iter()
        .map(|i| {
            (
                i["name"].as_str().unwrap_or_default().to_string(),
                num(i, "rxb") as u64,
                num(i, "txb") as u64,
            )
        })
        .collect();
    rows.sort();
    rows
}

/// Poll `daemon` until it is [`is_quiet`], `extra` holds, and both have held
/// continuously over unchanged byte counters for [`FREEZE_SETTLE`].
///
/// This is the suite's freeze, and the only wait in it that is allowed to
/// couple to a duration — because the thing being waited for IS a sampling
/// period (see [`FREEZE_SETTLE`]). Any positive speed, any byte-counter
/// movement or any `extra` violation restarts the window, so the wait ends on
/// an observed state and not on elapsed time.
async fn wait_frozen<F: Fn(&Value) -> bool>(
    daemon: &ParityDaemon,
    what: &str,
    deadline: Duration,
    extra: F,
) -> Value {
    let start = Instant::now();
    // Frequency deques drain one decayed sample per read, so polling faster
    // than the sampler only makes the freeze converge sooner.
    let interval = Duration::from_millis(200);
    let mut quiet_since: Option<(Instant, ByteCounters)> = None;
    loop {
        let stats = daemon.stats().await;
        let counters = byte_counters(&stats);
        if is_quiet(&stats) && extra(&stats) {
            match &quiet_since {
                Some((since, seen)) if *seen == counters => {
                    if since.elapsed() >= FREEZE_SETTLE {
                        return stats;
                    }
                }
                _ => quiet_since = Some((Instant::now(), counters)),
            }
        } else {
            quiet_since = None;
        }
        assert!(
            start.elapsed() < deadline,
            "{}: {} not reached within {:?} (needs {:?} of unbroken quiet); last stats:\n{}",
            daemon.stack.label(),
            what,
            deadline,
            FREEZE_SETTLE,
            serde_json::to_string_pretty(&stats).unwrap_or_default()
        );
        tokio::time::sleep(interval).await;
    }
}

/// Every per-interface field the idle render is a pure function of, one line
/// per interface, sorted by name.
///
/// Captured before and after the client comparison loop. The whole test rests
/// on the daemon standing still between two sequential client reads; if it
/// ever moves again, this says which field moved, instead of leaving a reader
/// to spot it in two multi-hundred-byte render blobs (Codeberg #195).
fn render_inputs(stats: &Value) -> Vec<String> {
    let mut rows: Vec<String> = ifaces(stats)
        .iter()
        .map(|i| {
            format!(
                "{} type={} status={} clients={:?} bitrate={} mode={} rxb={} txb={} \
                 rxs={} txs={} held={} iaf={} oaf={} ipf={} opf={} burst={} pr_burst={}",
                i["name"].as_str().unwrap_or_default(),
                i["type"].as_str().unwrap_or_default(),
                boolean(i, "status"),
                i.get("clients"),
                num(i, "bitrate"),
                num(i, "mode"),
                num(i, "rxb"),
                num(i, "txb"),
                num(i, "rxs"),
                num(i, "txs"),
                num(i, "held_announces"),
                num(i, "incoming_announce_frequency"),
                num(i, "outgoing_announce_frequency"),
                num(i, "incoming_pr_frequency"),
                num(i, "outgoing_pr_frequency"),
                boolean(i, "burst_active"),
                boolean(i, "pr_burst_active"),
            )
        })
        .collect();
    rows.sort();
    rows
}

/// Poll `daemon` until `pred` holds; panic with the final snapshot when the
/// deadline passes. Eventual-consistency polling: no assertion in this suite
/// couples to a wall-clock instant, only to a state being reached.
async fn wait_stats<F: Fn(&Value) -> bool>(
    daemon: &ParityDaemon,
    what: &str,
    deadline: Duration,
    interval: Duration,
    pred: F,
) -> Value {
    let start = Instant::now();
    loop {
        let stats = daemon.stats().await;
        if pred(&stats) {
            return stats;
        }
        assert!(
            start.elapsed() < deadline,
            "{}: {} not reached within {:?}; last stats:\n{}",
            daemon.stack.label(),
            what,
            deadline,
            serde_json::to_string_pretty(&stats).unwrap_or_default()
        );
        tokio::time::sleep(interval).await;
    }
}

/// Poll until the daemon's learned path set satisfies `pred`.
async fn wait_paths<F: Fn(&BTreeSet<String>) -> bool>(
    daemon: &ParityDaemon,
    what: &str,
    deadline: Duration,
    pred: F,
) -> BTreeSet<String> {
    let start = Instant::now();
    loop {
        let paths = daemon.path_hashes().await;
        if pred(&paths) {
            return paths;
        }
        assert!(
            start.elapsed() < deadline,
            "{}: {} not reached within {:?}; current path set ({}): {:?}",
            daemon.stack.label(),
            what,
            deadline,
            paths.len(),
            paths
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// =========================================================================
// JSON comparison (the -j exact anchor)
// =========================================================================

/// Canonical value for structural comparison: all numbers collapse to f64 so
/// Python's `0` compares equal to our `0.0` (both stacks serialize the same
/// semantic value with different int/float types).
#[derive(Debug, PartialEq)]
enum Canon {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Canon>),
    Obj(std::collections::BTreeMap<String, Canon>),
}

fn canon(v: &Value) -> Canon {
    match v {
        Value::Null => Canon::Null,
        Value::Bool(b) => Canon::Bool(*b),
        Value::Number(n) => Canon::Num(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => Canon::Str(s.clone()),
        Value::Array(a) => Canon::Arr(a.iter().map(canon).collect()),
        Value::Object(o) => Canon::Obj(o.iter().map(|(k, v)| (k.clone(), canon(v))).collect()),
    }
}

/// Collect human-readable differences between two canon trees.
fn canon_diff(a: &Canon, b: &Canon, path: &str, out: &mut Vec<String>) {
    match (a, b) {
        (Canon::Obj(ma), Canon::Obj(mb)) => {
            for k in ma.keys().chain(mb.keys()).collect::<BTreeSet<_>>() {
                match (ma.get(k), mb.get(k)) {
                    (Some(va), Some(vb)) => canon_diff(va, vb, &format!("{path}.{k}"), out),
                    (Some(_), None) => out.push(format!("{path}.{k}: only in left")),
                    (None, Some(_)) => out.push(format!("{path}.{k}: only in right")),
                    (None, None) => unreachable!(),
                }
            }
        }
        (Canon::Arr(aa), Canon::Arr(ab)) => {
            if aa.len() != ab.len() {
                out.push(format!(
                    "{path}: array lengths {} vs {}",
                    aa.len(),
                    ab.len()
                ));
            } else {
                for (i, (va, vb)) in aa.iter().zip(ab).enumerate() {
                    canon_diff(va, vb, &format!("{path}[{i}]"), out);
                }
            }
        }
        (x, y) => {
            if x != y {
                out.push(format!("{path}: {x:?} vs {y:?}"));
            }
        }
    }
}

/// Scrub a client's -j sample for SAME-DAEMON comparison. Everything the
/// scrub touches is asserted first, so the normalization cannot hide a
/// divergence:
///
///   * transport_uptime: monotonic daemon clock, necessarily different for
///     two invocations; asserted positive and plausible, then blanked;
///   * rss: rnsd reports its (constantly moving) process RSS, lnsd reports
///     null; asserted to match the stack, then blanked;
///   * observer footprint (rnsd only): rnstatus connects as a shared
///     instance client and therefore sees ITSELF: exactly one extra
///     LocalClientInterface entry, and clients=1 on the Shared Instance
///     entry. lnstatus talks RPC-only and must see zero/0. The scrub asserts
///     exactly that expectation and removes the footprint so the remaining
///     structures are comparable.
fn scrub_client_sample(stats: &mut Value, client: StatusClient, stack: Stack) {
    let uptime = num(stats, "transport_uptime");
    assert!(
        uptime > 0.0 && uptime < 3600.0,
        "{} on {}: implausible transport_uptime {uptime}",
        client.label(),
        stack.label()
    );
    stats["transport_uptime"] = Value::Null;

    match stack {
        Stack::Lnsd => assert!(
            stats.get("rss").is_some_and(|v| v.is_null()),
            "lnsd must report rss=null, got {:?}",
            stats.get("rss")
        ),
        Stack::Rnsd => assert!(
            num(stats, "rss") > 0.0,
            "rnsd must report a positive process rss, got {:?}",
            stats.get("rss")
        ),
    }
    stats["rss"] = Value::Null;

    // rnstatus connects to EITHER daemon as a shared-instance client, so its
    // own LocalClientInterface row is expected on both since Codeberg #177;
    // lnstatus talks RPC only and is never in the list.
    let _ = stack;
    let expect_self = client == StatusClient::Rnstatus;
    if let Some(list) = stats.get_mut("interfaces").and_then(|v| v.as_array_mut()) {
        let before = list.len();
        list.retain(|i| i["type"] != "LocalClientInterface");
        let removed = before - list.len();
        assert_eq!(
            removed,
            usize::from(expect_self),
            "{} on {}: unexpected LocalClientInterface observer count",
            client.label(),
            stack.label()
        );
        for iface in list.iter_mut() {
            if iface["type"] == "LocalServerInterface" {
                let clients = num(iface, "clients") as i64;
                assert_eq!(
                    clients,
                    i64::from(expect_self),
                    "{} on {}: Shared Instance clients must count exactly the observer",
                    client.label(),
                    stack.label()
                );
                iface["clients"] = Value::from(0);
            }
            // burst_activated / pr_burst_activated are absolute wall-clock
            // epoch timestamps (Interface.py ic_burst_activated /
            // ic_pr_burst_activated; rnstatus renders `now - activated` as the
            // burst duration, rnstatus.py:566). The daemon derives them from a
            // fresh SystemTime::now() epoch base on every query, so two
            // sequential samples of the same live daemon differ in the low
            // digits (e.g. 1783071384.7079797 vs 1783071384.70798) and the
            // exact-float structural compare flakes. Assert the KEY is emitted
            // by both stacks (the parity guarantee), then normalise the
            // volatile VALUE to a fixed sentinel. Mirrors `clients` above; an
            // idle interface already reads the int 0, which this leaves intact.
            for key in ["burst_activated", "pr_burst_activated"] {
                assert!(
                    iface.get(key).is_some(),
                    "{} on {}: interface -j must emit `{key}`",
                    client.label(),
                    stack.label()
                );
                iface[key] = Value::from(0);
            }
        }
    }
}

/// Assert two scrubbed samples are structurally IDENTICAL.
fn assert_json_identical(left: &Value, right: &Value, what: &str) {
    let mut diffs = Vec::new();
    canon_diff(&canon(left), &canon(right), "$", &mut diffs);
    assert!(
        diffs.is_empty(),
        "{what}: -j structures differ:\n  {}\nleft:\n{}\nright:\n{}",
        diffs.join("\n  "),
        serde_json::to_string_pretty(left).unwrap_or_default(),
        serde_json::to_string_pretty(right).unwrap_or_default(),
    );
}

// =========================================================================
// Text normalization (per-flag render parity)
// =========================================================================

/// Normalize the documented volatile parts of a status text sample. In the
/// FROZEN state the only volatile line is the uptime (every frequency and
/// speed reads exactly 0 by the freeze predicate, so their renderings are
/// already identical); during the burst the elapsed-time suffix, decaying
/// frequencies, speeds and the held count are additionally normalized, each
/// backed by a separate numeric assertion so the normalization never hides a
/// real divergence. Whitespace is collapsed only on lines that were touched,
/// because value width changes shift rnstatus's column padding.
///
/// Three artifacts of SEQUENTIAL sampling on a live daemon are normalized
/// structurally (their substance is asserted separately, see the during-burst
/// section):
///
///   * the `<HZ>/c` (or `/p`) per-client rate suffix: rnstatus samples with
///     itself connected as a shared-instance client (clients=1 on the Shared
///     Instance entry), lnstatus samples client-free (clients=0), so the
///     suffix presence differs by observer footprint exactly like the
///     `clients` field the -j scrub pins;
///   * the ` burst for <T>` suffix: on rnsd the parent listener's burst flag
///     is advanced by the 5 s jobs cadence and can flip between the two
///     samples; presence on the daemon under test is asserted explicitly
///     before normalization;
///   * padding between a frequency and its ↑/↓ arrow: rnstatus pads the
///     shorter of the two frequency strings for column alignment, and the
///     two clients sample different (decaying) values with different widths.
fn normalize_status_text(text: &str) -> String {
    let re_time = Regex::new(r"\d+(?:\.\d+)?(?:d|h|m|s)\b").unwrap();
    // Frequencies render sub-Hz multipliers (mHz/µHz) once the deque decay
    // sets in, so the multiplier class covers them as well.
    let re_hz = Regex::new(r"\d+(?:\.\d+)?\s[munµKMGTPEZY]?Hz").unwrap();
    let re_bps = Regex::new(r"\d+(?:\.\d+)?\s[kKMGTPEZY]?bps").unwrap();
    let re_held = Regex::new(r"\d+ announces?\b").unwrap();
    let re_per_client = Regex::new(r"\s*<HZ>/[cp]").unwrap();
    let re_burst_for = Regex::new(r"\s*burst for <T>(?:, <T>)*(?: and <T>)?").unwrap();

    text.lines()
        .map(|line| {
            let mut l = line.to_string();
            let mut touched = false;
            if l.contains("Uptime is") || l.contains("burst for") {
                // The announce-rate suffix (t:1h/p:0s/g:5) is stable config,
                // shielded from the time regex by masking it first.
                let shielded = l.replace("(t:1h/p:0s/g:5)", "(AR)");
                let replaced = re_time.replace_all(&shielded, "<T>").into_owned();
                l = replaced.replace("(AR)", "(t:1h/p:0s/g:5)");
                touched = true;
            }
            if re_hz.is_match(&l) {
                l = re_hz.replace_all(&l, "<HZ>").into_owned();
                touched = true;
            }
            if !l.contains("Rate ") && re_bps.is_match(&l) {
                l = re_bps.replace_all(&l, "<BPS>").into_owned();
                touched = true;
            }
            if l.contains("Held") && re_held.is_match(&l) {
                l = re_held.replace_all(&l, "<N> announces").into_owned();
                touched = true;
            }
            if touched {
                // Structural artifacts of sequential sampling (see above).
                l = re_per_client.replace_all(&l, "").into_owned();
                l = re_burst_for.replace_all(&l, "").into_owned();
                l = l.split_whitespace().collect::<Vec<_>>().join(" ");
                l = l.replace("<HZ> ↑", "<HZ>↑").replace("<HZ> ↓", "<HZ>↓");
            }
            l
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove the sampling client's OWN shared-instance connection from an `-a`
/// render, and assert the footprint is exactly what the observer model
/// predicts while doing it.
///
/// `rnstatus` reaches a daemon by connecting to its shared instance as a
/// client (Reticulum.py:418-425), so with `-a` it sees one extra
/// `LocalInterface[...]` block: itself. `lnstatus` talks RPC only and sees
/// none. This is the same observer asymmetry the `-j` scrub pins exactly
/// (see `scrub_client_sample`), and the only thing that can differ between
/// the two clients' `-a` renders of one frozen daemon.
fn strip_observer_footprint(text: &str, expected_blocks: usize, who: &str) -> String {
    // Per-client rate suffix on the Shared Instance block: rnstatus counts
    // itself as a client and therefore renders `<HZ>/c` there, lnstatus does
    // not. Same footprint, expressed as a rate instead of a row; the exact
    // client count it comes from is pinned by the -j scrub. Only the Shared
    // Instance block is touched - the TCP listener's `/c` suffix counts real
    // peers and is compared as-is.
    let re_per_client = Regex::new(r"\s*\d+(?:\.\d+)?\s*[munµKMGTPEZY]?Hz/[cp]\s*$").unwrap();
    let mut out: Vec<String> = Vec::new();
    let mut removed = 0usize;
    let mut skipping = false;
    let mut in_shared_instance = false;
    for line in text.lines() {
        if skipping {
            // An interface block's body lines are indented four spaces.
            if line.starts_with("    ") {
                continue;
            }
            skipping = false;
        }
        if line.starts_with(" LocalInterface[") {
            skipping = true;
            removed += 1;
            // Drop the blank separator that introduced the block.
            if out.last().is_some_and(|l| l.is_empty()) {
                out.pop();
            }
            continue;
        }
        if !line.starts_with("    ") {
            in_shared_instance = line.starts_with(" Shared Instance[");
        }
        if in_shared_instance && line.starts_with("    ") {
            out.push(re_per_client.replace(line, "").trim_end().to_string());
            continue;
        }
        out.push(line.to_string());
    }
    assert_eq!(
        removed, expected_blocks,
        "{who}: expected {expected_blocks} own shared-instance connection(s) in the -a render, \
         found {removed}:\n{text}"
    );
    out.join("\n")
}

/// Extract the held-announce count a client rendered (from its -A output).
fn parse_held(text: &str) -> Option<u64> {
    let re = Regex::new(r"Held\s+: (\d+) announce").unwrap();
    re.captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

// =========================================================================
// The scenario
// =========================================================================

/// Full 2x2 matrix under one controlled traffic script. Single test on
/// purpose: both daemons must live through the exact same scenario at the
/// same wall-clock time for the A/B to be valid, and the expensive phases
/// (burst drain, freeze) are shared by every comparison.
#[tokio::test]
#[ignore = "spawns Python daemons and tooling; run via --include-ignored after the tier3"]
async fn status_parity_matrix_2x2() {
    init_tracing();

    let script = build_script();

    // ---- Spawn both daemons and connect one injector to each. ----
    let lnsd = ParityDaemon::start(Stack::Lnsd).await;
    let rnsd = ParityDaemon::start(Stack::Rnsd).await;

    let mut injectors = Injectors::connect(&lnsd, &rnsd).await;

    // ---- Phase 1: STEADY announces. ----
    // Anti-flake: paced below the ingress threshold, then the path sets are
    // POLLED to the exact expected value on both daemons (count based, no
    // fixed post-send sleep).
    let steady_frames: Vec<&[u8]> = script.steady.iter().map(|(f, _)| f.as_slice()).collect();
    injectors.send_paced(&steady_frames, SLOW_PACE).await;

    let steady_set: BTreeSet<String> = script
        .steady
        .iter()
        .map(|(_, h)| hex::encode(h.as_bytes()))
        .collect();
    for daemon in [&lnsd, &rnsd] {
        let paths = wait_paths(
            daemon,
            "steady paths learned",
            Duration::from_secs(30),
            |p| p == &steady_set,
        )
        .await;
        assert_eq!(paths, steady_set);
    }

    // ---- Phase 2: PATH REQUESTS. ----
    let pr_frames: Vec<&[u8]> = script.path_requests.iter().map(|f| f.as_slice()).collect();
    injectors.send_paced(&pr_frames, SLOW_PACE).await;

    // Volume evidence for #67 pr_frequency: the traffic interface on BOTH
    // daemons must show a real (non-zero) incoming PR frequency and must
    // have SENT bytes (the three known-destination requests are answered
    // with path responses). Polled, not slept; the frequency needs more
    // than IC_DEQUE_MIN_SAMPLE=2 deque entries, which 4 requests guarantee.
    for daemon in [&lnsd, &rnsd] {
        wait_stats(
            daemon,
            "incoming_pr_frequency > 0 and path responses sent",
            Duration::from_secs(20),
            Duration::from_millis(500),
            |stats| {
                let t = traffic_iface(stats);
                num(&t, "incoming_pr_frequency") > 0.0
                    && num(&t, "outgoing_announce_frequency") > 0.0
                    && num(&t, "txb") > 0.0
            },
        )
        .await;
    }

    // ---- Phase 3: BURST (Codeberg #87). ----
    // Quiet gap first so the steady/PR-phase samples are decay-eligible and
    // the burst crosses the ingress threshold on both stacks (see
    // PRE_BURST_DECAY_GAP). This is a traffic-script property (when frames
    // are sent), not an assertion timing.
    tokio::time::sleep(PRE_BURST_DECAY_GAP).await;
    let burst_frames: Vec<&[u8]> = script.burst.iter().map(|(f, _)| f.as_slice()).collect();
    injectors.send_paced(&burst_frames, BURST_PACE).await;
    let burst_end = Instant::now();

    // The limiter must have engaged on both stacks: burst_active reported
    // and announces held. Polled immediately after the blast; the reads sit
    // deep inside the 15 s IC_BURST_PENALTY window (no release can have
    // happened yet), so held counts are STABLE while we sample.
    let lnsd_burst = wait_stats(
        &lnsd,
        "burst engaged",
        Duration::from_secs(5),
        Duration::from_millis(200),
        |s| any_burst(s) && held_total(s) > 0,
    )
    .await;
    let rnsd_burst = wait_stats(
        &rnsd,
        "burst engaged",
        Duration::from_secs(5),
        Duration::from_millis(200),
        |s| any_burst(s) && held_total(s) > 0,
    )
    .await;

    // Cross-daemon held-volume guard. The activation index depends on the
    // per-stack frequency arithmetic hitting the 3 Hz threshold one or two
    // frames apart (float timing on a 25 Hz stream), so an exact match is
    // not a valid expectation; a small window plus a hard lower bound is.
    let (held_l, held_r) = (held_total(&lnsd_burst), held_total(&rnsd_burst));
    assert!(
        held_l >= 5 && held_r >= 5,
        "both stacks must hold a substantial part of the burst (lnsd={held_l}, rnsd={held_r})"
    );
    assert!(
        held_l.abs_diff(held_r) <= 4,
        "held-announce volumes diverge beyond activation jitter: lnsd={held_l} rnsd={held_r}"
    );

    // DURING-BURST client sampling (#87 visibility through the real tools).
    // Per daemon the two clients run SEQUENTIALLY (observer determinism) but
    // the two daemons are sampled in parallel so everything lands inside the
    // penalty window. The held count each client rendered is asserted
    // against the RPC snapshot before the text normalization blanks it.
    let (lnsd_texts, rnsd_texts) = tokio::join!(
        async {
            // `-a`: the burst lives on the SPAWNED connection, which both
            // clients hide by default (the `TCPInterface[Client` name prefix,
            // rnstatus.py:393-401) since lnsd names it like the reference
            // (Codeberg #177). The held substance is only renderable with the
            // hidden interfaces shown.
            let rn = run_status(StatusClient::Rnstatus, &lnsd, &["-a", "-A"]).await;
            wait_observer_gone(&lnsd).await;
            let ln = run_status(StatusClient::Lnstatus, &lnsd, &["-a", "-A"]).await;
            let lb = run_status(StatusClient::Lnstatus, &lnsd, &["-a", "-B"]).await;
            (rn, ln, lb)
        },
        async {
            let rn = run_status(StatusClient::Rnstatus, &rnsd, &["-A"]).await;
            wait_observer_gone(&rnsd).await;
            let ln = run_status(StatusClient::Lnstatus, &rnsd, &["-A"]).await;
            (rn, ln)
        }
    );

    // With the spawned connection shown, both clients must render the held
    // count and the burst suffix. Tolerance of 2 on the parsed count vs the
    // RPC snapshot covers a release schedule that starts while a client is
    // still sampling under extreme load; the burst itself cannot clear that
    // fast (frequency decay plus 15 s hold).
    let (rn_l, ln_l, lnsd_burst_filter) = &lnsd_texts;
    for (who, text) in [("rnstatus", rn_l), ("lnstatus", ln_l)] {
        let held = parse_held(text).unwrap_or_else(|| {
            panic!("{who} on lnsd must render a Held line during the burst, got:\n{text}")
        });
        assert!(
            held > 0 && held_l.saturating_sub(held) <= 2,
            "{who} on lnsd rendered held={held}, RPC snapshot said {held_l}"
        );
        assert!(
            text.contains("burst for"),
            "{who} on lnsd must render the burst suffix during the burst, got:\n{text}"
        );
    }
    // Strip BEFORE normalizing: the normalizer collapses whitespace on the
    // lines it touches, which would erase the indentation the block scanner
    // uses to find a block's body.
    assert_eq!(
        normalize_status_text(&strip_observer_footprint(rn_l, 1, "rnstatus")),
        normalize_status_text(&strip_observer_footprint(ln_l, 0, "lnstatus")),
        "during-burst -a -A render parity on lnsd"
    );
    // -B (burst filter) must show the bursting interface on lnsd, under the
    // reference name it now carries.
    assert!(
        lnsd_burst_filter.contains("TCPInterface[Client on Parity TCP Server 0/"),
        "lnstatus -a -B on lnsd must list the bursting interface, got:\n{lnsd_burst_filter}"
    );

    // On rnsd the burst lives on the spawned connection entry, which
    // rnstatus HIDES by default (TCPInterface[Client prefix), so the held
    // substance is asserted via RPC above; the -A text parity between the
    // clients still holds on the visible entries. The per-client announce
    // rate (`.../c`) must be rendered by BOTH clients for the TCP listener
    // (clients=1, the injector connection) - asserted here because the
    // normalization strips the observer-dependent occurrence on the Shared
    // Instance entry (rnstatus counts itself, lnstatus is RPC-only).
    let (rn_r, ln_r) = &rnsd_texts;
    for (who, text) in [("rnstatus", rn_r), ("lnstatus", ln_r)] {
        assert!(
            text.contains("/c"),
            "{who} on rnsd must render the per-client announce rate, got:\n{text}"
        );
    }
    assert_eq!(
        normalize_status_text(rn_r),
        normalize_status_text(ln_r),
        "during-burst -A render parity on rnsd"
    );

    // ---- Phase 3b: DRAIN (hold-and-release, not drop). ----
    // Every burst announce must eventually reach the path table (release
    // schedule: 15 s penalty, then one per 5 s), and the held queues must
    // drain to zero with the burst flag cleared. Deadline sized to the
    // schedule (15 + 20*5 = 115 s) plus slack, never to a guessed latency.
    let full_set: BTreeSet<String> = steady_set
        .iter()
        .cloned()
        .chain(script.burst.iter().map(|(_, h)| hex::encode(h.as_bytes())))
        .collect();
    for daemon in [&lnsd, &rnsd] {
        let paths = wait_paths(
            daemon,
            "all burst announces released into the path table",
            Duration::from_secs(200),
            |p| p == &full_set,
        )
        .await;
        assert_eq!(paths, full_set, "hold-and-release must lose nothing");
        wait_stats(
            daemon,
            "held queue drained and burst cleared",
            Duration::from_secs(60),
            Duration::from_secs(1),
            |s| held_total(s) == 0 && !any_burst(s),
        )
        .await;
    }
    tracing::info!(
        "burst drained on both daemons after {:?} (held peaks lnsd={held_l} rnsd={held_r})",
        burst_end.elapsed()
    );

    // ---- Phase 4: BLACKHOLE (Codeberg #88), via the real operator tool. ----
    for daemon in [&lnsd, &rnsd] {
        let out = run_python_tool(
            RNPATH_PY,
            &["-B", &script.victim_identity_hex],
            &daemon.config_dir,
        )
        .await;
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("Blackholed identity"),
            "rnpath -B against {} failed: status {:?} stdout {:?} stderr {:?}",
            daemon.stack.label(),
            out.status.code(),
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        wait_observer_gone(daemon).await;
    }

    let victim_hex = hex::encode(script.victim_hash.as_bytes());
    // Blackholing removes the already-learned path row on both stacks.
    for daemon in [&lnsd, &rnsd] {
        wait_paths(
            daemon,
            "victim path removed by blackhole",
            Duration::from_secs(15),
            |p| !p.contains(&victim_hex),
        )
        .await;
    }

    // The blackhole set itself must read back identically through the RPC
    // both daemons expose (this is the is_blackholed surface rnpath uses).
    for daemon in [&lnsd, &rnsd] {
        let bh = daemon.blackholed().await;
        let entry = bh.get(&script.victim_identity_hex).unwrap_or_else(|| {
            panic!(
                "{}: blackholed_identities must contain the victim, got {bh}",
                daemon.stack.label()
            )
        });
        assert!(
            entry.get("until").is_some_and(|u| u.is_null()),
            "{}: rnpath -B sets no expiry, until must be null, got {entry}",
            daemon.stack.label()
        );
    }

    // Victim re-announce is DROPPED; the sentinel that follows on the same
    // ordered TCP stream is processed. Waiting for the sentinel path proves
    // the victim frame was consumed before we assert its absence (ordering
    // based, no sleep-and-hope).
    injectors
        .send_paced(&[&script.victim_reannounce, &script.sentinel.0], SLOW_PACE)
        .await;
    let sentinel_hex = hex::encode(script.sentinel.1.as_bytes());
    for daemon in [&lnsd, &rnsd] {
        wait_paths(
            daemon,
            "sentinel announce processed",
            Duration::from_secs(15),
            |p| p.contains(&sentinel_hex),
        )
        .await;
        let paths = daemon.path_hashes().await;
        assert!(
            !paths.contains(&victim_hex),
            "{}: the blackholed identity's announce must not create a path",
            daemon.stack.label()
        );
    }

    // Lift the blackhole; the victim announces normally again on both.
    for daemon in [&lnsd, &rnsd] {
        let out = run_python_tool(
            RNPATH_PY,
            &["-U", &script.victim_identity_hex],
            &daemon.config_dir,
        )
        .await;
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("Lifted blackhole"),
            "rnpath -U against {} failed: status {:?} stdout {:?} stderr {:?}",
            daemon.stack.label(),
            out.status.code(),
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        wait_observer_gone(daemon).await;
    }
    injectors
        .send_paced(&[&script.victim_after_unblackhole], SLOW_PACE)
        .await;
    for daemon in [&lnsd, &rnsd] {
        wait_paths(
            daemon,
            "victim path restored after unblackhole",
            Duration::from_secs(15),
            |p| p.contains(&victim_hex),
        )
        .await;
    }

    // ---- Phase 5: FREEZE. ----
    // Poll both daemons until every volatile stat reads its exact frozen
    // value and has held it for a full sampling period. The polling itself
    // drains the frequency deques on both stacks (identical n/span-with-decay
    // semantics), so this converges deterministically; the deadline is sized
    // to the deque length (~30 samples), not guessed. The settle window is
    // what separates a speed that has decayed to 0 from one that has not been
    // sampled yet — see FREEZE_SETTLE.
    for daemon in [&lnsd, &rnsd] {
        wait_frozen(
            daemon,
            "all volatile stats settled (freeze)",
            Duration::from_secs(240),
            |_| true,
        )
        .await;
    }

    // ---- FINAL VOLUME GUARD (fail loudly before any comparison). ----
    let expected_final: BTreeSet<String> = full_set
        .iter()
        .cloned()
        .chain([sentinel_hex.clone()])
        .collect();
    let paths_l = lnsd.path_hashes().await;
    let paths_r = rnsd.path_hashes().await;
    assert_eq!(
        paths_l, expected_final,
        "VOLUME GUARD: lnsd path set diverges from the script expectation"
    );
    assert_eq!(
        paths_r, expected_final,
        "VOLUME GUARD: rnsd path set diverges from the script expectation"
    );

    let stats_l = lnsd.stats().await;
    let stats_r = rnsd.stats().await;
    let (traffic_l, traffic_r) = (traffic_iface(&stats_l), traffic_iface(&stats_r));

    // ---- TX FRAME CENSUS (Codeberg #192). ----
    // The injector connection is each daemon's only non-local interface, so
    // everything it received back IS what the traffic interface transmitted.
    // Decoded before the byte comparison so a txb delta is reported as
    // frames-with-content, not as an unexplained percentage.
    let cen_l = census(&injectors.lnsd.captured());
    let cen_r = census(&injectors.rnsd.captured());
    tracing::info!(
        "TX CENSUS lnsd ({} frames, {} wire bytes):\n{}",
        cen_l.len(),
        injectors.lnsd.wire_bytes(),
        render_census(&cen_l)
    );
    tracing::info!(
        "TX CENSUS rnsd ({} frames, {} wire bytes):\n{}",
        cen_r.len(),
        injectors.rnsd.wire_bytes(),
        render_census(&cen_r)
    );
    // The census is trusted only because it measures the guard's OWN number:
    // the framed bytes the capture read off the socket must equal the txb the
    // daemon reports for that interface. A short settle covers socket lag
    // between the daemon's write (where it counts) and our read; the state is
    // frozen, so nothing new can arrive during it.
    for (label, injector, traffic) in [
        ("lnsd", &injectors.lnsd, &traffic_l),
        ("rnsd", &injectors.rnsd, &traffic_r),
    ] {
        let txb = num(traffic, "txb") as u64;
        let deadline = Instant::now() + Duration::from_secs(5);
        while injector.wire_bytes() < txb && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            injector.wire_bytes(),
            txb,
            "TX CENSUS on {label}: the capture ({} framed bytes) does not \
             account for the interface's reported txb ({txb}); the census \
             would be describing something other than the guarded number",
            injector.wire_bytes(),
        );
        tracing::info!("TX CENSUS {label}: captured framed bytes = reported txb = {txb}");
    }
    let surplus_l = frame_surplus(&cen_l, &cen_r);
    let surplus_r = frame_surplus(&cen_r, &cen_l);
    let render_surplus = |s: &[TxFrame]| {
        if s.is_empty() {
            "    (none)".to_string()
        } else {
            s.iter()
                .map(|f| format!("    {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    tracing::info!(
        "TX CENSUS surplus lnsd-over-rnsd ({}):\n{}\nsurplus rnsd-over-lnsd ({}):\n{}",
        surplus_l.len(),
        render_surplus(&surplus_l),
        surplus_r.len(),
        render_surplus(&surplus_r),
    );
    // The frame-level form of the volume guard: identical frames in, so the
    // frames OUT must be the same multiset on both stacks. Not a byte
    // percentage - which of the two stacks put an announce on the wire that
    // the other did not, and what was in it.
    //
    // Codeberg #192 was found here: lnsd transmitted 62 announce-sized frames
    // where the reference transmitted 59, and the three surplus frames
    // decoded as duplicate PATH_RESPONSEs for the three answered path
    // requests - our path-response announce-table entry started at
    // `retries = 0` (a received announce's value, rebroadcast twice) instead
    // of the reference's `retries = PATHFINDER_R` (Transport.py:2970, fires
    // once). Fixed in `transport.rs handle_path_request`; the mechanism is
    // pinned sans-I/O in `leviculum-core node::mvr_path_response_retries`.
    //
    // Timing is deliberately NOT compared: the two stacks schedule the same
    // frame within their own jitter windows, and the census shows offsets
    // differing by seconds on frames that are otherwise identical.
    assert!(
        surplus_l.is_empty() && surplus_r.is_empty(),
        "TX CENSUS: the two stacks transmitted different frames on identical \
         injected traffic.\nlnsd-only ({}):\n{}\nrnsd-only ({}):\n{}\n\
         full lnsd census ({} frames):\n{}\nfull rnsd census ({} frames):\n{}",
        surplus_l.len(),
        render_surplus(&surplus_l),
        surplus_r.len(),
        render_surplus(&surplus_r),
        cen_l.len(),
        render_census(&cen_l),
        cen_r.len(),
        render_census(&cen_r),
    );

    for key in ["rxb", "txb"] {
        let (a, b) = (num(&traffic_l, key), num(&traffic_r, key));
        assert!(
            a > 0.0 && b > 0.0,
            "VOLUME GUARD: {key} must be non-zero on both traffic interfaces"
        );
        let rel = (a - b).abs() / a.max(b);
        // Logged in BOTH states, not only on the assert: a tolerance whose
        // margin nobody has ever seen is a tolerance nobody can defend
        // (docs/src/concepts/evidence-and-honesty.md).
        tracing::info!(
            "VOLUME GUARD {key}: lnsd={a} rnsd={b} delta={:.2}% (limit 5%)",
            rel * 100.0
        );
        assert!(
            rel <= 0.05,
            "VOLUME GUARD: traffic interface {key} diverges {:.1}% (lnsd={a}, rnsd={b}); \
             identical frames were injected, comparison would be invalid",
            rel * 100.0
        );
    }

    // ---- Frozen 2x2 matrix: the -j JSON anchor. ----
    // Client parity: after the documented scrub the two clients' -j output
    // against the SAME daemon must be structurally IDENTICAL: identical
    // interface set, identical key sets, every stable field bit-equal.
    for daemon in [&lnsd, &rnsd] {
        let rn_raw = run_status(StatusClient::Rnstatus, daemon, &["-j"]).await;
        wait_observer_gone(daemon).await;
        let ln_raw = run_status(StatusClient::Lnstatus, daemon, &["-j"]).await;

        let mut rn: Value = serde_json::from_str(rn_raw.trim())
            .unwrap_or_else(|e| panic!("rnstatus -j on {} not JSON: {e}", daemon.stack.label()));
        let mut ln: Value = serde_json::from_str(ln_raw.trim())
            .unwrap_or_else(|e| panic!("lnstatus -j on {} not JSON: {e}", daemon.stack.label()));
        scrub_client_sample(&mut rn, StatusClient::Rnstatus, daemon.stack);
        scrub_client_sample(&mut ln, StatusClient::Lnstatus, daemon.stack);
        assert_json_identical(
            &rn,
            &ln,
            &format!("client -j parity on {}", daemon.stack.label()),
        );
    }

    // Daemon parity (#67): the reference client reads both daemons; compare
    // via rnstatus -j so the daemon side is the only variable.
    let rn_on_l: Value = serde_json::from_str(
        run_status(StatusClient::Rnstatus, &lnsd, &["-j"])
            .await
            .trim(),
    )
    .expect("rnstatus -j on lnsd JSON");
    wait_observer_gone(&lnsd).await;
    let rn_on_r: Value = serde_json::from_str(
        run_status(StatusClient::Rnstatus, &rnsd, &["-j"])
            .await
            .trim(),
    )
    .expect("rnstatus -j on rnsd JSON");
    wait_observer_gone(&rnsd).await;
    assert_daemon_stats_parity(&rn_on_l, &rn_on_r);

    // ---- Frozen per-flag render parity. ----
    // In the frozen state every frequency and speed renders "0 Hz"/"0 bps"
    // on both clients (asserted by the freeze predicate), so the only
    // normalized line is the uptime. Everything else must match byte for
    // byte. Flags are run sequentially per daemon so each pair reads the
    // same observer-free state.
    let flag_sets: &[&[&str]] = &[
        &[],
        &["-A"],
        &["-P"],
        &["-A", "-P"],
        &["-B"],
        &["-l"],
        &["-t"],
        &["-s", "traffic"],
        &["tcp"],
    ];
    for daemon in [&lnsd, &rnsd] {
        for flags in flag_sets {
            let rn = run_status(StatusClient::Rnstatus, daemon, flags).await;
            wait_observer_gone(daemon).await;
            let ln = run_status(StatusClient::Lnstatus, daemon, flags).await;
            assert_eq!(
                normalize_status_text(&rn),
                normalize_status_text(&ln),
                "render parity for flags {:?} on {}\nrnstatus:\n{}\nlnstatus:\n{}",
                flags,
                daemon.stack.label(),
                rn,
                ln
            );
        }
    }

    // ---- lnstatus on lnsd (full Rust stack): concrete sanity ranges. ----
    let ln_on_l: Value = serde_json::from_str(
        run_status(StatusClient::Lnstatus, &lnsd, &["-j"])
            .await
            .trim(),
    )
    .expect("lnstatus -j on lnsd JSON");
    assert_full_stack_sane(&ln_on_l);

    // Cleanup (only reached on success; failures keep the dirs + daemon
    // logs for diagnosis).
    drop(injectors);
    let (l_dir, r_dir) = (lnsd.config_dir.clone(), rnsd.config_dir.clone());
    drop(lnsd);
    drop(rnsd);
    let _ = std::fs::remove_dir_all(l_dir);
    let _ = std::fs::remove_dir_all(r_dir);
}

// =========================================================================
// Full-inventory parity (Codeberg #177)
// =========================================================================

/// One inventory row reduced to its IDENTITY: what a monitoring script keys
/// on. Volatile and per-daemon values (byte counters, hashes, ports, instance
/// name) are templated out by [`templated_inventory`]; what remains must be
/// equal across daemons, name included.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct InventoryRow {
    type_name: String,
    name: String,
    short_name: String,
    parent_name: Option<String>,
    clients: Option<i64>,
}

impl std::fmt::Display for InventoryRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<21} name={:<58} short_name={:<32} parent={:<40} clients={:?}",
            self.type_name,
            self.name,
            format!("{:?}", self.short_name),
            self.parent_name.as_deref().unwrap_or("-"),
            self.clients
        )
    }
}

/// Read the daemon's FULL interface inventory through the reference client and
/// reduce it to templated identity rows.
///
/// `rnstatus -j` dumps the raw `interface_stats` dict without applying the
/// display filter (rnstatus.py:344-357), so this sees hidden rows too - the
/// inventory, not the rendering.
///
/// Templating replaces exactly the values that CANNOT be equal across two
/// daemons running side by side, and nothing else:
///   * the daemon's instance name -> `<INSTANCE>` (each daemon needs its own
///     shared-instance socket);
///   * its configured listen port -> `<LISTEN_PORT>` (each needs its own);
///   * any remaining `:<port>` -> `<EPHEMERAL>` (kernel-assigned client ports);
///   * the leading `<n>@` of an IPC client's short_name -> `<N>@` (the
///     reference labels a client with the live client count at accept time;
///     see the pinned deviation in the test that uses this).
async fn templated_inventory(daemon: &ParityDaemon) -> Vec<InventoryRow> {
    let raw = run_status(StatusClient::Rnstatus, daemon, &["-j"]).await;
    let stats: Value = serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("rnstatus -j on {} not JSON: {e}", daemon.stack.label()));

    let ephemeral = Regex::new(r":\d+").unwrap();
    let client_index = Regex::new(r"^\d+@").unwrap();
    let template = |s: &str| -> String {
        let s = s.replace(&daemon.instance_name, "<INSTANCE>");
        let s = s.replace(&format!(":{}", daemon.tcp_port), ":<LISTEN_PORT>");
        let s = ephemeral.replace_all(&s, ":<EPHEMERAL>").into_owned();
        client_index.replace(&s, "<N>@").into_owned()
    };

    let mut rows: Vec<InventoryRow> = ifaces(&stats)
        .iter()
        .map(|i| InventoryRow {
            type_name: i["type"].as_str().unwrap_or("?").to_string(),
            name: template(i["name"].as_str().unwrap_or("?")),
            short_name: template(i["short_name"].as_str().unwrap_or("?")),
            parent_name: i
                .get("parent_interface_name")
                .and_then(|p| p.as_str())
                .map(template),
            clients: i.get("clients").and_then(|c| c.as_i64()),
        })
        .collect();
    rows.sort();
    rows
}

fn render_inventory(rows: &[InventoryRow]) -> String {
    if rows.is_empty() {
        return "    (no interfaces reported)".to_string();
    }
    rows.iter()
        .map(|r| format!("    {r}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_inventory_parity(lnsd: &[InventoryRow], rnsd: &[InventoryRow], phase: &str) {
    assert_eq!(
        lnsd,
        rnsd,
        "\nINVENTORY PARITY ({phase}): the same reference client was told about \
         different worlds.\nlnsd:\n{}\nrnsd:\n{}\n",
        render_inventory(lnsd),
        render_inventory(rnsd),
    );
}

/// Whole-inventory drop-in parity (Codeberg #177).
///
/// The 2x2 matrix compares the TRAFFIC-BEARING interface field by field, which
/// is why a difference in the interface LIST stayed green: a monitoring script
/// written against `rnsd` got an empty answer from `lnsd` where it expected
/// three rows, and the query still succeeded. This test compares the full
/// inventory - every row, its type, its name, its short_name, its parent link
/// and its client count - through the SAME reference client against both
/// daemons, in the two states an operator actually looks at:
///
///   1. idle: no peer connected, only the daemon's own listeners plus the
///      sampling client itself;
///   2. one connected TCP peer, waited for by the path it announces (a state
///      both stacks reach observably, never a sleep).
///
/// Deliberate, pinned deviations (see `docs/src/concepts/client-tools.md`):
///   * an IPC client's `short_name` carries a connection index that the
///     reference derives from the LIVE client count at accept time
///     (LocalInterface.py:441); ours mirrors that, but the index is templated
///     here so the assertion does not depend on connect/disconnect ordering
///     between the two daemons;
///   * `clients` on a `TCPServerInterface` counts live spawned connections on
///     both stacks, and is compared exactly.
#[tokio::test]
#[ignore = "spawns Python daemons and tooling; run via --include-ignored after the tier3"]
async fn status_inventory_parity_across_daemons() {
    init_tracing();

    let lnsd = ParityDaemon::start(Stack::Lnsd).await;
    let rnsd = ParityDaemon::start(Stack::Rnsd).await;

    // ---- Phase 1: idle. ----
    // Both daemons are freshly started and have never been sampled, so the
    // only IPC client either can report is the rnstatus run doing the
    // sampling.
    let idle_l = templated_inventory(&lnsd).await;
    let idle_r = templated_inventory(&rnsd).await;
    assert_inventory_parity(&idle_l, &idle_r, "idle, no peer connected");

    // The reference inventory of this config, spelled out so the test pins
    // the CONTENT and not merely the agreement of two possibly-empty lists.
    let expected_idle = vec![
        InventoryRow {
            type_name: "LocalClientInterface".into(),
            name: "LocalInterface[rns/<INSTANCE>]".into(),
            short_name: "<N>@\0rns/<INSTANCE>".into(),
            parent_name: Some("Shared Instance[rns/<INSTANCE>]".into()),
            clients: None,
        },
        InventoryRow {
            type_name: "LocalServerInterface".into(),
            name: "Shared Instance[rns/<INSTANCE>]".into(),
            short_name: "Reticulum".into(),
            parent_name: None,
            clients: Some(1),
        },
        InventoryRow {
            type_name: "TCPServerInterface".into(),
            name: "TCPServerInterface[Parity TCP Server 0/127.0.0.1:<LISTEN_PORT>]".into(),
            short_name: "Parity TCP Server 0".into(),
            parent_name: None,
            clients: Some(0),
        },
    ];
    assert_eq!(
        idle_r,
        expected_idle,
        "\nthe reference inventory itself changed:\n{}\n",
        render_inventory(&idle_r)
    );
    assert_eq!(
        idle_l,
        expected_idle,
        "\nlnsd idle inventory:\n{}\n",
        render_inventory(&idle_l)
    );

    // ---- Phase 2: one connected TCP peer. ----
    // Wait out the phase-1 sampling client on both sides so the observer
    // footprint is identical again, then connect one peer per daemon and
    // announce one identity into each; the learned path is the observable
    // state that says the connection is a registered interface on both.
    wait_observer_gone(&lnsd).await;
    wait_observer_gone(&rnsd).await;

    let mut injectors = Injectors::connect(&lnsd, &rnsd).await;
    let identity = Identity::generate(&mut OsRng);
    let (frame, dest) = build_announce_for(&identity, "inventory");
    injectors.send_both(&frame).await;
    let dest_hex = hex::encode(dest.as_bytes());
    for daemon in [&lnsd, &rnsd] {
        wait_paths(
            daemon,
            "peer announce processed (connection is a registered interface)",
            Duration::from_secs(15),
            |p| p.contains(&dest_hex),
        )
        .await;
    }

    let peer_l = templated_inventory(&lnsd).await;
    let peer_r = templated_inventory(&rnsd).await;
    assert_inventory_parity(&peer_l, &peer_r, "one connected TCP peer");

    let spawned = InventoryRow {
        type_name: "TCPClientInterface".into(),
        name: "TCPInterface[Client on Parity TCP Server 0/127.0.0.1:<EPHEMERAL>]".into(),
        short_name: "Client on Parity TCP Server 0".into(),
        parent_name: Some("TCPServerInterface[Parity TCP Server 0/127.0.0.1:<LISTEN_PORT>]".into()),
        clients: None,
    };
    assert!(
        peer_r.contains(&spawned),
        "\nthe reference naming of a spawned connection changed:\n{}\n",
        render_inventory(&peer_r)
    );
    assert!(
        peer_l.contains(&spawned),
        "\nlnsd must name a spawned connection like the reference:\n{}\n",
        render_inventory(&peer_l)
    );
    // The listener now reports its one client on both stacks.
    for (label, rows) in [("lnsd", &peer_l), ("rnsd", &peer_r)] {
        let listener = rows
            .iter()
            .find(|r| r.type_name == "TCPServerInterface")
            .unwrap_or_else(|| {
                panic!("{label} lists no TCP listener:\n{}", render_inventory(rows))
            });
        assert_eq!(
            listener.clients,
            Some(1),
            "{label}: the listener must count its spawned connection"
        );
    }

    drop(injectors);
    let (l_dir, r_dir) = (lnsd.config_dir.clone(), rnsd.config_dir.clone());
    drop(lnsd);
    drop(rnsd);
    let _ = std::fs::remove_dir_all(l_dir);
    let _ = std::fs::remove_dir_all(r_dir);
}

async fn connect_injector(daemon: &ParityDaemon) -> TcpStream {
    connect_injector_port(daemon, daemon.tcp_port).await
}

async fn connect_injector_port(daemon: &ParityDaemon, port: u16) -> TcpStream {
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(&addr).await {
            Ok(s) => return s,
            Err(e) => assert!(
                Instant::now() < deadline,
                "cannot connect injector to {}: {e}",
                daemon.stack.label()
            ),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// =========================================================================
// Multi-interface sort / -a parity (complements the single-interface 2x2)
// =========================================================================

/// The spawned per-connection entries (what `-a` reveals and what the sort
/// flags order): non-local, and not a listener.
fn visible_ifaces(stats: &Value) -> Vec<Value> {
    ifaces(stats)
        .into_iter()
        .filter(|i| {
            let t = i.get("type").and_then(|t| t.as_str()).unwrap_or("");
            t != "LocalServerInterface" && t != "LocalClientInterface" && !is_listener(i)
        })
        .collect()
}

/// Drop the volatile uptime value so two sequential reads of the same frozen
/// daemon are byte-comparable. Every other line is stable in the idle state
/// (speeds render `0 bps`, byte counters are pinned by the readiness poll),
/// so the uptime trailer is the only thing that can differ between the two
/// clients' invocations.
fn normalize_idle_text(text: &str) -> String {
    let re_uptime = Regex::new(r"Uptime is .*").unwrap();
    text.lines()
        .map(|l| {
            if l.contains("Uptime is") {
                re_uptime.replace(l, "Uptime is <T>").into_owned()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// lnstatus vs rnstatus (the drop-in A/B) against ONE lnsd carrying TWO
/// traffic interfaces with unequal received-byte counts. This pins the parity
/// surface the single-interface 2x2 cannot exercise: the default multi-entry
/// ordering, every `-s <field>` sort direction, `-r` reverse, and the `-a`
/// spawned-interface block. Both clients read the SAME frozen daemon state, so
/// after blanking only the uptime line their output must be byte-identical for
/// a user or script switching between the tools.
///
/// Determinism: the two injectors send different byte volumes on one raw TCP
/// connection each, so the spawned interfaces have distinct rxb; the test then
/// polls interface_stats until exactly two visible interfaces exist with
/// unequal, non-zero rxb AND every speed has decayed to 0 (frozen), so sort
/// order is well-defined and no live rate can differ between the two reads.
#[tokio::test]
#[ignore = "spawns lnsd + python rnstatus; run via --include-ignored after the tier3"]
async fn lnstatus_rnstatus_multi_interface_sort_parity() {
    init_tracing();

    let lnsd = ParityDaemon::start_n(Stack::Lnsd, 2).await;

    // One raw connection per TCP server; send more bytes on the first so the
    // two spawned interfaces end with clearly unequal rxb (deterministic sort
    // order). The connections stay open for the whole test so the interface
    // set never changes underneath the comparison.
    let mut inj_a = connect_injector_port(&lnsd, lnsd.tcp_ports[0]).await;
    let mut inj_b = connect_injector_port(&lnsd, lnsd.tcp_ports[1]).await;
    use tokio::io::AsyncWriteExt as _;
    // HDLC flag-delimited noise; the exact contents are irrelevant, only that
    // interface A receives strictly more on-wire bytes than interface B.
    inj_a
        .write_all(&[0x7e, 0x00, 0x11, 0x22, 0x33, 0x7e])
        .await
        .expect("write inj_a");
    inj_b.write_all(&[0x7e, 0x7e]).await.expect("write inj_b");
    inj_a.flush().await.expect("flush a");
    inj_b.flush().await.expect("flush b");

    // Readiness + freeze: exactly two visible interfaces, unequal non-zero
    // rxb, and every speed settled at 0 so nothing live can differ between the
    // two sequential client reads.
    //
    // `wait_frozen`, not a one-shot predicate: the noise above is the only
    // traffic in this test, so at the first poll it has been received (rxb is
    // already 6 and 2) but not yet sampled, and every speed reads 0 for the
    // pre-sample reason rather than the idle one. Accepting that snapshot as
    // the freeze put the single tick that turns those speeds positive for one
    // second inside the comparison window, and one run in five rendered
    // `0 bps` for rnstatus and `64 bps` for lnstatus off the same daemon
    // (Codeberg #195). Waiting the sampling period out is what makes the two
    // reads reads of the same state.
    let frozen = wait_frozen(
        &lnsd,
        "two settled interfaces with unequal rxb",
        Duration::from_secs(30),
        |stats| {
            let vis = visible_ifaces(stats);
            if vis.len() != 2 {
                return false;
            }
            let (r0, r1) = (num(&vis[0], "rxb"), num(&vis[1], "rxb"));
            r0 > 0.0 && r1 > 0.0 && r0 != r1
        },
    )
    .await;
    let before = render_inputs(&frozen);

    // The A/B matrix: same daemon, two clients, byte-identical output after
    // blanking only the uptime. `-a` is included on every set because the
    // spawned TCP connections are hidden by default (the `TCPInterface[Client`
    // name prefix) and the sort/order surface is only visible once shown.
    let flag_sets: &[&[&str]] = &[
        &["-a"],
        &["-a", "-s", "rx"],
        &["-a", "-s", "rx", "-r"],
        &["-a", "-s", "tx"],
        &["-a", "-s", "traffic"],
        &["-a", "-s", "rate"],
        &["-a", "-s", "traffic", "-r"],
        &["-a", "-t"],
        &["-a", "-l"],
        &["-a", "-A", "-P"],
        &["-a", "tcp"],
    ];
    for flags in flag_sets {
        let rn = run_status(StatusClient::Rnstatus, &lnsd, flags).await;
        wait_observer_gone(&lnsd).await;
        let ln = run_status(StatusClient::Lnstatus, &lnsd, flags).await;
        // The name filter selects by interface name, so it removes the
        // observer's own `LocalInterface[...]` row before the render does.
        let observer_rows = usize::from(!flags.contains(&"tcp"));
        assert_eq!(
            normalize_idle_text(&strip_observer_footprint(&rn, observer_rows, "rnstatus")),
            normalize_idle_text(&strip_observer_footprint(&ln, 0, "lnstatus")),
            "multi-interface render parity for flags {flags:?} on lnsd\nrnstatus:\n{rn}\nlnstatus:\n{ln}"
        );
    }

    // What the render parity above is a statement about: the daemon stood
    // still for all of it. Asserted rather than assumed, and named field by
    // field, so a future move reports itself as the field that moved instead
    // of as a diff between two render blobs (Codeberg #195).
    assert_eq!(
        before,
        render_inputs(&lnsd.stats().await),
        "lnsd interface state moved during the client comparison; the two \
         sequential client reads were not reads of one frozen daemon"
    );

    // The -j top-level key set must match structurally as well (the JSON both
    // clients read is the same daemon dict; key order/separators are the only
    // allowed difference, per the module doc).
    let rn_j: Value = serde_json::from_str(
        run_status(StatusClient::Rnstatus, &lnsd, &["-j"])
            .await
            .trim(),
    )
    .expect("rnstatus -j JSON");
    let ln_j: Value = serde_json::from_str(
        run_status(StatusClient::Lnstatus, &lnsd, &["-j"])
            .await
            .trim(),
    )
    .expect("lnstatus -j JSON");
    let top_keys = |v: &Value| -> BTreeSet<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    assert_eq!(
        top_keys(&rn_j),
        top_keys(&ln_j),
        "multi-interface -j top-level key sets must match across clients"
    );

    drop(inj_a);
    drop(inj_b);
    let dir = lnsd.config_dir.clone();
    drop(lnsd);
    let _ = std::fs::remove_dir_all(dir);
}

/// Field-by-field daemon stats parity (#67) on the frozen state, rnstatus
/// being the shared measuring instrument. Cross-daemon identity fields
/// (name/short_name/hash/type) encode per-daemon naming and are asserted
/// per daemon instead; the known structural divergences are pinned exactly
/// so any drift fails the test.
fn assert_daemon_stats_parity(on_lnsd: &Value, on_rnsd: &Value) {
    // Top-level key sets: identical up to the pinned lnsd-only additions.
    // The post-1.3.5 upstream totals/queue keys (announce/path-request
    // traffic, pps, inbound-queue pressure, txq) exist in current upstream
    // but not in the 1.3.5 vendor rnsd sampled here; we emit them for
    // current-rnstatus drop-in compatibility (its --pps/--queues paths
    // index them unguarded). Pinned exactly so a vendor bump or an
    // accidental key drop fails loudly.
    let keys = |v: &Value| -> BTreeSet<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    let top_ours_only: BTreeSet<String> = [
        "arxb",
        "atxb",
        "arxs",
        "atxs",
        "arxf",
        "atxf",
        "prxb",
        "ptxb",
        "prxs",
        "ptxs",
        "prxf",
        "ptxf",
        "rxpps",
        "txpps",
        "rxqt",
        "rxqd",
        "rxqa",
        "rxqp",
        "rxqil",
        "rxqtd",
        "rxqdd",
        "rxqad",
        "rxqpd",
        "rxqild",
        "tqpressure",
        "dqpressure",
        "aqpressure",
        "pqpressure",
        "ilqpressure",
        "txq",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        keys(on_lnsd)
            .difference(&keys(on_rnsd))
            .cloned()
            .collect::<BTreeSet<String>>(),
        top_ours_only,
        "unexpected lnsd-only top-level interface_stats keys"
    );
    assert_eq!(
        keys(on_rnsd)
            .difference(&keys(on_lnsd))
            .cloned()
            .collect::<BTreeSet<String>>(),
        BTreeSet::new(),
        "unexpected rnsd-only top-level interface_stats keys"
    );

    // Pinned interface-list shapes (see module doc). rnstatus is connected
    // while it samples, so on rnsd its own LocalClientInterface entry is
    // part of the expected shape.
    let type_counts = |v: &Value| -> Vec<String> {
        let mut t: Vec<String> = ifaces(v)
            .iter()
            .map(|i| i["type"].as_str().unwrap_or("?").to_string())
            .collect();
        t.sort();
        t
    };
    let expected_shape = vec![
        "LocalClientInterface",
        "LocalServerInterface",
        "TCPClientInterface",
        "TCPServerInterface",
    ];
    for (label, stats) in [("lnsd", on_lnsd), ("rnsd", on_rnsd)] {
        assert_eq!(
            type_counts(stats),
            expected_shape,
            "{label} must expose shared instance + listener + spawned connection \
             + the sampling client (Codeberg #177)"
        );
    }

    let (tl, tr) = (traffic_iface(on_lnsd), traffic_iface(on_rnsd));

    // Pinned per-interface key-set divergence. `announce_queue` is always
    // reported by lnsd but only LAZILY by Python: the key appears once the
    // interface's announce-cap queue has ever held an entry, which depends
    // on rebroadcast timing under the burst load. Both shapes are pinned.
    //
    // `tx_queue_drops` (Codeberg #318) is an additive lnsd-only key with no
    // reference equivalent, emitted unconditionally next to rxb/txb. The
    // reference reader tolerates it: rnstatus looks every field up by name
    // (text path gated with `"key" in ifstat`) and its --json path enumerates
    // keys only to hex-encode bytes values, so an unknown int key passes
    // through unread. Same shape as the in-repo precedent `tx_jitter_max`
    // (#190) — see the note above `row_fields` in rpc/handlers.rs; that one
    // stays out of this set only because it is emitted per medium and the
    // TCP interface this test samples has none.
    //
    // Operational lesson: any batch that adds an interface_stats key MUST run
    // the status-parity step (`bash scripts/run-status-parity.sh`, the recipe
    // in `just standard`). This suite is `#[ignore]`d, so the normal test
    // lanes — `cargo test --workspace` included — skip it, and the key-set
    // drift only surfaces here.
    let (kl, kr) = (keys(&tl), keys(&tr));
    let ours_only: BTreeSet<String> = kl.difference(&kr).cloned().collect();
    let python_only: BTreeSet<String> = kr.difference(&kl).cloned().collect();
    // The post-1.3.5 upstream per-interface keys (see the note above
    // `row_fields` in rpc/handlers.rs): current rnstatus indexes txdrp/mtu/
    // txbuffered unguarded on its default path and every one of these under
    // some --sort or -a/-p flag, while the 1.3.5 vendor rnsd here predates
    // them all.
    let upstream_additions = [
        "mtu",
        "txdrp",
        "txdrb",
        "txstalled",
        "txbuffered",
        "arxb",
        "atxb",
        "arxc",
        "atxc",
        "prxb",
        "ptxb",
        "prxc",
        "ptxc",
        "arxs",
        "atxs",
        "prxs",
        "ptxs",
        "burst_count",
        "pr_burst_count",
        "gravity",
        "announces_to_internal",
        "protocol_violations",
        "ifac_violations",
        "packet_filter_hits",
    ];
    let ours_only_full: BTreeSet<String> = ["announce_queue", "peers", "tx_queue_drops"]
        .iter()
        .chain(upstream_additions.iter())
        .map(|s| s.to_string())
        .collect();
    let ours_only_queued: BTreeSet<String> = ["peers", "tx_queue_drops"]
        .iter()
        .chain(upstream_additions.iter())
        .map(|s| s.to_string())
        .collect();
    assert!(
        ours_only == ours_only_full || ours_only == ours_only_queued,
        "unexpected lnsd-only interface_stats keys: {ours_only:?}"
    );
    // `parent_interface_name`/`parent_interface_hash` are emitted by BOTH
    // stacks since Codeberg #177 (the spawned connection points at its
    // listener), and `autoconnect_source` moved to both sides with the
    // upstream key-set batch (its cross-daemon equality is asserted below),
    // so nothing is left on the Python side.
    assert_eq!(
        python_only,
        BTreeSet::new(),
        "unexpected rnsd-only interface_stats keys"
    );
    for (label, t) in [("lnsd", &tl), ("rnsd", &tr)] {
        let parent = t["parent_interface_name"].as_str().unwrap_or_default();
        assert!(
            parent.starts_with("TCPServerInterface[Parity TCP Server 0/"),
            "{label}: the spawned connection must point at its listener, got {parent:?}"
        );
        assert_eq!(
            t["parent_interface_hash"].as_str().map(|h| h.len()),
            Some(64),
            "{label}: parent_interface_hash must be the listener's full 32-byte hash"
        );
    }

    // Exact-equal stats on the traffic interface. All of these are frozen
    // (freeze predicate) or static config, so numeric equality is exact.
    for key in [
        "status",
        "mode",
        "bitrate",
        "clients",
        "ifac_signature",
        "ifac_size",
        "ifac_netname",
        "announce_rate_target",
        "announce_rate_penalty",
        "announce_rate_grace",
        "held_announces",
        "burst_active",
        "pr_burst_active",
        "pr_burst_activated",
        "incoming_announce_frequency",
        "outgoing_announce_frequency",
        "incoming_pr_frequency",
        "outgoing_pr_frequency",
        "rxs",
        "txs",
        "autoconnect_source",
    ] {
        let (a, b) = (canon(&tl[key]), canon(&tr[key]));
        assert_eq!(
            a, b,
            "daemon parity: traffic interface field {key} differs (lnsd vs rnsd)"
        );
    }

    // Both stacks record that an announce burst was activated at some point
    // of the scenario. The raw activation timestamps use different epochs,
    // so only the "it happened" fact is comparable.
    assert!(
        num(&tl, "burst_activated") != 0.0 && num(&tr, "burst_activated") != 0.0,
        "both daemons must remember the burst activation (lnsd={:?}, rnsd={:?})",
        tl.get("burst_activated"),
        tr.get("burst_activated")
    );

    // Frozen totals: speeds are exactly zero on both.
    for key in ["rxs", "txs"] {
        assert_eq!(
            num(on_lnsd, key),
            0.0,
            "lnsd total {key} must be frozen to 0"
        );
        assert_eq!(
            num(on_rnsd, key),
            0.0,
            "rnsd total {key} must be frozen to 0"
        );
    }

    // Transport identity fields: per-daemon values, shared shape.
    for (label, v) in [("lnsd", on_lnsd), ("rnsd", on_rnsd)] {
        let tid = v["transport_id"].as_str().unwrap_or_default();
        assert_eq!(tid.len(), 32, "{label}: transport_id must be 16 bytes hex");
        assert!(
            v["network_id"].is_null(),
            "{label}: network_id must be null"
        );
        assert!(
            v["probe_responder"].is_null(),
            "{label}: probe responder is off"
        );
        let up = num(v, "transport_uptime");
        assert!(up > 0.0 && up < 3600.0, "{label}: implausible uptime {up}");
    }
}

/// Concrete value-range assertions for the full Rust stack (lnstatus reading
/// lnsd), the "our stack is sane" cell of the matrix. Every range is
/// defined, not "looks sane".
fn assert_full_stack_sane(stats: &Value) {
    // Shared instance + TCP listener + the spawned traffic connection.
    // lnstatus samples over RPC only, so no client row of its own
    // (Codeberg #177).
    let ifs = ifaces(stats);
    let names: Vec<&str> = ifs
        .iter()
        .map(|i| i["name"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        ifs.len(),
        3,
        "shared instance + listener + traffic connection expected, got {names:?}"
    );
    let t = traffic_iface(stats);
    let t = &t;
    assert_eq!(t["type"], "TCPClientInterface");
    assert!(
        t["name"]
            .as_str()
            .unwrap_or_default()
            .starts_with("TCPInterface[Client on Parity TCP Server 0/"),
        "spawned connection must carry the reference name, got {:?}",
        t["name"]
    );
    assert_eq!(t["status"], Value::Bool(true), "interface must be up");
    assert_eq!(num(t, "mode"), 1.0, "MODE_FULL");
    assert_eq!(num(t, "bitrate"), 10_000_000.0, "TCP bitrate guess");
    assert_eq!(
        t["hash"].as_str().map(|h| h.len()),
        Some(64),
        "interface hash must be the full 32-byte Identity.full_hash, hex"
    );
    assert!(num(t, "rxb") > 0.0, "traffic was received");
    assert!(num(t, "txb") > 0.0, "path responses were sent");
    assert_eq!(num(t, "announce_rate_target"), 3600.0);
    assert_eq!(num(t, "announce_rate_penalty"), 0.0);
    assert_eq!(num(t, "announce_rate_grace"), 5.0);
    assert_eq!(num(t, "held_announces"), 0.0, "drained after the burst");
    assert_eq!(t["burst_active"], Value::Bool(false));
    for key in [
        "incoming_announce_frequency",
        "outgoing_announce_frequency",
        "incoming_pr_frequency",
        "outgoing_pr_frequency",
    ] {
        assert_eq!(num(t, key), 0.0, "{key} frozen to exactly 0");
    }
    assert_eq!(
        stats["transport_id"].as_str().map(|h| h.len()),
        Some(32),
        "transport_id must be 16 bytes hex"
    );
    let up = num(stats, "transport_uptime");
    assert!(up > 0.0 && up < 3600.0, "implausible uptime {up}");
    // Totals count the traffic-bearing interface once: the listener rows
    // aggregate the same bytes for display and must not double-count them.
    assert_eq!(
        num(stats, "rxb"),
        num(t, "rxb"),
        "totals equal the single traffic interface"
    );
    assert_eq!(
        num(stats, "txb"),
        num(t, "txb"),
        "totals equal the single traffic interface"
    );
    // The listener reports exactly what its one child carried (Python keeps
    // the parent counters in step with the child's, TCPInterface.py:306-329).
    let listener = ifaces(stats)
        .into_iter()
        .find(|i| i["type"] == "TCPServerInterface")
        .expect("listener row");
    assert_eq!(num(&listener, "rxb"), num(t, "rxb"));
    assert_eq!(num(&listener, "txb"), num(t, "txb"));
    assert_eq!(num(&listener, "clients"), 1.0, "one connected peer");
}
