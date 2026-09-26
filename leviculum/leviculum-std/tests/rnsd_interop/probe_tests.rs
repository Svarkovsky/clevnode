//! Tests for rnprobe hop count correctness.
//!
//! When lnsd forwards a cached announce to a shared instance local client
//! (via path request response), it should include the receipt-incremented
//! hop count, not the raw wire hops from the cache.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use leviculum_core::Identity;
use leviculum_std::config::Config;
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{init_tracing, parse_dest_hash, wait_for_path_on_node};
use crate::harness::{find_available_ports, TestDaemon};

/// Unique counter to avoid collisions between parallel tests.
static PROBE_TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Path to vendor rnprobe script.
const RNPROBE_PY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../reference/Reticulum/RNS/Utilities/rnprobe.py"
);

/// Path to vendor Reticulum package (for PYTHONPATH).
const VENDOR_RNS_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../reference/Reticulum");

/// Create a temp config directory with a Python-compatible config file and
/// transport_identity file so that Python tools derive the same RPC auth key.
fn create_probe_config_dir(instance_name: &str, identity_bytes: &[u8; 64]) -> PathBuf {
    let tempdir = std::env::temp_dir().join(format!("probe_test_{}", instance_name));

    // Clean up from any previous run
    let _ = std::fs::remove_dir_all(&tempdir);

    // Create directory structure
    let storage_dir = tempdir.join("storage");
    std::fs::create_dir_all(&storage_dir).expect("create storage dir");

    // Write the transport_identity file (64 bytes: X25519 prv + Ed25519 prv)
    // Must match exactly what the Rust daemon uses, so Python derives the same
    // RPC auth key = SHA-256(private_key_bytes).
    std::fs::write(storage_dir.join("transport_identity"), identity_bytes)
        .expect("write transport_identity");

    // Write minimal Python Reticulum config.
    // share_instance = Yes makes Python connect as a client to the existing daemon.
    let config_content = format!(
        "[reticulum]\n\
         \x20 enable_transport = no\n\
         \x20 share_instance = Yes\n\
         \x20 instance_name = {instance_name}\n\
         \n\
         [logging]\n\
         \x20 loglevel = 4\n\
         \n\
         [interfaces]\n"
    );
    std::fs::write(tempdir.join("config"), config_content).expect("write config");

    tempdir
}

use crate::common::cleanup_config_dir;

/// Test that rnprobe reports correct hops for a direct neighbor destination.
///
/// Topology:
/// ```text
/// Python daemon (node B)         ── TCP ──  Rust lnsd (node A, shared instance)
/// (respond_to_probes, announces)             (transport, TCP server, share_instance)
///                                                    |
///                                            Unix socket (shared instance)
///                                                    |
///                                            rnprobe (shared instance client)
/// ```
///
/// The bug: lnsd forwards the raw cached announce bytes to the local client,
/// which contain wire hops=0 (pre-increment). The local client's process_incoming
/// applies +1 then -1 (net zero), so rnprobe sees hops=0 instead of hops=1.
#[tokio::test]
async fn test_rnprobe_reports_correct_hops() {
    init_tracing();

    // Phase 1: Setup
    let (ports, _port_alloc) = find_available_ports::<2>()
        .await
        .expect("Failed to allocate ports");
    let daemon_tcp_port = ports[0];
    let test_id = PROBE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let instance_name = format!("probetest_{}_{}", std::process::id(), test_id);
    let daemon_tcp_addr: SocketAddr = format!("127.0.0.1:{}", daemon_tcp_port).parse().unwrap();

    // Generate identity up front so we can export it for rnprobe's config dir.
    let identity = Identity::generate(&mut rand_core::OsRng);
    let identity_bytes = identity
        .private_key_bytes()
        .expect("generated identity must have private keys");

    // Phase 2: Start Rust lnsd (node A)
    let mut config = Config::default();
    config.reticulum.respond_to_probes = true;
    let _storage = crate::common::temp_storage("test_rnprobe_reports_correct_hops", "node");
    let mut daemon_node = ReticulumNodeBuilder::new()
        .identity(identity)
        .config(config)
        .enable_transport(true)
        .share_instance(true)
        .instance_name(instance_name.clone())
        .add_tcp_server(daemon_tcp_addr)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust daemon node");
    daemon_node
        .start()
        .await
        .expect("Failed to start Rust daemon node");

    // Wait for Unix socket listener
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 3: Start Python daemon (node B) with --respond-to-probes
    let py_daemon = TestDaemon::start_with_probes()
        .await
        .expect("Failed to start Python daemon with probes");

    let py_probe_hex = py_daemon
        .probe_dest_hash()
        .expect("Python daemon should have printed PROBE_DEST:<hex>")
        .to_string();

    // Connect Python to Rust daemon's TCP server
    py_daemon
        .add_client_interface("127.0.0.1", daemon_tcp_port, Some("ToRustDaemon"))
        .await
        .expect("Failed to connect Python to Rust daemon");

    // Phase 4: Wait for Python's probe announce to propagate
    let dest_hash = parse_dest_hash(&py_probe_hex);

    assert!(
        wait_for_path_on_node(&daemon_node, &dest_hash, Duration::from_secs(25)).await,
        "Rust daemon should learn path to Python's probe destination"
    );

    // Verify Rust daemon sees hops=1 for this direct neighbor
    let daemon_hops = daemon_node.hops_to(&dest_hash);
    assert_eq!(
        daemon_hops,
        Some(1),
        "Rust daemon's path table should show hops=1 for direct neighbor probe destination"
    );

    // Phase 5: Write config dir for rnprobe with correct identity
    let config_dir = create_probe_config_dir(&instance_name, &identity_bytes);

    // Phase 6: Run rnprobe
    let config_str = config_dir.to_str().expect("config dir must be valid UTF-8");
    let output = tokio::process::Command::new("python3")
        .arg(RNPROBE_PY)
        .arg("--config")
        .arg(config_str)
        .args(["-n", "1", "-t", "15"])
        .args(["rnstransport.probe", &py_probe_hex])
        .env("PYTHONPATH", VENDOR_RNS_ROOT)
        .output()
        .await
        .expect("Failed to run rnprobe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("rnprobe stdout: {}", stdout);
    eprintln!("rnprobe stderr: {}", stderr);
    eprintln!("rnprobe exit status: {}", output.status);

    // Phase 7: Parse and verify hop count
    let re = regex::Regex::new(r"over (\d+) hop").unwrap();
    let caps = re.captures(&stdout).unwrap_or_else(|| {
        panic!(
            "rnprobe should report hops in output. stdout: '{}', stderr: '{}'",
            stdout, stderr
        )
    });
    let hops: u8 = caps[1].parse().unwrap();
    assert_eq!(
        hops, 1,
        "Direct neighbor should be 1 hop, got {}. \
         This fails because the cached announce forwarded to the shared instance \
         client contains raw wire hops=0 instead of receipt-incremented hops=1.",
        hops
    );

    cleanup_config_dir(&config_dir);
}
