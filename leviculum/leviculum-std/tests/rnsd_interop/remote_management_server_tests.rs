//! Remote-management SERVER tests — our `lnsd` serving `/status` to a client
//! (Codeberg #86 Stage 3b).
//!
//! Stage 3a proved our `rnstatus -R` CLIENT against a Python `rnsd`
//! (`remote_management_tests.rs`). This file proves the mirror image: our stack
//! as the remote-management SERVER, answering the same `/status` request/link/
//! identify flow. The Python `TestDaemon` has no `link.request` client surface,
//! so the driver is our own Stage-3a client (`fetch_remote_status`) pointed at
//! our server node — a rust<->rust exercise of the full server path
//! (`NodeCore::enable_remote_management`, the `/status` request handler, the
//! allow-list, and the response packet/Resource).
//!
//! GAP vs Python: this does not yet drive Python `rnstatus -R` against our
//! server (that needs a Python-as-client harness helper). Wire compatibility is
//! preserved deliberately: the server frames the RESPONSE packet as
//! `[request_id, response]` and, for a bundle over the link MDU, sends a
//! response Resource carrying `[request_id, response]` with the `is_response`
//! flag and `request_id` set — the exact bytes our client already accepts from
//! a Python `rnsd`.
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop remote_management_server
//! ```

use std::time::Duration;

use rand_core::OsRng;

use leviculum_core::identity::Identity;
use leviculum_std::config::Config;
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::remote_status::fetch_remote_status;

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

/// Happy path: an allowed management identity gets a populated status bundle
/// back from OUR server's `/status` handler, including a link count for `-l`.
#[tokio::test]
async fn test_remote_status_server_allows_and_serves() {
    // Management identity the client identifies with; its hash goes on the
    // server's allow-list.
    let mgmt_identity = Identity::generate(&mut OsRng);
    let allowed_hash = hex_lower(mgmt_identity.hash());

    // Server: a transport instance with a TCP server interface and remote
    // management enabled. `without_events()` exercises the daemon path — the
    // `/status` responder must run without an application event sink.
    let server_storage =
        crate::common::temp_storage("test_remote_status_server_allows_and_serves", "server");
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

    // Client: an edge node with a direct TCP client to the server.
    let client_storage =
        crate::common::temp_storage("test_remote_status_server_allows_and_serves", "client");
    let mut client = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(server_addr)
        .storage_path(client_storage.path().to_path_buf())
        .build()
        .await
        .expect("build client node");
    let mut events = client.take_event_receiver().expect("event receiver");
    client.start().await.expect("start client node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (stats, link_count) = fetch_remote_status(
        &client,
        &mut events,
        &server_id_hash,
        &mgmt_identity,
        /* include_lstats = */ true,
        Duration::from_secs(20),
        /* quiet = */ false,
    )
    .await
    .expect("remote status should succeed against our server");

    // The server runs a TCPServerInterface, so the bundle must list at least
    // one interface, each carrying the core keys the renderer reads.
    let interfaces = stats
        .get("interfaces")
        .and_then(|v| v.as_array())
        .expect("stats bundle must contain an interfaces array");
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
    // A transport instance reports its transport_id in the bundle.
    assert_eq!(
        stats.get("transport_id").and_then(|v| v.as_str()),
        Some(hex_lower(&server_id_hash).as_str()),
        "transport_id must be the server's transport identity hash"
    );
    // `-l` requested the link count, so the server appended it. The number is
    // `len(Transport.link_table)` — the links the server RELAYS
    // (`Transport.remote_status_handler` → `get_link_count`, Reticulum 1.5.2)
    // — and the client's management link is not one of them: the client is a
    // direct TCP neighbour and the server TERMINATES that link, so it lands in
    // the local-link inventory and never in the relay table. An rnsd in this
    // topology answers 0 for the same reason.
    //
    // Present-but-zero is the whole assertion: it separates "the server
    // appended the count" from "the server had a relayed link to count", which
    // an `is_some()` alone would not.
    let lc = link_count.expect("link count must be present when include_lstats is set");
    assert_eq!(
        lc, 0,
        "the server relays no link in this two-node topology; a non-zero \
         count here means the verb is reporting terminated links again"
    );

    server.stop().await.ok();
    client.stop().await.ok();
}

/// Allow-list enforcement: a management identity that is NOT on the server's
/// allow-list is refused. The core drops the `/status` request (Python parity),
/// so the client's request concludes as a failure rather than a bundle.
#[tokio::test]
async fn test_remote_status_server_rejects_non_allowed() {
    // The server allows some OTHER identity, not the one the client uses.
    let allowed_other = Identity::generate(&mut OsRng);
    let allowed_hash = hex_lower(allowed_other.hash());
    let client_identity = Identity::generate(&mut OsRng);

    let server_storage =
        crate::common::temp_storage("test_remote_status_server_rejects_non_allowed", "server");
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

    let client_storage =
        crate::common::temp_storage("test_remote_status_server_rejects_non_allowed", "client");
    let mut client = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(server_addr)
        .storage_path(client_storage.path().to_path_buf())
        .build()
        .await
        .expect("build client node");
    let mut events = client.take_event_receiver().expect("event receiver");
    client.start().await.expect("start client node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let result = fetch_remote_status(
        &client,
        &mut events,
        &server_id_hash,
        &client_identity,
        /* include_lstats = */ false,
        Duration::from_secs(6),
        /* quiet = */ false,
    )
    .await;

    // Path/link succeed (those are not gated), but the `/status` request from a
    // disallowed identity is dropped, so the flow returns an error.
    assert!(
        result.is_err(),
        "status request from a non-allowed identity must fail, got: {result:?}"
    );

    server.stop().await.ok();
    client.stop().await.ok();
}
