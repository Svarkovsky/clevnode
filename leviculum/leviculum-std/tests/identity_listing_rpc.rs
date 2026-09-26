//! Integration for `lnstatus --identities` (the `identities` RPC verb): a
//! second daemon announces over TCP, and the first daemon lists the learned
//! identity over its shared-instance RPC socket — the same socket `rnstatus`
//! and `rnpath` speak, queried through the same `rpc_query` client the CLI
//! uses.
//!
//! What the assertions pin, per the 2026-09-04 field finding: the listing
//! carries the announcing node's IDENTITY hash (the input every destination
//! derivation starts from), the announced destination, the path columns for a
//! direct neighbour — and the name column is filled ONLY for an aspect the
//! listing daemon registered itself (`rnstransport.probe`, via
//! respond_to_probes); a foreign app name of the same peer stays null rather
//! than being guessed.

use std::time::Duration;

use leviculum_core::{Destination, DestinationType, Direction, Identity};
use leviculum_std::config::Config;
use leviculum_std::driver::ReticulumNodeBuilder;
use rand_core::OsRng;

#[path = "support/port_alloc.rs"]
#[allow(dead_code)]
mod port_alloc;

fn make_dest(identity: Identity, app: &str, aspects: &[&str]) -> Destination {
    Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        app,
        aspects,
    )
    .expect("create destination")
}

#[tokio::test]
async fn second_daemon_announce_appears_in_identity_listing() {
    let port = port_alloc::free_tcp_port();
    let instance = format!("idlist_{}_{}", std::process::id(), port);

    // Daemon A: transport node, shared instance with the RPC server, probe
    // responder on (so it registers rnstransport.probe under its own name
    // map), listening for B on TCP.
    let identity_a = Identity::generate(&mut OsRng);
    let authkey = {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(identity_a.private_key_bytes().expect("private key"));
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);
        key
    };
    let storage_a = tempfile::tempdir().expect("tempdir a");
    let mut cfg = Config::default();
    cfg.reticulum.respond_to_probes = true;
    let mut node_a = ReticulumNodeBuilder::new()
        .config(cfg)
        .identity(identity_a)
        .enable_transport(true)
        .share_instance(true)
        .instance_name(instance.clone())
        .add_tcp_server(([127, 0, 0, 1], port).into())
        .storage_path(storage_a.path().to_path_buf())
        .build()
        .await
        .expect("build daemon A");
    node_a.start().await.expect("start daemon A");

    // Daemon B: connects to A over TCP and announces two destinations of one
    // identity — the probe aspect A knows by name, and a foreign app name A
    // has never registered.
    let identity_b = Identity::generate(&mut OsRng);
    let b_identity_hash = hex(identity_b.hash());
    let probe_dest = make_dest(identity_b.clone(), "rnstransport", &["probe"]);
    let foreign_dest = make_dest(identity_b, "idlistforeign", &["delivery"]);
    let probe_hash = *probe_dest.hash();
    let foreign_hash = *foreign_dest.hash();

    let storage_b = tempfile::tempdir().expect("tempdir b");
    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(([127, 0, 0, 1], port).into())
        .storage_path(storage_b.path().to_path_buf())
        .build()
        .await
        .expect("build daemon B");
    node_b.start().await.expect("start daemon B");
    node_b.register_destination(probe_dest);
    node_b.register_destination(foreign_dest);

    // Announce until A's listing carries both rows. The announce is repeated
    // because one sent before B's TCP interface finishes coming up is
    // silently dropped and never retried — the retry makes the test
    // deterministic without guessing at startup timing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let rows = loop {
        let _ = node_b.announce_destination(&probe_hash, None).await;
        let _ = node_b.announce_destination(&foreign_hash, None).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        if let Ok(v) = leviculum_std::rpc_query(&instance, &authkey, "identities").await {
            let rows = v.as_array().cloned().unwrap_or_default();
            if rows.len() >= 2 {
                break rows;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon A never listed both announced destinations"
        );
    };

    let row_for = |dest_hex: &str| {
        rows.iter()
            .find(|r| r["destination_hash"].as_str() == Some(dest_hex))
            .unwrap_or_else(|| panic!("no row for destination {dest_hex} in {rows:?}"))
    };

    let probe_row = row_for(&hex(probe_hash.as_bytes()));
    assert_eq!(
        probe_row["identity_hash"].as_str(),
        Some(b_identity_hash.as_str()),
        "the row carries B's identity hash"
    );
    assert_eq!(
        probe_row["name"].as_str(),
        Some("rnstransport.probe"),
        "an aspect A registered itself is named"
    );
    assert_eq!(
        probe_row["hops"].as_i64(),
        Some(1),
        "B is a direct neighbour"
    );
    assert!(
        probe_row["interface"].is_string(),
        "learned-on interface is reported: {probe_row:?}"
    );
    assert!(
        probe_row["via"].is_null(),
        "direct path has no relay hop: {probe_row:?}"
    );
    assert!(
        probe_row["last_seen"].is_number(),
        "last_seen carries the path timestamp: {probe_row:?}"
    );

    let foreign_row = row_for(&hex(foreign_hash.as_bytes()));
    assert_eq!(
        foreign_row["identity_hash"].as_str(),
        Some(b_identity_hash.as_str()),
        "same peer identity behind the foreign name"
    );
    assert!(
        foreign_row["name"].is_null(),
        "a name A never registered stays unknown, not guessed: {foreign_row:?}"
    );

    // The new verb must not disturb the pre-existing surface: the same
    // connection style still answers rnstatus's interface_stats.
    let stats = leviculum_std::rpc_query(&instance, &authkey, "interface_stats")
        .await
        .expect("interface_stats still answers after the identities query");
    assert!(stats.get("interfaces").is_some());

    node_b.stop().await.ok();
    node_a.stop().await.ok();
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
