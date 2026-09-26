//! Interop evidence for Codeberg #155 — a real Python-RNS daemon's path
//! table must order our announces correctly by their emission timestamp.
//!
//! Python writes `int(time.time())` into bytes 5..10 of the announce
//! random_hash (Destination.py:282) and replaces a stored path only when a
//! new announce's emission is strictly newer (Transport.py:1772/1809). Our
//! std stack fills the field with process uptime, so on a Python peer:
//!
//! - a stale announce that looped through the network (real unix timebase,
//!   high hop count) displaces our fresh low-hop path (negative test), and
//! - a poisoned entry can never be reclaimed by re-announcing, because a
//!   fresh process's uptime (~0) never exceeds the stored unix timebase
//!   (positive test — the exact live-pair scenario from the issue).
//!
//! The "stale looped announce" is crafted with the same destination
//! identity the Rust node holds, signed for real, with a chosen emission
//! timestamp and a mangled hop byte (outside the Ed25519 signature), and
//! injected over a second raw TCP connection.
//!
//! **Acceptance**: both tests are red on master HEAD, green once our
//! announces carry unix time.

use std::time::Duration;

use leviculum_core::constants::MTU;
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction, Identity};
use leviculum_std::driver::{ReticulumNode, ReticulumNodeBuilder};
use leviculum_std::interfaces::hdlc::frame;
use rand_core::OsRng;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::common::{connect_to_daemon, now_ms, temp_storage};
use crate::harness::TestDaemon;

fn make_destination(identity: Identity, aspect: &str) -> Destination {
    Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "emission155",
        &[aspect],
    )
    .expect("create destination")
}

/// Craft a signed announce for `dest` carrying `emission_secs` in the
/// 5-byte emission-timestamp field, with the hop byte (outside the
/// signature) set to `hops`.
fn craft_announce_raw(
    dest: &mut Destination,
    emission_secs: u64,
    hops: u8,
    app_data: &[u8],
) -> Vec<u8> {
    let packet = dest
        .announce(Some(app_data), &mut OsRng, 0, emission_secs)
        .expect("craft announce");
    let mut raw = [0u8; MTU];
    let size = packet.pack(&mut raw).expect("pack crafted announce");
    let mut bytes = raw[..size].to_vec();
    bytes[1] = hops;
    bytes
}

async fn send_framed(stream: &mut TcpStream, raw: &[u8]) {
    let mut framed = Vec::new();
    frame(raw, &mut framed);
    stream.write_all(&framed).await.expect("send crafted frame");
    stream.flush().await.expect("flush crafted frame");
}

/// Current hop count in the daemon's path table for `hash`, if any.
async fn daemon_hops(daemon: &TestDaemon, hash: &DestinationHash) -> Option<u8> {
    let table = daemon.get_path_table().await.ok()?;
    table.get(&hex::encode(hash.as_bytes()))?.hops
}

/// Poll the daemon's path table until `hash` is at `want` hops.
async fn wait_for_hops(
    daemon: &TestDaemon,
    hash: &DestinationHash,
    want: u8,
    timeout: Duration,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if daemon_hops(daemon, hash).await == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    daemon_hops(daemon, hash).await == Some(want)
}

async fn start_node(daemon: &TestDaemon, storage: &tempfile::TempDir) -> ReticulumNode {
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build node");
    node.start().await.expect("start node");
    // Let the TCP client connect and settle before announcing.
    tokio::time::sleep(Duration::from_millis(500)).await;
    node
}

