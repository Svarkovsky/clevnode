//! Codeberg #38: the FIX proven end to end — a Python client and a leviculum-std
//! client BOTH establish the link through our relay on the identical hop-asymmetry
//! topology that previously timed the Python client out.
//!
//! ## Mechanism (reference-confirmed against vendored Python)
//!
//! The Python **initiator** matches a returning LRPROOF to its pending link ONLY
//! when `packet.hops == link.expected_hops` (`RNS/Transport.py:2226`, unless
//! `expected_hops == PATHFINDER_M`, i.e. the path was unknown at link creation).
//! `expected_hops` is taken from the initiator's path table at `create_link`
//! time. If that path is UNDERCOUNTED (optimistic, fewer hops than the live
//! round trip), the proof would return with `packet.hops > expected_hops`, no
//! pending link would match, `validate_proof` would never be called, and
//! `create_link` would time out.
//!
//! The #38 fix in our relay `transport.rs` REWRITES the forwarded LRPROOF's
//! `packet.hops` down to the frozen `remaining_hops` on the mismatch branch
//! (Python `Transport.py:2176` requires this equality). So the proof now reaches
//! the initiator carrying the hop count it expects: the strict Python initiator
//! matches its pending link and establishes, exactly as our own leviculum-std
//! initiator always did. The relay still DETECTS and logs the asymmetry
//! (`LRPROOF hop asymmetry: rewriting forwarded hops to the frozen count (remaining_hops)`);
//! it does not pretend the path is symmetric. Same relay, same page: both clients establish.
//!
//! ## Topology
//!
//! ```text
//!   initiator ──tcp──> relay-1 ──tcp──> [SHIM] ──tcp──> relay-2 ──tcp──> relay-3 ──tcp──> Python responder
//!   (Python A, or       (our lnsd,                       (our lnsd,       (our lnsd,        (TestDaemon B,
//!    Rust control)       transport on)                   transport on)    transport on)     link-accepting)
//! ```
//!
//! The UNDERCOUNT SHIM is an in-test TCP forwarder. It copies bytes verbatim in
//! the initiator→responder direction; in the responder→initiator direction it
//! DECREMENTS the hop-count byte of every ANNOUNCE frame by one (the hop byte is
//! packet byte[1], outside the Ed25519-signed announce payload — #38 STEP-0 Q4),
//! leaving the signature valid. Effect: relay-1 (and the initiator behind it)
//! learn the responder one hop too close: the true chain relay-1..B is 3 hops,
//! relay-1 stores 2. Two hops still need a relay, so relay-1 forwards the link
//! request DESIGNATED (Type2, transport_id = relay-2) — with the overheard-
//! link-request gate (Python Transport.py:1559) an undercounted "direct"
//! (1-hop) view would instead strand the request at relay-2, on our relay and
//! on a real rnsd alike, before any proof exists. When the initiator links to
//! the responder, relay-1 freezes `remaining_hops = 2`; the LRPROOF returns
//! over the true 3-hop path; relay-1 logs the asymmetry and forwards anyway
//! with the hop count rewritten to the frozen 2, so the strict Python
//! initiator (expected_hops = 3 after its own increment) accepts it.
//!
//! ## Running
//!
//! ```sh
//! cargo test -p leviculum-std --test rnsd_interop \
//!   lrproof_hop_undercount -- --nocapture --test-threads=1
//! ```

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};
use leviculum_core::DestinationHash;
use leviculum_std::driver::{ReticulumNode, ReticulumNodeBuilder};
use leviculum_std::test_support::warn_capture::register_warn_capture;

use crate::common::{
    temp_storage, wait_for_link_established, wait_for_path_on_node, wait_for_path_reannounce,
    wait_for_path_reannounce_on_daemon,
};
use crate::harness::{pick_free_tcp_port, TestDaemon};

// ----------------------------------------------------------------------------
// WARN capture for relay-1's asymmetry line.
//
// This observes the relay's plain `tracing::warn!` "LRPROOF hop asymmetry:
// rewriting forwarded hops to the frozen count" message. It hooks the harness's ONE global subscriber via
// `register_warn_capture` (an active-handles capture layer) rather than a
// private `set_global_default`. The private-subscriber approach was a race: any
// test that calls `common::init_tracing()` installs the global subscriber first
// under the parallel full suite, so the private one never took effect and the
// buffer stayed empty — assertion (b) then failed under load while passing in
// isolation. The global-subscriber hook captures regardless of test ordering.

