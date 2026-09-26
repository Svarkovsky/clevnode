//! Python `rnstatus -R` against OUR `lnsd` server (Codeberg #86 Stage 3c).
//!
//! Stage 3a proved our `lnstatus -R` CLIENT against a Python `rnsd`
//! (`remote_management_tests.rs`). Stage 3b proved our stack as the remote-
//! management SERVER, driven by our OWN Rust client (`fetch_remote_status`,
//! `remote_management_server_tests.rs`) — a rust<->rust exercise that left one
//! gap open: it never drove the reference Python status tool against our
//! daemon.
//!
//! This file closes that gap — the P1 compatibility direction. The real
//! vendored `rnstatus -R <our-transport-hash> -i <identity> -j` runs as a
//! subprocess and queries our lnsd's `rnstransport.remote.management`
//! destination over a shared TCP segment:
//!
//! 1. rnstatus builds a standalone Reticulum from a config dir whose only
//!    interface is a `TCPClientInterface` onto our server's TCP listener,
//! 2. requests a path to the management destination (derived from our server's
//!    transport identity hash) and establishes a link,
//! 3. `identify`s with the management identity (allow-list authentication),
//! 4. issues the `/status` request and decodes the returned interface-stats
//!    bundle, which it prints as JSON on stdout under `-j`.
//!
//! The management identity is generated on the Rust side exactly as the
//! sibling server test does; its 64 raw private-key bytes are byte-identical to
//! Python's identity file format, so the same bytes produce the same identity
//! hash on both sides and the allow-list wiring is unchanged.
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop python_rnstatus_server
//! ```

use std::time::Duration;

use rand_core::OsRng;
use serde_json::Value;

use leviculum_core::identity::Identity;
use leviculum_std::config::Config;
use leviculum_std::driver::{ReticulumNode, ReticulumNodeBuilder};
use leviculum_std::remote_status::mgmt_destination_hash;

use crate::harness::{run_python_rnstatus_remote, RnstatusRemoteResult};

/// Lowercase hex without external deps.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build a transport-instance server config with remote management enabled and
/// `allowed` on the ACL.
fn server_config(allowed: Vec<String>) -> Config {
    let mut cfg = Config::default();
    cfg.reticulum.enable_transport = true;
    cfg.reticulum.remote_management_enabled = true;
    cfg.reticulum.remote_management_allowed = allowed;
    cfg
}

/// Drive the Python `rnstatus -R` subprocess while re-announcing the server's
/// management destination on a short cadence, then return its captured outcome.
///
/// `rnstatus` learns the path to `rnstransport.remote.management` from a SINGLE
/// one-shot `RNS.Transport.request_path` and then waits passively up to its
/// timeout with no re-request (`rnstatus.py` `get_remote_status`). Under a
/// CPU-starved `just standard` run that lone path round-trip can slip past the
/// window, and the server's own periodic management announce does not fire
/// until `MGMT_ANNOUNCE_INITIAL_DELAY_MS` (~15s) after startup, leaving a
/// fragile margin inside the 20s budget (Codeberg #110). A live Python peer
/// periodically re-announces its management destination; mirror that here by
/// re-driving the announce every couple of seconds for the lifetime of the
/// subprocess, so a dropped one-shot is retried rather than lost. This is the
/// proven #103/#105 re-drive, applied on the announcer (server) side because
/// the learner is an external subprocess whose `has_path` we cannot poll. It
/// does not touch the 20s timeout.
async fn run_rnstatus_with_reannounce(
    server: &ReticulumNode,
    server_addr: std::net::SocketAddr,
    server_id_hash: [u8; 16],
    mgmt_prv: Vec<u8>,
    include_lstats: bool,
    timeout_secs: u64,
) -> RnstatusRemoteResult {
    const REANNOUNCE_EVERY: Duration = Duration::from_secs(2);
    let mgmt_hash = mgmt_destination_hash(&server_id_hash);

    // Blocking subprocess, so run it off the reactor; the reactor stays free to
    // re-drive the announce while it runs.
    let mut task = tokio::task::spawn_blocking(move || {
        run_python_rnstatus_remote(
            server_addr,
            &server_id_hash,
            &mgmt_prv,
            include_lstats,
            timeout_secs,
        )
    });

    loop {
        tokio::select! {
            joined = &mut task => {
                return joined
                    .expect("join rnstatus task")
                    .expect("run rnstatus subprocess");
            }
            _ = tokio::time::sleep(REANNOUNCE_EVERY) => {
                let _ = server.announce_destination(&mgmt_hash, None).await;
            }
        }
    }
}

