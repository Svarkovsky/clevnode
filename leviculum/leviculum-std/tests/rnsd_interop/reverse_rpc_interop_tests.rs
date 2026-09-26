//! Reverse-direction RPC interop: OUR client drives a Python `rnsd`.
//!
//! The always-on `rpc_interop_tests` run Python CLI tools (`rnstatus`,
//! `rnpath`) against OUR `lnsd` shared-instance RPC. This module proves the
//! other direction on every `cargo test` run: boot the vendored Python `rnsd`
//! and drive OUR `rpc_query` client against its shared-instance RPC socket,
//! asserting a status query returns valid data. That exercises our RPC client's
//! HMAC handshake and msgpack decode against a real Python server, which the
//! `#[ignore]` parity suite otherwise covers only on demand.
//!
//! Skips gracefully when `python3` + the vendored `RNS` package is unavailable;
//! runs in CI where both are present (same assumption as `rpc_interop_tests`).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use leviculum_core::Identity;
use leviculum_std::process::spawn_supervised;
use rand_core::OsRng;
use sha2::Digest;

use crate::harness::find_available_ports;

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

const RNSD_PY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../reference/Reticulum/RNS/Utilities/rnsd.py"
);
const VENDOR_RNS_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../reference/Reticulum");

/// True when `python3` can import the vendored `RNS` package. Mirrors the
/// python assumption `rpc_interop_tests` makes, but lets us skip cleanly
/// (test passes) where the interpreter or package is absent.
fn python_rns_available() -> bool {
    Command::new("python3")
        .args(["-c", "import RNS"])
        .env("PYTHONPATH", VENDOR_RNS_ROOT)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A spawned Python `rnsd` subprocess, killed on drop.
struct Rnsd {
    config_dir: PathBuf,
    instance_name: String,
    authkey: [u8; 32],
    child: Child,
}

impl Drop for Rnsd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.config_dir);
    }
}

/// Boot a vendored Python `rnsd` as a transport node with a shared instance and
/// one TCP server, pre-seeding the transport identity so the RPC authkey is
/// known before startup.
fn spawn_rnsd(tcp_port: u16) -> Rnsd {
    let test_id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let instance_name = format!("revrpc_{}_{}", std::process::id(), test_id);

    let config_dir = std::env::temp_dir().join(format!("reverse_rpc_{instance_name}"));
    let _ = std::fs::remove_dir_all(&config_dir);
    std::fs::create_dir_all(config_dir.join("storage")).expect("create config dir");

    let identity = Identity::generate(&mut OsRng);
    let identity_bytes = identity.private_key_bytes().expect("private key bytes");
    std::fs::write(
        config_dir.join("storage").join("transport_identity"),
        identity_bytes,
    )
    .expect("write transport_identity");
    let mut authkey = [0u8; 32];
    authkey.copy_from_slice(&sha2::Sha256::digest(identity_bytes));

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
         \x20 [[Reverse RPC TCP Server]]\n\
         \x20   type = TCPServerInterface\n\
         \x20   enabled = yes\n\
         \x20   listen_ip = 127.0.0.1\n\
         \x20   listen_port = {tcp_port}\n"
    );
    std::fs::write(config_dir.join("config"), config).expect("write config");

    let log = std::fs::File::create(config_dir.join("daemon.log")).expect("create log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut cmd = Command::new("python3");
    cmd.arg(RNSD_PY)
        .arg("--config")
        .arg(&config_dir)
        .env("PYTHONPATH", VENDOR_RNS_ROOT)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    let child = spawn_supervised(cmd).expect("spawn rnsd");

    Rnsd {
        config_dir,
        instance_name,
        authkey,
        child,
    }
}

/// Reverse interop: our `rpc_query` client drives the Python `rnsd`
/// shared-instance RPC socket and gets a valid `interface_stats` response
/// (transport uptime + interfaces present) on the first successful query.
#[tokio::test]
async fn test_lns_rpc_query_against_python_rnsd() {
    if !python_rns_available() {
        eprintln!("skipping reverse RPC interop: python3 + vendored RNS unavailable");
        return;
    }

    // The harness allocator hands out a minimum of two ports; we use only the
    // first for the single TCP server interface.
    let (ports, _alloc) = find_available_ports::<2>().await.expect("allocate port");
    let rnsd = spawn_rnsd(ports[0]);

    // Readiness: the RPC socket answering interface_stats is the real
    // "daemon up" condition. Poll our own client until it succeeds.
    let deadline = Instant::now() + Duration::from_secs(30);
    let stats = loop {
        match leviculum_std::rpc_query(&rnsd.instance_name, &rnsd.authkey, "interface_stats").await
        {
            Ok(v) if v.get("transport_uptime").is_some() => break v,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "python rnsd RPC not ready within 30 s (log: {})",
            rnsd.config_dir.join("daemon.log").display()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };

    // A shared-instance transport node reports a non-negative uptime and an
    // interfaces list. Both come straight through our RPC client's HMAC
    // handshake + msgpack decode from the Python server.
    let uptime = stats
        .get("transport_uptime")
        .and_then(|u| u.as_f64())
        .expect("transport_uptime must be a float");
    assert!(uptime >= 0.0, "transport uptime should be non-negative");
    assert!(
        stats.get("interfaces").and_then(|i| i.as_array()).is_some(),
        "interface_stats must carry an interfaces array, got: {stats}"
    );

    // Codeberg #174, the absence half of `lnstatus --tables`. `transport_tables`
    // is a Leviculum-only command; a Python `rnsd` matches no arm for it and
    // falls straight through to `conn.close()` (Reticulum.py:1213-1260). This
    // pins what that looks like from our side, because the design of
    // `merge_transport_tables` rests on it: an error, bounded in time, not a
    // hang and not a plausible-looking empty answer that would be
    // indistinguishable from "the tables really are empty".
    let started = Instant::now();
    let unsupported =
        leviculum_std::rpc_query(&rnsd.instance_name, &rnsd.authkey, "transport_tables").await;
    let waited = started.elapsed();
    assert!(
        unsupported.is_err(),
        "a Python rnsd must not answer transport_tables; got {unsupported:?}"
    );
    assert!(
        waited < Duration::from_secs(10),
        "an unsupported command must fail fast, not stall the client; took {waited:?}"
    );

    // And the daemon is still usable afterwards: the unanswered command closed
    // one connection, not the RPC listener. A client that degrades to "no
    // tables" must still get its status.
    let after = leviculum_std::rpc_query(&rnsd.instance_name, &rnsd.authkey, "interface_stats")
        .await
        .expect("rnsd still answers interface_stats after refusing transport_tables");
    assert!(after.get("interfaces").and_then(|i| i.as_array()).is_some());
}