// ----------------------------------------------------------------------------
// The undercount TCP shim.
// ----------------------------------------------------------------------------

/// A packet is an ANNOUNCE with no IFAC signature when byte[0]'s low two bits are
/// `0b01` (PacketType::Announce) and the IFAC bit (0x80) is clear.
fn is_plain_announce(pkt: &[u8]) -> bool {
    pkt.len() >= 2 && (pkt[0] & 0x80) == 0 && (pkt[0] & 0x03) == 0x01
}

/// Copy `reader` → `writer` verbatim (initiator → responder direction).
async fn copy_verbatim<R, W>(mut reader: R, mut writer: W)
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if writer.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = writer.shutdown().await;
}

/// Copy `reader` → `writer` (responder → initiator direction), HDLC-deframing the
/// stream and decrementing the hop byte of each ANNOUNCE frame before re-framing.
async fn copy_undercount_announces<R, W>(mut reader: R, mut writer: W, mangled: Arc<AtomicU64>)
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut deframer = Deframer::new();
    let mut chunk = [0u8; 4096];
    let mut out = Vec::with_capacity(600);
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for result in deframer.process(&chunk[..n]) {
            let DeframeResult::Frame(mut pkt) = result else {
                continue;
            };
            if is_plain_announce(&pkt) && pkt[1] > 0 {
                pkt[1] -= 1; // hop byte, outside the announce signature
                mangled.fetch_add(1, Ordering::Relaxed);
            }
            frame(&pkt, &mut out);
            if writer.write_all(&out).await.is_err() {
                let _ = writer.shutdown().await;
                return;
            }
        }
    }
    let _ = writer.shutdown().await;
}