/// Happy path: the real Python `rnstatus -R` gets a populated status bundle
/// back from OUR server's `/status` handler, printed as JSON on stdout.
#[tokio::test]
#[allow(non_snake_case)] // name fixed by Codeberg #86 harness spec (rnstatus -R)
async fn test_python_rnstatus_R_against_our_lnsd() {
    // Management identity rnstatus identifies with; its hash goes on the
    // server's allow-list. Generated on the Rust side — the 64 raw private-key
    // bytes are the exact bytes Python's identity file holds.
    let mgmt_identity = Identity::generate(&mut OsRng);
    let allowed_hash = hex_lower(mgmt_identity.hash());
    let mgmt_prv = mgmt_identity
        .private_key_bytes()
        .expect("management identity must expose its private key bytes");

    // Server: a transport instance with a TCP server interface and remote
    // management enabled. `without_events()` exercises the daemon path — the
    // `/status` responder must run without an application event sink.
    let server_storage =
        crate::common::temp_storage("test_python_rnstatus_R_against_our_lnsd", "server");
    let mut server = ReticulumNodeBuilder::new()
        .config(server_config(vec![allowed_hash]))
        .add_tcp_server("127.0.0.1:0".parse().expect("loopback addr"))
        .storage_path(server_storage.path().to_path_buf())
        .without_events()
        .build()
        .await
        .expect("build server node");
    server.start().await.expect("start server node");
    let server_addr = *server
        .tcp_listen_addrs()
        .first()
        .expect("server reports its bound TCP listener (Codeberg #221)");
    let server_id_hash = server.identity_hash();
    // Let the TCP server interface bind and start listening.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Drive the real Python status CLI against our server, re-driving the
    // management-destination announce for the lifetime of the subprocess so the
    // one-shot path-learn is rescued under load (Codeberg #110/#103).
    let result = run_rnstatus_with_reannounce(
        &server,
        server_addr,
        server_id_hash,
        mgmt_prv.to_vec(),
        /* include_lstats = */ true,
        /* timeout_secs = */ 20,
    )
    .await;

    assert_eq!(
        result.code(),
        Some(0),
        "rnstatus must exit 0 on a successful remote status.\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );

    // Under `-j` a successful run prints the status bundle as JSON on stdout.
    // Find the JSON object line (RNS may prepend log lines).
    let json_line = result
        .stdout
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| {
            panic!(
                "rnstatus stdout must contain a JSON status bundle.\nstdout:\n{}\nstderr:\n{}",
                result.stdout, result.stderr
            )
        });
    let stats: Value = serde_json::from_str(json_line.trim())
        .unwrap_or_else(|e| panic!("rnstatus JSON must parse ({e}): {json_line}"));

    // The server runs a TCPServerInterface, so the bundle must list at least
    // one interface, each carrying the core keys the renderer reads.
    let interfaces = stats
        .get("interfaces")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("status bundle must contain an interfaces array: {stats}"));
    assert!(
        !interfaces.is_empty(),
        "remote status must report at least one interface, got: {stats}"
    );
    for ifstat in interfaces {
        assert!(
            ifstat.get("name").and_then(|v| v.as_str()).is_some(),
            "each interface must have a name: {ifstat}"
        );
        assert!(
            ifstat.get("status").is_some(),
            "each interface must have a status field: {ifstat}"
        );
    }
    // A transport instance reports its transport_id in the bundle. rnstatus
    // hex-encodes bytes fields (delimit=False) before JSON emission.
    assert_eq!(
        stats.get("transport_id").and_then(|v| v.as_str()),
        Some(hex_lower(&server_id_hash).as_str()),
        "transport_id must be the server's transport identity hash: {stats}"
    );

    server.stop().await.ok();
}

/// Allow-list enforcement: Python `rnstatus -R` with an identity NOT on the
/// server's allow-list is refused. Our core drops the `/status` request, so
/// rnstatus never receives a bundle and exits non-zero (its request-failed /
/// no-status path) rather than printing a populated status.
#[tokio::test]
#[allow(non_snake_case)] // name fixed by Codeberg #86 harness spec (rnstatus -R)
async fn test_python_rnstatus_R_rejected_when_not_allowed() {
    // The server allows some OTHER identity, not the one rnstatus uses.
    let allowed_other = Identity::generate(&mut OsRng);
    let allowed_hash = hex_lower(allowed_other.hash());
    let client_identity = Identity::generate(&mut OsRng);
    let client_prv = client_identity
        .private_key_bytes()
        .expect("client identity must expose its private key bytes");

    let server_storage =
        crate::common::temp_storage("test_python_rnstatus_R_rejected_when_not_allowed", "server");
    let mut server = ReticulumNodeBuilder::new()
        .config(server_config(vec![allowed_hash]))
        .add_tcp_server("127.0.0.1:0".parse().expect("loopback addr"))
        .storage_path(server_storage.path().to_path_buf())
        .without_events()
        .build()
        .await
        .expect("build server node");
    server.start().await.expect("start server node");
    let server_addr = *server
        .tcp_listen_addrs()
        .first()
        .expect("server reports its bound TCP listener (Codeberg #221)");
    let server_id_hash = server.identity_hash();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Re-drive the management-destination announce here too: the rejection this
    // test asserts only happens AFTER path + link succeed, so a load-starved
    // one-shot path-learn would make the test pass for the wrong reason (a path
    // timeout, not an auth rejection). Re-announcing keeps it exercising the
    // real reject path (Codeberg #110/#103).
    let result = run_rnstatus_with_reannounce(
        &server,
        server_addr,
        server_id_hash,
        client_prv.to_vec(),
        /* include_lstats = */ false,
        /* timeout_secs = */ 8,
    )
    .await;

    // Path + link succeed (those are not gated), but the `/status` request from
    // a disallowed identity is dropped, so rnstatus exits non-zero and prints
    // no JSON status bundle.
    assert_ne!(
        result.code(),
        Some(0),
        "rnstatus from a non-allowed identity must not succeed.\nstdout:\n{}\nstderr:\n{}",
        result.stdout,
        result.stderr
    );
    let printed_bundle = result
        .stdout
        .lines()
        .any(|l| l.trim_start().starts_with('{') && l.contains("\"interfaces\""));
    assert!(
        !printed_bundle,
        "rnstatus must not print a populated status bundle for a disallowed identity.\nstdout:\n{}",
        result.stdout
    );

    server.stop().await.ok();
}
