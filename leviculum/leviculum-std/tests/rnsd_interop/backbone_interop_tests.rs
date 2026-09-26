//! Config drop-in: our lnsd reads a `type = BackboneInterface` config and peers
//! with a REAL Python `BackboneClientInterface` (Codeberg #89).
//!
//! `BackboneInterface` is wire-identical to `TCPInterface` (HDLC-over-TCP, same
//! FLAG/ESC framing, no handshake). This test proves the drop-in end-to-end:
//!
//! 1. Our lnsd is built from an on-disk config whose only interface is
//!    `type = BackboneInterface` (server / listen). Loading it through the real
//!    `Config::load` path normalizes it onto our TCP server exactly as Python
//!    does (`Reticulum.py:960-972`).
//! 2. A real Python `BackboneClientInterface` connects to that TCP listener.
//! 3. Announces cross both directions over that Backbone<->TCP segment:
//!    Python's probe destination becomes a path on our node (inbound), and a
//!    destination we announce becomes a path on the Python daemon (outbound).
//!
//! Direction covered: our-server <- Python-Backbone-client (both announce
//! directions ride the same link). `BackboneInterface` is Linux-only in Python;
//! schneckenschreck is Linux, so it runs.
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop backbone_interop
//! ```

use std::time::Duration;

use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationType, Direction};
use leviculum_std::config::Config;
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{init_tracing, parse_dest_hash, temp_storage, wait_for_path_on_node};
use crate::harness::{find_available_ports, TestDaemon};

/// Build a `type = BackboneInterface` server config on disk, bound to `port`.
/// Carries an unknown Backbone-only key (`prioritise`) to exercise the real
/// `Config::load` unknown-key tolerance path, not just the parser unit tests.
fn write_backbone_server_config(dir: &std::path::Path, port: u16) -> std::path::PathBuf {
    let content = format!(
        "[reticulum]\n\
         \x20 enable_transport = True\n\
         \n\
         [interfaces]\n\
         \x20 [[Backbone Server]]\n\
         \x20   type = BackboneInterface\n\
         \x20   listen_on = 127.0.0.1\n\
         \x20   port = {port}\n\
         \x20   prioritise = lo\n"
    );
    let path = dir.join("config");
    std::fs::write(&path, content).expect("write backbone config");
    path
}

#[tokio::test]
async fn test_backbone_config_dropin_peers_with_python_backbone_client() {
    init_tracing();

    // Reserve a free TCP port for our Backbone/TCP listener.
    let (ports, _port_alloc) = find_available_ports::<2>().await.expect("allocate port");
    let server_port = ports[0];

    // --- Drop-in parse: a stock Backbone config must load as a TCP server. ---
    let cfg_storage = temp_storage(
        "test_backbone_config_dropin_peers_with_python_backbone_client",
        "config",
    );
    let cfg_path = write_backbone_server_config(cfg_storage.path(), server_port);
    let config = Config::load(&cfg_path).expect("Backbone config must load through Config::load");
    {
        let iface = config
            .interfaces
            .get("Backbone Server")
            .expect("Backbone Server interface must survive the load");
        assert_eq!(
            iface.interface_type, "TCPServerInterface",
            "a listen-only BackboneInterface must normalize onto our TCP server"
        );
        assert_eq!(iface.listen_ip, Some("127.0.0.1".to_string()));
        assert_eq!(iface.listen_port, Some(server_port));
    }

    // --- Bring up our lnsd from that config. ---
    let node_storage = temp_storage(
        "test_backbone_config_dropin_peers_with_python_backbone_client",
        "node",
    );
    let mut node = ReticulumNodeBuilder::new()
        .config(config)
        .storage_path(node_storage.path().to_path_buf())
        .build()
        .await
        .expect("build lnsd from Backbone config");
    node.start().await.expect("start lnsd");
    // Let the TCP listener bind.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // --- Python daemon with a probe destination it announces. ---
    let py = TestDaemon::start_with_probes()
        .await
        .expect("start Python daemon with probes");
    let py_probe_hex = py
        .probe_dest_hash()
        .expect("Python daemon should print PROBE_DEST:<hex>")
        .to_string();

    // Real Python BackboneClientInterface connects to our TCP listener.
    py.add_backbone_client_interface("127.0.0.1", server_port, Some("ToLnsdBackbone"))
        .await
        .expect("Python BackboneClientInterface must connect to our lnsd");

    // --- Inbound: Python's probe announce must become a path on our node. ---
    let probe_hash = parse_dest_hash(&py_probe_hex);
    assert!(
        wait_for_path_on_node(&node, &probe_hash, Duration::from_secs(25)).await,
        "our lnsd must learn a path to Python's probe destination over the \
         Python BackboneClientInterface <-> our TCP server segment"
    );
    assert_eq!(
        node.hops_to(&probe_hash),
        Some(1),
        "Python is a direct neighbor over the Backbone link (hops=1)"
    );

    // --- Outbound: a destination we announce must become a path on Python. ---
    let rust_identity = Identity::generate(&mut rand_core::OsRng);
    let mut dest = Destination::new(
        Some(rust_identity),
        Direction::In,
        DestinationType::Single,
        "backbonetest",
        &["echo"],
    )
    .expect("create Rust destination");
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    node.announce_destination(&dest_hash, Some(b"rust-backbone"))
        .await
        .expect("announce Rust destination");

    let py_has_path = crate::common::wait_for_node_reannounce_on_daemon(
        &py,
        &dest_hash,
        &node,
        b"rust-backbone",
        Duration::from_secs(20),
    )
    .await;
    assert!(
        py_has_path,
        "the Python daemon must learn a path to our announced destination over \
         its BackboneClientInterface"
    );

    node.stop().await.ok();
}
