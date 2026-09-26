//! mvr for L-0020 — `iface_online_map` only ever learns `true`.
//!
//! The driver inserts `online = true` when an interface registers and no
//! code path ever records `false`, so `rnstatus` reports an interface as
//! online forever, even after its carrier died. Python-RNS flips
//! `Interface.online` at the connect boundaries (TCPInterface.py sets
//! `self.online = False` on `teardown()` and back to `True` on
//! reconnect), so a dead client interface shows `Down` there.
//!
//! One node, one TCP client, no Python, < 5 s in the green case: the peer
//! goes away, the client's reconnect wrapper loops on refused connects,
//! and the status snapshot must say so.
//!
//! **Acceptance**: red on master 28de836 (snapshot stays `online: true`
//! after the peer is gone), green once the interface task's connect
//! boundaries drive the reported online state.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use leviculum_std::driver::ReticulumNodeBuilder;

fn next_port() -> u16 {
    crate::harness::port_alloc::free_tcp_port()
}

/// Poll `cond` every 100 ms until it returns true or the deadline passes.
async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    cond()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_client_reports_offline_after_peer_is_gone() {
    let port = next_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    // Peer: a plain TCP server node.
    let b_storage = tempfile::tempdir().expect("tempdir b");
    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_server(addr)
        .storage_path(b_storage.path().to_path_buf())
        .build()
        .await
        .expect("build b");
    node_b.start().await.expect("start b");

    // Node under test: a TCP client to the peer.
    let a_storage = tempfile::tempdir().expect("tempdir a");
    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(addr)
        .storage_path(a_storage.path().to_path_buf())
        .build()
        .await
        .expect("build a");
    node_a.start().await.expect("start a");

    // The ready signal fires on TcpStream::connect Ok, so after this the
    // client was genuinely connected once (not still in its first attempt).
    node_a
        .wait_for_interface_ready(0, Duration::from_secs(10))
        .await
        .expect("tcp client never connected");

    let connected = wait_until(Duration::from_secs(10), || {
        node_a
            .interface_stats()
            .iter()
            .any(|i| !i.is_local_client && i.online)
    })
    .await;
    assert!(connected, "connected client must report online");

    // Kill the peer. The client's reconnect wrapper keeps the interface
    // registered while it retries against the closed port.
    node_b.stop().await.expect("stop b");
    drop(node_b);

    let reported_offline = wait_until(Duration::from_secs(10), || {
        node_a
            .interface_stats()
            .iter()
            .any(|i| !i.is_local_client && !i.online)
    })
    .await;

    let snapshot: Vec<(String, bool)> = node_a
        .interface_stats()
        .iter()
        .filter(|i| !i.is_local_client)
        .map(|i| (i.name.clone(), i.online))
        .collect();
    node_a.stop().await.expect("stop a");

    assert!(
        reported_offline,
        "L-0020: peer is gone but the client interface still reports online: {snapshot:?}"
    );
}
