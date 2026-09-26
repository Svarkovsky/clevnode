//! IFAC (Interface Access Code) interop: our lnsd joins a Python
//! IFAC-protected network over TCP (Codeberg #90).
//!
//! IFAC authenticates every packet on an interface: from network_name +
//! passphrase both stacks derive the same ifac_identity/ifac_key, sign each
//! outbound packet, embed the signature tail as the access code, and set the
//! `[ifac:1]` header bit. On receipt the code is verified and the packet is
//! dropped on mismatch (Transport.py:1051-1090 outbound, 1398-1449 inbound).
//!
//! The Python daemon runs the reference implementation: its TCPServerInterface
//! is IFAC-protected from config (`network_name`/`passphrase`), so it derives
//! the access code natively. Our lnsd connects as a TCPClientInterface built
//! from an on-disk config that carries the same (or a mismatched) IFAC keys,
//! exercising the full ini_config -> build_ifac_config -> transport apply/verify
//! path.
//!
//! Two properties are proven:
//!   * MATCHING network_name + passphrase -> announces flow BOTH directions
//!     (wire compatibility: our HMAC verifies against Python's and vice versa).
//!   * MISMATCHED passphrase -> NO announce crosses either direction (the
//!     security property: unauthenticated/foreign-key traffic is dropped).
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop ifac_interop
//! ```

use std::time::Duration;

use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationType, Direction};
use leviculum_std::config::Config;
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{init_tracing, parse_dest_hash, temp_storage, wait_for_path_on_node};
use crate::harness::TestDaemon;

const NETNAME: &str = "leviculum-ifac-net";
const PASSPHRASE: &str = "correct horse battery staple";

/// Write a `TCPClientInterface` config that targets `port` and carries the
/// given IFAC `network_name`/`passphrase`. Loaded through the real
/// `Config::load` so the IFAC keys traverse the production parse path.
fn write_ifac_client_config(
    dir: &std::path::Path,
    port: u16,
    netname: &str,
    passphrase: &str,
) -> std::path::PathBuf {
    let content = format!(
        "[reticulum]\n\
         \x20 enable_transport = True\n\
         \n\
         [interfaces]\n\
         \x20 [[IFAC Client]]\n\
         \x20   type = TCPClientInterface\n\
         \x20   target_host = 127.0.0.1\n\
         \x20   target_port = {port}\n\
         \x20   network_name = {netname}\n\
         \x20   passphrase = {passphrase}\n"
    );
    let path = dir.join("config");
    std::fs::write(&path, content).expect("write IFAC client config");
    path
}

/// Bring up our lnsd from an IFAC client config pointed at `daemon`.
async fn start_ifac_client(
    test_name: &str,
    daemon: &TestDaemon,
    netname: &str,
    passphrase: &str,
) -> (leviculum_std::driver::ReticulumNode, tempfile::TempDir) {
    let storage = temp_storage(test_name, "node");
    let cfg_path = write_ifac_client_config(storage.path(), daemon.rns_port(), netname, passphrase);
    let config = Config::load(&cfg_path).expect("IFAC client config must load");
    // Sanity: the IFAC keys survived the parse.
    {
        let iface = config
            .interfaces
            .get("IFAC Client")
            .expect("IFAC Client interface must survive load");
        assert_eq!(iface.networkname.as_deref(), Some(netname));
        assert_eq!(iface.passphrase.as_deref(), Some(passphrase));
    }
    let mut node = ReticulumNodeBuilder::new()
        .config(config)
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build lnsd from IFAC config");
    node.start().await.expect("start lnsd");
    // Let the TCP client connect to the Python server.
    tokio::time::sleep(Duration::from_secs(1)).await;
    (node, storage)
}

