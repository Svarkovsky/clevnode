//! mvr for Codeberg #155 — announces carry process uptime as the emission
//! timestamp instead of unix time.
//!
//! Python-RNS writes `int(time.time())` into bytes 5..10 of the announce
//! random_hash (Destination.py:282) and orders same-destination paths by
//! that value (Transport.py:1772/1809: `announce_emitted > path_timebase`).
//! Our std stack fills the field from `SystemClock::now_ms()/1000`, i.e.
//! seconds since process start. Consequences on a Python peer:
//!
//! - a stale looped announce with a large timebase beats a fresh local one,
//! - a freshly restarted process (uptime ~ 0) can never reclaim its own
//!   path entry until the 7-day `PATHFINDER_E` expiry.
//!
//! Both tests capture our node's real on-wire announce through a plain TCP
//! listener standing in for the peer — no Python, no Docker, < 5 s.
//!
//! **Acceptance**: both tests are red on master HEAD (emission decodes to
//! process uptime), green once the emission timestamp carries unix time.

use std::time::Duration;

use leviculum_core::{Destination, DestinationType, Direction, Identity};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::interfaces::hdlc::{DeframeResult, Deframer};
use rand_core::OsRng;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

/// Truncated destination hash length (bytes) on the wire.
const DEST_HASH_LEN: usize = 16;
/// Type-1 header: flags(1) + hops(1) + dest(16) + context(1).
const HEADER_LEN: usize = 2 + DEST_HASH_LEN + 1;
/// Offset of the 5-byte emission timestamp inside the announce payload:
/// public_key(64) + name_hash(10) + random_part(5).
const EMISSION_OFFSET: usize = 64 + 10 + 5;

/// Decode the emission timestamp from a raw (deframed) announce packet for
/// `dest_hash`, or `None` if this frame is not such an announce.
fn emission_from_announce(raw: &[u8], dest_hash: &[u8; DEST_HASH_LEN]) -> Option<u64> {
    if raw.len() < HEADER_LEN + EMISSION_OFFSET + 5 {
        return None;
    }
    // packet_type lives in flags bits 0..2; Announce = 0b01.
    if raw[0] & 0b11 != 0b01 {
        return None;
    }
    if &raw[2..2 + DEST_HASH_LEN] != dest_hash {
        return None;
    }
    let ts = &raw[HEADER_LEN + EMISSION_OFFSET..HEADER_LEN + EMISSION_OFFSET + 5];
    Some(
        ((ts[0] as u64) << 32)
            | ((ts[1] as u64) << 24)
            | ((ts[2] as u64) << 16)
            | ((ts[3] as u64) << 8)
            | (ts[4] as u64),
    )
}

/// Read frames from `stream` until an announce for `dest_hash` appears,
/// returning its emission timestamp. Panics after `timeout`.
async fn read_announce_emission(
    stream: &mut TcpStream,
    dest_hash: &[u8; DEST_HASH_LEN],
    timeout: Duration,
) -> u64 {
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut buf))
            .await
            .expect("timed out waiting for the announce on the wire")
            .expect("read from node connection");
        assert!(n > 0, "node closed the connection before announcing");
        for result in deframer.process(&buf[..n]) {
            if let DeframeResult::Frame(data) = result {
                if let Some(emission) = emission_from_announce(&data, dest_hash) {
                    return emission;
                }
            }
        }
    }
}

fn make_destination(identity: Identity) -> Destination {
    Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvr155",
        &["emission", "unixtime"],
    )
    .expect("create destination")
}

/// Boot a node against `listener`, announce `dest`, and return the emission
/// timestamp its on-wire announce carried.
async fn announce_and_capture(listener: &TcpListener, dest: Destination) -> u64 {
    let dest_hash = *dest.hash();
    let storage = tempfile::tempdir().expect("tempdir");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(listener.local_addr().expect("listener addr"))
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build node");
    node.start().await.expect("start node");

    let (mut peer, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("timed out waiting for the node's TCP connect")
        .expect("accept node connection");

    // Let the interface finish coming up before broadcasting.
    tokio::time::sleep(Duration::from_millis(300)).await;

    node.register_destination(dest);
    node.announce_destination(&dest_hash, Some(b"mvr155"))
        .await
        .expect("announce");

    let emission =
        read_announce_emission(&mut peer, dest_hash.as_bytes(), Duration::from_secs(5)).await;
    node.stop().await.ok();
    emission
}

/// (a) The emission timestamp of an emitted announce must decode to a
/// plausible unix time, not to process uptime.
#[tokio::test]
async fn announce_emission_is_unix_time() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let dest = make_destination(Identity::generate(&mut OsRng));

    let emission = announce_and_capture(&listener, dest).await;

    // Wide plausibility band: 2025-07..2039-09. Process uptime (seconds
    // since start) is at most a few hundred here and fails hard.
    assert!(
        (1_750_000_000..2_200_000_000).contains(&emission),
        "announce emission timestamp must be unix seconds, got {emission} \
         (process uptime instead of wall clock — Codeberg #155)"
    );
}

/// (b) A restarted node (fresh process clock) re-announcing the same
/// destination must beat its own pre-restart announce under Python's
/// ordering rule (`announce_emitted > path_timebase`, Transport.py:1809),
/// i.e. the second emission must be strictly greater.
#[tokio::test]
async fn restart_reannounce_beats_pre_restart_announce() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let identity = Identity::generate(&mut OsRng);

    let first = announce_and_capture(&listener, make_destination(identity.clone())).await;

    // The field has 1 s granularity; span at least two full seconds so a
    // correct implementation is strictly greater. With the uptime bug both
    // processes announce at uptime ~0 s and the comparison is 0 > 0.
    tokio::time::sleep(Duration::from_millis(2_100)).await;

    let second = announce_and_capture(&listener, make_destination(identity)).await;

    assert!(
        second > first,
        "post-restart announce (emission {second}) must beat the pre-restart \
         announce (emission {first}) under Python's path ordering; a node \
         whose emission timestamp restarts from zero poisons its own path \
         entries on every Python peer (Codeberg #155)"
    );
}