/// Spawn the undercount shim. Returns its listen address and the counter of
/// mangled announces. relay-1 dials the shim; the shim dials relay-2.
async fn spawn_undercount_shim(relay2_addr: SocketAddr) -> (SocketAddr, Arc<AtomicU64>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind shim");
    let shim_addr = listener.local_addr().expect("shim local addr");
    let mangled = Arc::new(AtomicU64::new(0));
    let mangled_ret = mangled.clone();

    tokio::spawn(async move {
        loop {
            let (inbound, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            let upstream = match TcpStream::connect(relay2_addr).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let (in_r, in_w) = inbound.into_split();
            let (up_r, up_w) = upstream.into_split();
            let mangled = mangled.clone();
            // initiator(relay-1) → responder(relay-2): verbatim.
            tokio::spawn(copy_verbatim(in_r, up_w));
            // responder(relay-2) → initiator(relay-1): undercount announces.
            tokio::spawn(copy_undercount_announces(up_r, in_w, mangled));
        }
    });

    (shim_addr, mangled_ret)
}

// ----------------------------------------------------------------------------
// Shared topology: three Rust relays with the undercount shim between the
// first two, a Python responder B behind relay-3, relay-1 having learned B
// UNDERCOUNTED (2 hops stored, 3 hops true).
// ----------------------------------------------------------------------------

/// A running hop-undercount relay chain, ready for an initiator to link through
/// relay-1 to the Python responder B. Callers must keep this alive for the whole
/// test (dropping it stops the relays and deletes their storage).
struct UndercountChain {
    responder: TestDaemon,
    dest_b_hash: String,
    dest_b_public_key: String,
    dest_b_hash_typed: DestinationHash,
    dest_b_signing_key: [u8; 32],
    relay1: ReticulumNode,
    relay2: ReticulumNode,
    relay3: ReticulumNode,
    relay1_addr: SocketAddr,
    relay1_port: u16,
    _relay1_storage: tempfile::TempDir,
    _relay2_storage: tempfile::TempDir,
    _relay3_storage: tempfile::TempDir,
}

/// Build and start the shared topology and drive B's announce down the chain
/// until relay-1 has learned an UNDERCOUNTED path to B. Panics on any setup
/// failure. Same chain used by both the establish and the data-transfer tests.
async fn setup_undercount_chain() -> UndercountChain {
    // --- Ports for the three Rust relay servers (the shim binds :0 itself). ---
    let relay1_port = pick_free_tcp_port().expect("relay-1 port");
    let relay2_port = pick_free_tcp_port().expect("relay-2 port");
    let relay3_port = pick_free_tcp_port().expect("relay-3 port");
    let relay1_addr: SocketAddr = format!("127.0.0.1:{relay1_port}").parse().unwrap();
    let relay2_addr: SocketAddr = format!("127.0.0.1:{relay2_port}").parse().unwrap();
    let relay3_addr: SocketAddr = format!("127.0.0.1:{relay3_port}").parse().unwrap();

    // --- Python responder B (link-accepting destination). ---
    let responder = TestDaemon::start().await.expect("start Python responder B");
    let dest_b = responder
        .register_destination("mvr38", &["responder"])
        .await
        .expect("register responder dest");
    responder
        .set_proof_strategy(&dest_b.hash, "PROVE_ALL")
        .await
        .expect("responder PROVE_ALL");

    let dest_b_hash_bytes: [u8; 16] = hex::decode(&dest_b.hash).unwrap().try_into().unwrap();
    let dest_b_hash_typed = DestinationHash::new(dest_b_hash_bytes);
    // Ed25519 signing (verifying) key = second half of the 64-byte public key.
    let dest_b_key_bytes = hex::decode(&dest_b.public_key).unwrap();
    let mut dest_b_signing_key = [0u8; 32];
    dest_b_signing_key.copy_from_slice(&dest_b_key_bytes[32..64]);

    // --- relay-3: transport on, TCP server for relay-2, TCP client to B. ---
    let _relay3_storage = temp_storage("lrproof_hop_undercount", "relay3");
    let mut relay3 = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_server(relay3_addr)
        .add_tcp_client(responder.rns_addr())
        .storage_path(_relay3_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay-3");
    relay3.start().await.expect("start relay-3");

    // --- relay-2: transport on, TCP server for the shim, TCP client to
    //     relay-3 (lengthens the true chain to 3 hops from relay-1, so the
    //     undercounted view at relay-1 is still a 2-hop RELAYED path and the
    //     link request stays designated at every hop). ---
    let _relay2_storage = temp_storage("lrproof_hop_undercount", "relay2");
    let mut relay2 = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_server(relay2_addr)
        .add_tcp_client(relay3_addr)
        .storage_path(_relay2_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay-2");
    relay2.start().await.expect("start relay-2");

    // --- The undercount shim between relay-1 and relay-2. ---
    let (shim_addr, mangled) = spawn_undercount_shim(relay2_addr).await;

    // --- relay-1 ("our lnsd"): transport on, TCP server for the initiator,
    //     TCP client to the shim. ---
    let _relay1_storage = temp_storage("lrproof_hop_undercount", "relay1");
    let mut relay1 = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_server(relay1_addr)
        .add_tcp_client(shim_addr)
        .storage_path(_relay1_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay-1");
    relay1.start().await.expect("start relay-1");

    // Let the relay chain's TCP links come up.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Drive B's announce down the chain; relay-1 must learn B UNDERCOUNTED.
    responder
        .announce_destination(&dest_b.hash, b"mvr38-responder")
        .await
        .expect("announce B");
    let relay1_has_b =
        wait_for_path_on_node(&relay1, &dest_b_hash_typed, Duration::from_secs(30)).await;
    assert!(
        relay1_has_b,
        "relay-1 must learn a path to the responder within 30s"
    );

    // Sanity: the shim actually mangled ≥1 announce, and relay-1's learned path
    // is the undercounted value (fewer hops than the true 2-hop chain).
    assert!(
        mangled.load(Ordering::Relaxed) >= 1,
        "the shim must have decremented at least one announce hop byte"
    );
    let relay1_hops_to_b = relay1
        .path_table_entries()
        .into_iter()
        .find(|e| e.hash == dest_b_hash_bytes)
        .map(|e| e.hops);
    // True chain relay-1..B is 3 transport hops; the undercount makes relay-1
    // store 2. (We assert 1 < h < 3 rather than == 2: still a RELAYED path —
    // a 1-hop view would strand the designated forward — while staying robust
    // to re-announce ordering.)
    assert!(
        matches!(relay1_hops_to_b, Some(h) if h > 1 && h < 3),
        "relay-1's path to B must be undercounted but still relayed (2 hops); \
         got {relay1_hops_to_b:?}"
    );

    UndercountChain {
        responder,
        dest_b_hash: dest_b.hash,
        dest_b_public_key: dest_b.public_key,
        dest_b_hash_typed,
        dest_b_signing_key,
        relay1,
        relay2,
        relay3,
        relay1_addr,
        relay1_port,
        _relay1_storage,
        _relay2_storage,
        _relay3_storage,
    }
}

// ----------------------------------------------------------------------------
// The reproduction.
// ----------------------------------------------------------------------------

/// Full #38 fix proof: Python initiator AND Rust initiator both establish the
/// link on the identical hop-undercount relay chain that previously timed the
/// Python client out.
#[tokio::test]
async fn lrproof_hop_undercount_python_client_and_rust_client_both_establish() {
    let warn_capture = register_warn_capture();

    let UndercountChain {
        responder,
        dest_b_hash: dest_b_hash_str,
        dest_b_public_key,
        dest_b_hash_typed: dest_b_hash,
        dest_b_signing_key,
        mut relay1,
        mut relay2,
        mut relay3,
        relay1_addr,
        relay1_port,
        ..
    } = setup_undercount_chain().await;

    // ========================================================================
    // (A) PYTHON initiator: create_link must now ESTABLISH (no timeout).
    // ========================================================================
    let initiator_py = TestDaemon::start().await.expect("start Python initiator A");
    initiator_py
        .add_client_interface("127.0.0.1", relay1_port, Some("to-relay1"))
        .await
        .expect("Python A dials relay-1");

    // A must learn a (likewise undercounted) path to B, so create_link freezes a
    // concrete expected_hops (not PATHFINDER_M, which would bypass the check).
    let py_has_b = wait_for_path_reannounce_on_daemon(
        &initiator_py,
        &dest_b_hash,
        &responder,
        &dest_b_hash_str,
        b"mvr38-responder",
        Duration::from_secs(30),
    )
    .await;
    assert!(
        py_has_b,
        "Python initiator A must learn a path to B before create_link"
    );

    // create_link establishes in ~100ms even under full-suite CPU contention
    // (measured); 30s is generous headroom for slower CI, matching the heaviest
    // sibling relay test (test_diamond_relay_and_failure_recovery).
    let py_link = initiator_py
        .create_link(&dest_b_hash_str, &dest_b_public_key, 30)
        .await;
    let logs = warn_capture.snapshot();
    assert!(
        py_link.is_ok(),
        "FIX (a): the Python initiator's create_link must now ESTABLISH on the \
         hop asymmetry (our relay rewrites the forwarded LRPROOF hops to the \
         frozen remaining_hops, satisfying Transport.py:2176), but it returned \
         {py_link:?}.\n--- relay logs ---\n{logs}"
    );

    // ========================================================================
    // (B) our relay logged the asymmetry-forward.
    // ========================================================================
    assert!(
        logs.contains("LRPROOF hop asymmetry: rewriting forwarded hops to the frozen count"),
        "REPRODUCTION (b): relay-1 must log the LRPROOF hop asymmetry \
         (rewriting forwarded hops to the frozen count).\n--- relay logs ---\n{logs}"
    );

    // ========================================================================
    // (C) a leviculum-std initiator on the SAME running relays also establishes
    //     the link — post-fix both clients establish (previously only this one).
    // ========================================================================
    let _initiator_rs_storage = temp_storage("lrproof_hop_undercount", "initiator_rs");
    let mut initiator_rs = ReticulumNodeBuilder::new()
        .add_tcp_client(relay1_addr)
        .storage_path(_initiator_rs_storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust initiator");
    let mut events_rs = initiator_rs.take_event_receiver().unwrap();
    initiator_rs.start().await.expect("start Rust initiator");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let rs_has_b = wait_for_path_reannounce(
        || initiator_rs.has_path(&dest_b_hash),
        &responder,
        &dest_b_hash_str,
        b"mvr38-responder",
        Duration::from_secs(30),
    )
    .await;
    assert!(
        rs_has_b,
        "Rust control initiator must learn a path to B before connect"
    );

    let handle = initiator_rs
        .connect(&dest_b_hash, &dest_b_signing_key)
        .await
        .expect("Rust initiator connect");
    let established =
        wait_for_link_established(&mut events_rs, handle.link_id(), Duration::from_secs(20)).await;
    let logs = warn_capture.snapshot();
    assert!(
        established,
        "FIX (c): the leviculum-std initiator must ESTABLISH the link on the \
         identical hop-undercount chain. Post-fix both the Python initiator (a) \
         and this leviculum-std initiator establish: same relay, both clients \
         accept the rewritten proof.\n--- relay logs ---\n{logs}"
    );

    // Tidy up async nodes.
    let _ = initiator_rs.stop().await;
    let _ = relay1.stop().await;
    let _ = relay2.stop().await;
    let _ = relay3.stop().await;
}

/// #38 completion: the link established over the hop-undercount asymmetry must
/// actually CARRY DATA both ways (the user's real goal is loading a NomadNet
/// page = establish + transfer). A Python initiator links to the Python
/// responder B through our two relays, then sends a payload and receives B's
/// echo. Both traverse the relay link-DATA forward-anyway site
/// (transport.rs, "Link data hop asymmetry, forwarding anyway"), the mirror of
/// the LRPROOF site fixed in 5d0833d.
///
/// Note on scope: Python's strict per-hop check (Transport.py:1656) gates only
/// LINK-DATA that Python *relays* (destination in its link_table). Here the
/// Python nodes are the link ENDPOINTS, so they deliver local link data without
/// that check; the only relays are our Rust nodes. This test therefore proves
/// end-to-end data delivery over the asymmetric link; a Python-relay-in-the-
/// middle drop would need a different topology.
#[tokio::test]
async fn lrproof_hop_undercount_data_transfer_both_ways() {
    let warn_capture = register_warn_capture();

    let UndercountChain {
        responder,
        dest_b_hash: dest_b_hash_str,
        dest_b_public_key,
        dest_b_hash_typed: dest_b_hash,
        mut relay1,
        mut relay2,
        mut relay3,
        relay1_port,
        ..
    } = setup_undercount_chain().await;

    // --- Python initiator A links to B through the asymmetric relay chain. ---
    let initiator_py = TestDaemon::start().await.expect("start Python initiator A");
    initiator_py
        .add_client_interface("127.0.0.1", relay1_port, Some("to-relay1"))
        .await
        .expect("Python A dials relay-1");

    let py_has_b = wait_for_path_reannounce_on_daemon(
        &initiator_py,
        &dest_b_hash,
        &responder,
        &dest_b_hash_str,
        b"mvr38-responder",
        Duration::from_secs(30),
    )
    .await;
    assert!(
        py_has_b,
        "Python initiator A must learn a path to B before create_link"
    );

    // 30s: generous headroom (establishes in ~100ms even under full-suite load).
    let link_hash = initiator_py
        .create_link(&dest_b_hash_str, &dest_b_public_key, 30)
        .await
        .expect("Python A create_link must establish over the asymmetric chain");

    // Let both ends settle the link (RTT/keepalive) before pushing data.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ========================================================================
    // A -> B: the initiator sends a payload over the link; B records it (and
    // its _on_packet echoes it straight back).
    // ========================================================================
    let payload = b"nomadnet page fetch over the asymmetric link";
    initiator_py
        .send_on_link(&link_hash, payload)
        .await
        .expect("Python A send_on_link");

    // B must RECEIVE the payload (A -> B direction through the relay chain).
    let mut b_got = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let recv = responder
            .get_received_packets()
            .await
            .expect("responder get_received_packets");
        if recv.iter().any(|p| p.data == payload) {
            b_got = true;
            break;
        }
    }
    let logs = warn_capture.snapshot();
    assert!(
        b_got,
        "A->B: responder B must receive the payload sent over the asymmetric \
         link (relay forwards link data despite the hop mismatch).\n\
         --- relay logs ---\n{logs}"
    );

    // ========================================================================
    // B -> A: A must RECEIVE B's echo of the same payload (return direction
    // through the relay chain). A's create_link packet callback records it.
    // ========================================================================
    let mut a_got_echo = false;
    for _ in 0..40 {
        let recv = initiator_py
            .get_received_packets()
            .await
            .expect("initiator get_received_packets");
        if recv.iter().any(|p| p.data == payload) {
            a_got_echo = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let logs = warn_capture.snapshot();
    assert!(
        a_got_echo,
        "B->A: Python initiator A must receive B's echo over the asymmetric \
         link (relay forwards the return link data despite the hop \
         mismatch).\n--- relay logs ---\n{logs}"
    );

    // Tidy up async nodes.
    let _ = relay1.stop().await;
    let _ = relay2.stop().await;
    let _ = relay3.stop().await;
}
