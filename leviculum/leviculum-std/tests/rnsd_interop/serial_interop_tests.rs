//! Live-Python serial-interface interop tests (Codeberg #102).
//!
//! `#95` (Pipe), `#96` (KISS) and `#97` (AX.25) proved these interfaces
//! end-to-end Rust<->Rust and pinned their framing byte-for-byte against Python
//! with KATs. What was missing was a live Python peer on the other end of the
//! wire: at the time these landed, `socat` was not installed, so there was no
//! way to give a real Python `rnsd` a serial port to talk over.
//!
//! These tests close that gap. For KISS and AX.25 a `socat` pty pair stands in
//! for a serial cable: `socat -d -d pty,raw,echo=0 pty,raw,echo=0` opens two
//! linked `/dev/pts/N` devices, a real Python `rnsd` (via [`TestDaemon`]) opens
//! one end with a matching interface, and our lnsd opens the other. An announce
//! must then cross the serial link in both directions — Python -> Rust learned
//! as a [`NodeEvent::AnnounceReceived`], Rust -> Python learned into Python's
//! path table.
//!
//! Pipe is a subprocess bridge, not a serial port, so it does not use a pty:
//! both daemons run a `PipeInterface` whose command is the `pipe_bridge.py`
//! stdio<->TCP relay, and the two relays splice together over loopback. The
//! crossing proof is identical.
//!
//! All three are `#[ignore]` — they spawn a Python interpreter (and `socat`),
//! so they run under `--include-ignored`, not in the default fast pass.

use std::time::Duration;

use leviculum_core::DestinationHash;
use leviculum_std::driver::{ReticulumNode, ReticulumNodeBuilder};
use leviculum_std::{Destination, DestinationType, Direction, Identity, NodeEvent};

use crate::common::{init_tracing, temp_storage, wait_for_event};
use crate::harness::{SocatPtyPair, TestDaemon};

/// Absolute path to the stdio<->TCP bridge helper shared with the Pipe tests.
const BRIDGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/pipe_bridge.py");

/// Baud rate used for the pty serial link. A pty ignores the physical rate, but
/// both ends must agree on the configured value so the interfaces match.
const SERIAL_SPEED: u32 = 115200;

/// A loopback TCP port for the two bridge halves to rendezvous on. The number
/// crosses a process boundary through argv, so it cannot be a kept `:0`
/// listener; it comes from the host-wide allocator band above the ephemeral
/// range instead (Codeberg #221). `SO_REUSEADDR` on the listening half and the
/// connecting half's retry cover the allocator's own probe release.
fn free_tcp_port() -> u16 {
    crate::harness::port_alloc::free_tcp_port()
}