/// Negative: a stale announce that looped through the network (older
/// emission, higher hop count) must NOT displace the fresh low-hop path
/// our announce installed on the Python daemon.
#[tokio::test]
async fn stale_looped_announce_cannot_displace_fresh_path_on_python() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let identity = Identity::generate(&mut OsRng);
    let dest = make_destination(identity.clone(), "displace");
    let dest_hash = *dest.hash();

    let storage = temp_storage("emission155_displace", "node");
    let mut node = start_node(&daemon, &storage).await;
    node.register_destination(dest);
    node.announce_destination(&dest_hash, Some(b"fresh"))
        .await
        .expect("announce");

    assert!(
        wait_for_hops(&daemon, &dest_hash, 1, Duration::from_secs(10)).await,
        "Python must first install our direct announce at 1 hop"
    );

    // Craft the stale loop copy: same destination identity, emitted an hour
    // ago, arriving over a second interface at a high hop count.
    let stale_emission = now_ms() / 1000 - 3_600;
    let mut craft_dest = make_destination(identity.clone(), "displace");
    let stale = craft_announce_raw(&mut craft_dest, stale_emission, 4, b"stale-loop");

    // Canary on the same injection channel: a fresh-table install for a
    // second destination proves the crafted frames actually reach and pass
    // validation on the daemon, so the main assertion cannot pass vacuously.
    let mut canary_dest = make_destination(Identity::generate(&mut OsRng), "canary");
    let canary_hash = *canary_dest.hash();
    let canary = craft_announce_raw(&mut canary_dest, stale_emission, 4, b"canary");

    let mut injector = connect_to_daemon(&daemon).await;
    send_framed(&mut injector, &stale).await;
    send_framed(&mut injector, &canary).await;

    assert!(
        wait_for_hops(&daemon, &canary_hash, 5, Duration::from_secs(10)).await,
        "canary announce must install (injection channel sanity)"
    );
    // Give the stale announce ample processing time beyond the canary.
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        daemon_hops(&daemon, &dest_hash).await,
        Some(1),
        "the stale looped announce (emission {stale_emission}) displaced our \
         fresh 1-hop path: our announce's emission timestamp must be unix \
         time so Python's `announce_emitted > path_timebase` rejects the \
         stale copy (Codeberg #155)"
    );

    node.stop().await.ok();
}

/// Positive: a Python daemon holding a poisoned (stale-timebase, high-hop)
/// entry must accept a fresh announce from the restarted destination and
/// restore the 1-hop path. This is the recovery scenario from the issue:
/// the crafted announce stands in for our own pre-restart announce still
/// circulating in the network.
#[tokio::test]
async fn fresh_announce_reclaims_poisoned_path_on_python() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let identity = Identity::generate(&mut OsRng);
    let dest_hash = *make_destination(identity.clone(), "reclaim").hash();

    // Poison first: the "pre-restart" announce, emitted an hour ago,
    // arrives at a high hop count and installs into the empty table.
    let stale_emission = now_ms() / 1000 - 3_600;
    let mut craft_dest = make_destination(identity.clone(), "reclaim");
    let stale = craft_announce_raw(&mut craft_dest, stale_emission, 4, b"pre-restart");

    let mut injector = connect_to_daemon(&daemon).await;
    send_framed(&mut injector, &stale).await;

    assert!(
        wait_for_hops(&daemon, &dest_hash, 5, Duration::from_secs(10)).await,
        "the crafted pre-restart announce must install at 5 hops first"
    );

    // "Restart": the same destination comes up in a fresh process and
    // re-announces directly to the daemon.
    let dest = make_destination(identity.clone(), "reclaim");
    let storage = temp_storage("emission155_reclaim", "node");
    let mut node = start_node(&daemon, &storage).await;
    node.register_destination(dest);
    node.announce_destination(&dest_hash, Some(b"post-restart"))
        .await
        .expect("announce");

    assert!(
        wait_for_hops(&daemon, &dest_hash, 1, Duration::from_secs(10)).await,
        "the fresh direct announce must reclaim the poisoned path (5 hops -> \
         1 hop). With an uptime-based emission timestamp the fresh announce \
         can never beat the stored stale timebase {stale_emission} and the \
         destination stays unreachable (Codeberg #155)"
    );

    node.stop().await.ok();
}
