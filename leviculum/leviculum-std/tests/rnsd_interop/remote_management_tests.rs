//! Remote management interop tests — `lnstatus -R` against Python `rnsd`
//! (Codeberg #86 Stage 3).
//!
//! These exercise the exact flow `lnstatus -R` runs
//! (`leviculum_std::remote_status::fetch_remote_status`) against a Python
//! daemon with `enable_remote_management = yes` and the client's management
//! identity hash in `remote_management_allowed`
//! (`reference/Reticulum/RNS/Transport.py:253-259`, `remote_status_handler`;
//! `reference/Reticulum/RNS/Reticulum.py:548-561`):
//!
//! 1. request a path to the remote `rnstransport.remote.management` destination
//!    (derived from the daemon's transport identity hash),
//! 2. recall the daemon identity and establish an outbound link,
//! 3. `identify` with the management identity (allow-list authentication),
//! 4. issue a `/status` request and decode the returned interface-stats bundle.
//!
//! The Rust client reaches the daemon over a direct TCP client interface (the
//! same transport lnstatus gets via the shared instance in production); the
//! `fetch_remote_status` flow under test is identical either way.
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop remote_management
//! ```

use std::time::Duration;

use rand_core::OsRng;

use leviculum_core::identity::Identity;
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::remote_status::fetch_remote_status;

use crate::harness::TestDaemon;

/// Lowercase hex without external deps.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn parse_hash16(hex: &str) -> [u8; 16] {
    assert_eq!(hex.len(), 32, "identity hash must be 32 hex chars");
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
    }
    out
}

/// Happy path: an allowed management identity gets a populated status bundle
/// back from Python's `remote_status_handler`, including a link count for `-l`.
#[tokio::test]
async fn test_remote_status_from_python_rnsd() {
    // Management identity the client will identify with; its hash goes on the
    // daemon's allow-list.
    let identity = Identity::generate(&mut OsRng);
    let allowed_hash = hex_lower(identity.hash());

    let daemon = TestDaemon::start_with_remote_management(&allowed_hash)
        .await
        .expect("start daemon with remote management");

    // The remote management destination is derived from the daemon's own
    // transport identity hash.
    let status = daemon
        .get_transport_status()
        .await
        .expect("get transport status");
    let daemon_id_hex = status
        .identity_hash
        .expect("daemon should have a transport identity");
    let daemon_id_hash = parse_hash16(&daemon_id_hex);

    // Rust client node: edge node with a direct TCP client to the daemon.
    let _storage = crate::common::temp_storage("test_remote_status_from_python_rnsd", "client");
    let mut client = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build client node");
    let mut events = client.take_event_receiver().expect("event receiver");
    client.start().await.expect("start client node");
    // Let the TCP interface connect.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (stats, link_count) = fetch_remote_status(
        &client,
        &mut events,
        &daemon_id_hash,
        &identity,
        /* include_lstats = */ true,
        Duration::from_secs(20),
        /* quiet = */ false,
    )
    .await
    .expect("remote status should succeed");

    // The daemon runs a TCPServerInterface, so at least one interface must be
    // present in the returned bundle.
    let interfaces = stats
        .get("interfaces")
        .and_then(|v| v.as_array())
        .expect("stats bundle must contain an interfaces array");
    assert!(
        !interfaces.is_empty(),
        "remote status must report at least one interface, got: {stats}"
    );
    // Every interface entry must carry the core stats keys the renderer reads.
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
    // `-l` requested the link count, so Python appended it to the response.
    assert!(
        link_count.is_some(),
        "link count must be present when include_lstats is set"
    );
}

/// Allow-list enforcement: a management identity that is NOT on the daemon's
/// allow-list is refused. Python's request handler drops the request (the
/// requester is not in `remote_management_allowed`), so the client's request
/// concludes as a failure rather than a populated bundle.
#[tokio::test]
async fn test_remote_status_rejected_when_not_allowed() {
    // The daemon allows some OTHER identity, not the one the client uses.
    let allowed_other = Identity::generate(&mut OsRng);
    let allowed_hash = hex_lower(allowed_other.hash());
    let client_identity = Identity::generate(&mut OsRng);

    let daemon = TestDaemon::start_with_remote_management(&allowed_hash)
        .await
        .expect("start daemon with remote management");

    let status = daemon
        .get_transport_status()
        .await
        .expect("get transport status");
    let daemon_id_hash = parse_hash16(
        &status
            .identity_hash
            .expect("daemon should have a transport identity"),
    );

    let _storage =
        crate::common::temp_storage("test_remote_status_rejected_when_not_allowed", "client");
    let mut client = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build client node");
    let mut events = client.take_event_receiver().expect("event receiver");
    client.start().await.expect("start client node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let result = fetch_remote_status(
        &client,
        &mut events,
        &daemon_id_hash,
        &client_identity,
        false,
        Duration::from_secs(8),
        false,
    )
    .await;

    // Path/link succeed (those are not gated), but the `/status` request itself
    // is not answered for a disallowed identity, so the flow returns an error.
    assert!(
        result.is_err(),
        "status request from a non-allowed identity must fail, got: {result:?}"
    );
}