/// MATCHING network_name + passphrase: announces cross both directions,
/// proving our IFAC HMAC is byte-compatible with Python's.
#[tokio::test]
async fn test_ifac_matching_passphrase_announces_flow() {
    init_tracing();

    // Python daemon with an IFAC-protected TCP server and a probe destination
    // it announces (so the inbound direction is observable).
    let py = TestDaemon::start_with_ifac(NETNAME, PASSPHRASE, None)
        .await
        .expect("start Python daemon with IFAC");
    let py_probe_hex = py
        .probe_dest_hash()
        .expect("Python daemon should print PROBE_DEST:<hex>")
        .to_string();

    // Our lnsd connects with the SAME IFAC keys.
    let (mut node, _storage) = start_ifac_client(
        "test_ifac_matching_passphrase_announces_flow",
        &py,
        NETNAME,
        PASSPHRASE,
    )
    .await;

    // Inbound: Python's probe announce must become a path on our node.
    let probe_hash = parse_dest_hash(&py_probe_hex);
    assert!(
        wait_for_path_on_node(&node, &probe_hash, Duration::from_secs(25)).await,
        "with a matching IFAC passphrase our lnsd must learn a path to Python's \
         probe destination (inbound announce authenticated)"
    );
    assert_eq!(
        node.hops_to(&probe_hash),
        Some(1),
        "Python is a direct IFAC neighbor over the TCP link (hops=1)"
    );

    // Outbound: a destination we announce must become a path on Python.
    let rust_identity = Identity::generate(&mut rand_core::OsRng);
    let mut dest = Destination::new(
        Some(rust_identity),
        Direction::In,
        DestinationType::Single,
        "ifactest",
        &["echo"],
    )
    .expect("create Rust destination");
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    node.announce_destination(&dest_hash, Some(b"rust-ifac"))
        .await
        .expect("announce Rust destination");

    let py_has_path = crate::common::wait_for_node_reannounce_on_daemon(
        &py,
        &dest_hash,
        &node,
        b"rust-ifac",
        Duration::from_secs(20),
    )
    .await;
    assert!(
        py_has_path,
        "with a matching IFAC passphrase the Python daemon must learn a path to \
         our announced destination (outbound announce authenticated)"
    );

    node.stop().await.ok();
}

/// MISMATCHED passphrase: the TCP link still connects, but every packet
/// carries an access code the other side cannot verify, so NO announce crosses
/// in either direction. This is the security property IFAC exists to provide.
#[tokio::test]
async fn test_ifac_mismatched_passphrase_traffic_dropped() {
    init_tracing();

    let py = TestDaemon::start_with_ifac(NETNAME, PASSPHRASE, None)
        .await
        .expect("start Python daemon with IFAC");
    let py_probe_hex = py
        .probe_dest_hash()
        .expect("Python daemon should print PROBE_DEST:<hex>")
        .to_string();

    // Our lnsd connects with the SAME network_name but the WRONG passphrase, so
    // its derived IFAC key differs and its HMAC never verifies on the Python
    // side (and vice versa).
    let (mut node, _storage) = start_ifac_client(
        "test_ifac_mismatched_passphrase_traffic_dropped",
        &py,
        NETNAME,
        "totally different passphrase",
    )
    .await;

    // Register + announce a Rust destination so there is outbound traffic to
    // (fail to) cross.
    let rust_identity = Identity::generate(&mut rand_core::OsRng);
    let mut dest = Destination::new(
        Some(rust_identity),
        Direction::In,
        DestinationType::Single,
        "ifacdrop",
        &["echo"],
    )
    .expect("create Rust destination");
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    node.announce_destination(&dest_hash, Some(b"rust-ifac-drop"))
        .await
        .expect("announce Rust destination");

    // Give both sides ample time to (fail to) exchange announces. The matching
    // test observes both paths well within this window.
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Inbound must NOT have crossed: Python's probe announce is dropped by our
    // node because its access code does not verify under our (wrong) key.
    let probe_hash = parse_dest_hash(&py_probe_hex);
    assert!(
        !node.has_path(&probe_hash),
        "a mismatched IFAC passphrase must NOT admit Python's probe announce"
    );

    // Outbound must NOT have crossed: Python drops our announce because its
    // access code does not verify under Python's key.
    assert!(
        !py.has_path(dest_hash.as_bytes()).await,
        "a mismatched IFAC passphrase must NOT admit our announce onto the \
         Python daemon"
    );

    node.stop().await.ok();
}