/// Drive both announce directions across a serial-family link between a live
/// Python `rnsd` and our lnsd, and assert each side learns the other.
///
/// `label` names the aspect used for the destinations so parallel tests do not
/// collide. `node` is a fully started lnsd whose only over-the-air interface is
/// the mirror of the daemon's serial-family interface.
async fn assert_announce_crosses_both_ways(
    daemon: &TestDaemon,
    node: &mut ReticulumNode,
    label: &str,
) {
    let mut events = node.take_event_receiver().expect("event receiver");

    // Both sides need their interface up before the first announce. Our side is
    // gated on wait_for_interfaces_ready by the caller; give the link a short
    // settle for the Python side's read loop / bridge handshake to come up.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- Direction 1: Python announces, our lnsd must learn it. ----
    let py_dest = daemon
        .register_destination("interop", &[label])
        .await
        .expect("register Python destination");
    let py_hash_bytes: [u8; 16] = hex::decode(&py_dest.hash)
        .expect("valid hex hash")
        .try_into()
        .expect("16-byte destination hash");
    let py_hash = DestinationHash::new(py_hash_bytes);

    let mut py_app_data = None;
    for _ in 0..8 {
        daemon
            .announce_destination(&py_dest.hash, b"py-serial-hello")
            .await
            .expect("Python announce");
        py_app_data = wait_for_event(&mut events, Duration::from_secs(3), |event| {
            if let NodeEvent::AnnounceReceived { announce, .. } = event {
                if *announce.destination_hash() == py_hash {
                    return Some(announce.app_data().to_vec());
                }
            }
            None
        })
        .await;
        if py_app_data.is_some() {
            break;
        }
    }
    let py_app_data =
        py_app_data.expect("our lnsd should learn the Python announce over the serial link");
    assert_eq!(
        py_app_data, b"py-serial-hello",
        "app_data must survive the framing round-trip from Python"
    );
    assert!(
        node.has_path(&py_hash),
        "our lnsd should have a path to the Python destination via the serial link"
    );

    // ---- Direction 2: our lnsd announces, Python must learn it. ----
    let identity = Identity::generate(&mut rand_core::OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "interop",
        &[label, "rust"],
    )
    .expect("create Rust destination");
    let dest_hash = *dest.hash();
    node.register_destination(dest);

    let mut python_learned = false;
    for _ in 0..8 {
        node.announce_destination(&dest_hash, Some(b"rust-serial-hello"))
            .await
            .expect("Rust announce");
        for _ in 0..6 {
            if daemon.has_path(&dest_hash).await {
                python_learned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if python_learned {
            break;
        }
    }
    assert!(
        python_learned,
        "Python should have a path to our lnsd's destination via the serial link"
    );
}

/// KISS TNC framing over a socat pty pair against a live Python `KISSInterface`.
#[tokio::test]
#[ignore = "spawns Python rnsd + socat; run with --include-ignored"]
async fn kiss_serial_announce_crosses_live_python() {
    init_tracing();

    let pty = SocatPtyPair::spawn().await.expect("socat pty pair");
    let daemon = TestDaemon::start_with_kiss_serial(&pty.end_a, SERIAL_SPEED)
        .await
        .expect("start Python KISS daemon");

    let storage = temp_storage("kiss_serial", "rust");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage.path().to_path_buf())
        .add_kiss_interface(
            pty.end_b.clone(),
            SERIAL_SPEED,
            8,
            "N".to_string(),
            1,
            false,
        )
        .build()
        .await
        .expect("build lnsd");
    node.start().await.expect("start lnsd");
    node.wait_for_interfaces_ready(Duration::from_secs(10))
        .await
        .expect("KISS interface ready");

    assert_announce_crosses_both_ways(&daemon, &mut node, "kiss").await;

    node.stop().await.ok();
}

/// AX.25 UI-frame KISS framing over a socat pty pair against a live Python
/// `AX25KISSInterface`. The tocall is Python's fixed `APZRNS-0`; both ends set a
/// source callsign/SSID.
#[tokio::test]
#[ignore = "spawns Python rnsd + socat; run with --include-ignored"]
async fn ax25_serial_announce_crosses_live_python() {
    init_tracing();

    let pty = SocatPtyPair::spawn().await.expect("socat pty pair");
    let daemon = TestDaemon::start_with_ax25_serial(&pty.end_a, SERIAL_SPEED, "NOCALL", 1)
        .await
        .expect("start Python AX.25 daemon");

    let storage = temp_storage("ax25_serial", "rust");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage.path().to_path_buf())
        .add_ax25_kiss_interface(
            pty.end_b.clone(),
            "LNODE".to_string(),
            2,
            SERIAL_SPEED,
            8,
            "N".to_string(),
            1,
            false,
        )
        .build()
        .await
        .expect("build lnsd");
    node.start().await.expect("start lnsd");
    node.wait_for_interfaces_ready(Duration::from_secs(10))
        .await
        .expect("AX.25 interface ready");

    assert_announce_crosses_both_ways(&daemon, &mut node, "ax25").await;

    node.stop().await.ok();
}

/// PipeInterface over the `pipe_bridge.py` stdio<->TCP relay against a live
/// Python `PipeInterface`. Pipe is a subprocess bridge, not a serial port, so
/// this uses loopback TCP between the two bridge halves rather than a pty — the
/// documented difference from KISS/AX.25.
#[tokio::test]
#[ignore = "spawns Python rnsd + pipe bridge; run with --include-ignored"]
async fn pipe_announce_crosses_live_python() {
    init_tracing();

    assert!(
        std::path::Path::new(BRIDGE).exists(),
        "pipe bridge helper missing at {BRIDGE}"
    );

    let port = free_tcp_port();
    // Python listens, our lnsd connects: same bridge script, mirrored args.
    let py_command = format!("python3 {BRIDGE} listen {port}");
    let rust_command = format!("python3 {BRIDGE} connect {port}");

    let daemon = TestDaemon::start_with_pipe(&py_command)
        .await
        .expect("start Python pipe daemon");

    let storage = temp_storage("pipe_live", "rust");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage.path().to_path_buf())
        .add_pipe_interface(rust_command, Some(0.2))
        .build()
        .await
        .expect("build lnsd");
    node.start().await.expect("start lnsd");
    node.wait_for_interfaces_ready(Duration::from_secs(10))
        .await
        .expect("pipe interface ready");

    assert_announce_crosses_both_ways(&daemon, &mut node, "pipe").await;

    node.stop().await.ok();
}
