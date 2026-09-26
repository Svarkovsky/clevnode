//! Contract pin for Codeberg #221: TCP test ports have no probe-then-rebind
//! window.
//!
//! Before the #221 fix this file pinned the defect the other way around:
//! `support::free_port()` bound `:0`, read the port, dropped the listener and
//! returned the number; this test then played the co-tenant, took the port
//! inside the window, and asserted the node's start failed with exactly the
//! tier-2 nightly's `start failed: I/O error: Address in use (os error 98)`.
//! That reproduction is now impossible to set up — `free_port()` is gone —
//! so the test asserts the replacement contract instead:
//!
//! The node binds `127.0.0.1:0` itself and the harness reads the
//! kernel-assigned port back (`lev_tcp_listen_addr`). The port number never
//! exists unbound: from the instant the kernel picks it, the node's listener
//! holds it, so there is no window in which a co-tenant can take it.

mod support;

use std::net::{TcpListener, TcpStream};

use leviculum::*;
use support::{start_tcp_server_node, tcp_listen_port};

#[test]
fn binding_port_zero_reports_the_port_and_leaves_no_window() {
    let dir = tempfile::tempdir().unwrap();
    let (node, addr) = start_tcp_server_node(dir.path(), |_b| {});
    let port = tcp_listen_port(&node, 0);
    assert_ne!(port, 0, "the reported port is the kernel-assigned one");
    assert_eq!(addr, format!("127.0.0.1:{port}"));

    // The old co-tenant move, replayed against the new contract: by the time
    // the harness learns the number, the node already holds it, so the
    // co-tenant loses — the inversion of the pre-fix window.
    assert!(
        TcpListener::bind(("127.0.0.1", port)).is_err(),
        "no co-tenant can bind the reported port; the node holds it from birth"
    );

    // And the reported port is genuinely the node's listener: a client
    // reaches it.
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "the reported port accepts connections"
    );

    // Out-of-range listener indices are a clean error, not junk.
    let mut len = 0usize;
    let rc = unsafe { lev_tcp_listen_addr(node.0, 1, std::ptr::null_mut(), 0, &mut len) };
    assert_eq!(rc, LEV_ERR_INVALID_ARG, "index 1 is out of range");
}
