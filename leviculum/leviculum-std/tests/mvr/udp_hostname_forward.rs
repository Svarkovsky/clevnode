//! mvr for Codeberg #148 — UDPInterface rejects a hostname in `forward_ip`
//! that Python accepts.
//!
//! Python-RNS `UDPInterface.process_outgoing` passes `forward_ip` straight
//! to `sendto` (UDPInterface.py:124-127), so the OS resolves a hostname on
//! every send and a resolution failure is a logged interface error that
//! leaves the daemon running. Our `parse_forward_addrs` only accepted
//! numeric addresses, so the same config file that works against rnsd was
//! rejected by lnsd at startup — a P1 config-compatibility gap. The
//! container topology that found this names its UDP peer by service name.
//!
//! Both tests drive the real config path (`Config` → interface builder), a
//! raw UDP socket stands in for the peer — no Python, no Docker, < 5 s.
//!
//! **Acceptance**: both tests are red on master 4dcd3b4 (node build fails
//! with "UDPInterface invalid forward address"), green once `forward_ip`
//! accepts a hostname and resolution failures stay interface-level.

use std::time::Duration;

use leviculum_core::{Destination, DestinationType, Direction, Identity};
use leviculum_std::config::{Config, InterfaceConfig};
use leviculum_std::driver::ReticulumNodeBuilder;
use rand_core::OsRng;

/// Config with a single UDPInterface whose peer is named by `forward_ip`,
/// exactly as an rnsd-style config file would.
fn udp_config(forward_ip: &str, forward_port: u16) -> Config {
    let mut config = Config::default();
    config.interfaces.insert(
        "udp".to_string(),
        InterfaceConfig {
            interface_type: "UDPInterface".to_string(),
            listen_ip: Some("127.0.0.1".to_string()),
            listen_port: Some(0),
            forward_ip: Some(forward_ip.to_string()),
            forward_port: Some(forward_port),
            ..Default::default()
        },
    );
    config
}

fn make_destination() -> Destination {
    Destination::new(
        Some(Identity::generate(&mut OsRng)),
        Direction::In,
        DestinationType::Single,
        "mvr148",
        &["udp", "hostname"],
    )
    .expect("create destination")
}

/// (a) A `forward_ip` naming the peer by hostname must build, start, and
/// deliver datagrams to the resolved address (here: `localhost` from
/// /etc/hosts — no network dependency).
#[tokio::test]
async fn udp_hostname_forward_resolves_and_delivers() {
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind peer socket");
    let peer_port = peer.local_addr().expect("peer addr").port();

    let storage = tempfile::tempdir().expect("tempdir");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .config(udp_config("localhost", peer_port))
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("a hostname forward_ip must build — rnsd accepts this config");
    node.start().await.expect("start node");

    let dest = make_destination();
    let dest_hash = *dest.hash();
    node.register_destination(dest);

    // Re-announce on a short cadence instead of sleeping for interface
    // settle + name resolution; the first datagram to arrive ends the test.
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut delivered = false;
    while tokio::time::Instant::now() < deadline {
        node.announce_destination(&dest_hash, Some(b"mvr148"))
            .await
            .expect("announce");
        if let Ok(recv) =
            tokio::time::timeout(Duration::from_millis(500), peer.recv_from(&mut buf)).await
        {
            let (len, _) = recv.expect("recv at peer");
            assert!(len > 0, "peer received an empty datagram");
            delivered = true;
            break;
        }
    }
    assert!(
        delivered,
        "announce must reach the peer named by hostname (Codeberg #148)"
    );

    node.stop().await.expect("stop node");
}

/// (b) A `forward_ip` that does not resolve is an interface-level error:
/// the daemon builds, starts, accepts sends, and keeps running — matching
/// Python, where the failure surfaces per send inside process_outgoing.
#[tokio::test]
async fn udp_hostname_unresolvable_keeps_daemon_up() {
    let storage = tempfile::tempdir().expect("tempdir");
    // ".invalid" is reserved (RFC 2606) and never resolves.
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .config(udp_config("unresolvable-peer.invalid", 4242))
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("an unresolvable forward_ip must not be a config error");
    node.start()
        .await
        .expect("daemon must start with an unresolvable peer");

    let dest = make_destination();
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    // The send fails at the interface (logged), never up here.
    node.announce_destination(&dest_hash, Some(b"mvr148"))
        .await
        .expect("announce must not surface a resolution failure");

    tokio::time::sleep(Duration::from_millis(500)).await;
    node.stop()
        .await
        .expect("daemon must still be running after failed resolutions");
}
