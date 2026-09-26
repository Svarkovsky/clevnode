//! mvr for the rig's `lora_ratchet_rotation` single-loss: exactly one of ten
//! single packets lost after a ratchet rotation (9/10 in both 2026-08-21 full
//! corpus runs, `corrupt=0`, every transport drop counter zero except
//! `announce_replay`).
//!
//! This reproduces the selftest's `ratchet-rotation` sequence
//! (`leviculum-cli/src/selftest.rs`) with the radio and the daemons removed:
//! two in-process `ReticulumNode`s on TCP loopback, both with ratchet-enabled
//! destinations. Drive: announce + discover, exchange 5 single packets each
//! way (expect 10), let the ratchet interval expire, re-announce (this is what
//! rotates — `Destination::announce` -> `rotate_ratchet_if_needed`,
//! destination.rs:1079), verify both sides rotated, exchange 5 each way again
//! (expect 10).
//!
//! The suspected mechanism is a crypto-window: a post-rotation message
//! encrypted under a key the receiver cannot try, dropped silently in the
//! Single-destination decrypt (node/mod.rs "Dropped packet, decryption
//! failed"). If that window exists at the protocol layer, this test sees it
//! as `post_recv < 10` and the event timeline names the lost seq. If it only
//! opens under radio timing (announce still on the air when the exchange
//! starts), this test stays green and the non-reproduction is the result.
//!
//! Every send/receive/announce/rotation is logged as a structured event line
//! (`EVENT key=val t=<ms>`) from both sides; the unified timeline prints on
//! every run.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use leviculum_core::{Destination, DestinationHash, DestinationType, Direction, Identity};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::NodeEvent;

/// Shared timeline + per-side reception sets, guarded by one lock so event
/// lines from both drains interleave in wall order.
struct Timeline {
    start: Instant,
    lines: Mutex<Vec<String>>,
}

impl Timeline {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            lines: Mutex::new(Vec::new()),
        }
    }

    fn push(&self, event: &str, fields: &str) {
        let t = self.start.elapsed().as_millis();
        let line = format!("{event} {fields} t={t}");
        self.lines.lock().unwrap().push(line);
    }

    fn dump(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }
}

fn hex8(key: &Option<[u8; 32]>) -> String {
    match key {
        Some(k) => k[..4].iter().map(|b| format!("{b:02x}")).collect(),
        None => "none".into(),
    }
}

/// Per-side reception ledger: seqs seen for the direction this side expects.
#[derive(Default)]
struct RecvLedger {
    seqs: HashSet<u64>,
}

/// Parse the mvr message format `"<dir> <seq>"`.
fn parse_msg(data: &[u8]) -> Option<(String, u64)> {
    let s = std::str::from_utf8(data).ok()?;
    let mut it = s.split_whitespace();
    let dir = it.next()?.to_string();
    let seq: u64 = it.next()?.parse().ok()?;
    Some((dir, seq))
}

struct RunResult {
    pre_recv: usize,
    post_recv: usize,
    rotated_a: bool,
    rotated_b: bool,
    post_missing: Vec<String>,
    timeline: String,
}

/// One full pre-exchange -> rotation -> post-exchange cycle. Mirrors the
/// selftest sequence with loopback-appropriate times: ratchet interval 1 s
/// (the destination minimum) instead of 5 s, expiry sleep 1.15 s instead of
/// 6 s, announce settle `settle_ms` instead of 2 s. `settle_ms = 0` starts
/// the post-rotation exchange in the same scheduler breath as the
/// re-announces — the widest switching window loopback can express (a sender
/// may then still encrypt under the peer's pre-rotation key, exercising
/// retained-key decrypt end to end).
async fn run_once(settle_ms: u64) -> RunResult {
    let tl = Arc::new(Timeline::new());

    let server_port = crate::harness::port_alloc::free_tcp_port();
    let server_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse().unwrap();

    // A: TCP server. B: TCP client. Transport off on both, like the selftest
    // clients (the daemons between them are transparent to the ratchet
    // protocol, which runs end to end).
    let a_storage = tempfile::tempdir().expect("tempdir A");
    let mut a = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_server(server_addr)
        .storage_path(a_storage.path().to_path_buf())
        .build()
        .await
        .expect("build A");
    a.start().await.expect("start A");

    let b_storage = tempfile::tempdir().expect("tempdir B");
    let mut b = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(server_addr)
        .storage_path(b_storage.path().to_path_buf())
        .build()
        .await
        .expect("build B");
    b.start().await.expect("start B");

    // Ratchet-enabled destinations, exactly as the selftest builds them
    // (enable at now_ms=0, then set the rotation interval; 1000 ms is the
    // destination-enforced minimum).
    let make_dest = |aspect: &str| -> Destination {
        let id = Identity::generate(&mut rand_core::OsRng);
        let mut dest = Destination::new(
            Some(id),
            Direction::In,
            DestinationType::Single,
            "mvr",
            &["rotation", aspect],
        )
        .expect("destination");
        dest.enable_ratchets(&mut rand_core::OsRng, 0)
            .expect("enable ratchets");
        dest.set_ratchet_interval(1000);
        dest
    };

    let dest_a = make_dest("a");
    let dest_b = make_dest("b");
    let hash_a = *dest_a.hash();
    let hash_b = *dest_b.hash();
    a.register_destination(dest_a);
    b.register_destination(dest_b);

    // Event drains: log ANN_RX and SP_RX from both sides, count received seqs.
    let ledger_a = Arc::new(Mutex::new(RecvLedger::default())); // messages "ba" landing at A
    let ledger_b = Arc::new(Mutex::new(RecvLedger::default())); // messages "ab" landing at B
    let a_discovered_b = Arc::new(tokio::sync::Notify::new());
    let b_discovered_a = Arc::new(tokio::sync::Notify::new());

    let mut a_rx = a.take_event_receiver().expect("A event rx");
    let a_drain = {
        let tl = Arc::clone(&tl);
        let ledger = Arc::clone(&ledger_a);
        let notify = Arc::clone(&a_discovered_b);
        tokio::spawn(async move {
            while let Some(ev) = a_rx.recv().await {
                match ev {
                    NodeEvent::AnnounceReceived { announce, .. } => {
                        let dst = *announce.destination_hash();
                        tl.push("ANN_RX", &format!("side=a dst={}", hexh(&dst)));
                        if dst == hash_b {
                            notify.notify_one();
                        }
                    }
                    NodeEvent::PacketReceived { data, .. } => match parse_msg(&data) {
                        Some((dir, seq)) if dir == "ba" => {
                            tl.push("SP_RX", &format!("side=a seq={seq}"));
                            ledger.lock().unwrap().seqs.insert(seq);
                        }
                        other => {
                            tl.push("SP_RX_UNPARSED", &format!("side=a msg={other:?}"));
                        }
                    },
                    _ => {}
                }
            }
        })
    };

    let mut b_rx = b.take_event_receiver().expect("B event rx");
    let b_drain = {
        let tl = Arc::clone(&tl);
        let ledger = Arc::clone(&ledger_b);
        let notify = Arc::clone(&b_discovered_a);
        tokio::spawn(async move {
            while let Some(ev) = b_rx.recv().await {
                match ev {
                    NodeEvent::AnnounceReceived { announce, .. } => {
                        let dst = *announce.destination_hash();
                        tl.push("ANN_RX", &format!("side=b dst={}", hexh(&dst)));
                        if dst == hash_a {
                            notify.notify_one();
                        }
                    }
                    NodeEvent::PacketReceived { data, .. } => match parse_msg(&data) {
                        Some((dir, seq)) if dir == "ab" => {
                            tl.push("SP_RX", &format!("side=b seq={seq}"));
                            ledger.lock().unwrap().seqs.insert(seq);
                        }
                        other => {
                            tl.push("SP_RX_UNPARSED", &format!("side=b msg={other:?}"));
                        }
                    },
                    _ => {}
                }
            }
        })
    };

    // Let the TCP peering settle, then announce both (A first, as the
    // selftest does).
    tokio::time::sleep(Duration::from_millis(400)).await;
    a.announce_destination(&hash_a, Some(b"mvr-rot-a"))
        .await
        .expect("announce A");
    b.announce_destination(&hash_b, Some(b"mvr-rot-b"))
        .await
        .expect("announce B");
    tl.push("ANNOUNCED", "phase=initial");

    // Mutual discovery, bounded.
    let discovery = async {
        tokio::join!(a_discovered_b.notified(), b_discovered_a.notified());
    };
    tokio::time::timeout(Duration::from_secs(5), discovery)
        .await
        .expect("mutual discovery within 5s");
    tl.push("DISCOVERED", "phase=initial");

    let ep_a = a.packet_sender(&hash_b);
    let ep_b = b.packet_sender(&hash_a);

    // Pre-rotation exchange: 5 each way, 200 ms apart, like the selftest.
    for seq in 0..5u64 {
        send_one(&tl, &ep_a, "ab", seq, "a", &a, &hash_b).await;
        send_one(&tl, &ep_b, "ba", seq, "b", &b, &hash_a).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let pre_recv = drain_until(&ledger_a, &ledger_b, 10, Duration::from_secs(2)).await;
    tl.push("PHASE_DONE", &format!("phase=pre recv={pre_recv}"));

    // Capture ratchet keys, let the interval expire, re-announce to rotate.
    let ratchet_before_a = a.destination_ratchet_public(&hash_a);
    let ratchet_before_b = b.destination_ratchet_public(&hash_b);
    tokio::time::sleep(Duration::from_millis(1150)).await;

    a.announce_destination(&hash_a, Some(b"mvr-rot-a"))
        .await
        .expect("re-announce A");
    b.announce_destination(&hash_b, Some(b"mvr-rot-b"))
        .await
        .expect("re-announce B");
    tl.push("ANNOUNCED", "phase=rotation");
    if settle_ms > 0 {
        tokio::time::sleep(Duration::from_millis(settle_ms)).await;
    }

    let ratchet_after_a = a.destination_ratchet_public(&hash_a);
    let ratchet_after_b = b.destination_ratchet_public(&hash_b);
    let rotated_a = ratchet_before_a != ratchet_after_a;
    let rotated_b = ratchet_before_b != ratchet_after_b;
    tl.push(
        "ROTATED",
        &format!(
            "a={rotated_a} b={rotated_b} a_key={}->{} b_key={}->{}",
            hex8(&ratchet_before_a),
            hex8(&ratchet_after_a),
            hex8(&ratchet_before_b),
            hex8(&ratchet_after_b),
        ),
    );

    // Reset ledgers, then the post-rotation exchange: 5 each way at seq
    // 100..105, exactly like the selftest.
    ledger_a.lock().unwrap().seqs.clear();
    ledger_b.lock().unwrap().seqs.clear();
    for seq in 100..105u64 {
        send_one(&tl, &ep_a, "ab", seq, "a", &a, &hash_b).await;
        send_one(&tl, &ep_b, "ba", seq, "b", &b, &hash_a).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let post_recv = drain_until(&ledger_a, &ledger_b, 10, Duration::from_secs(2)).await;
    tl.push("PHASE_DONE", &format!("phase=post recv={post_recv}"));

    // Name what is missing, from the ledgers, not from reasoning.
    let mut post_missing = Vec::new();
    {
        let la = ledger_a.lock().unwrap();
        let lb = ledger_b.lock().unwrap();
        for seq in 100..105u64 {
            if !lb.seqs.contains(&seq) {
                post_missing.push(format!("ab {seq}"));
            }
            if !la.seqs.contains(&seq) {
                post_missing.push(format!("ba {seq}"));
            }
        }
    }

    a_drain.abort();
    b_drain.abort();
    let _ = a.stop().await;
    let _ = b.stop().await;

    RunResult {
        pre_recv,
        post_recv,
        rotated_a,
        rotated_b,
        post_missing,
        timeline: tl.dump(),
    }
}

fn hexh(h: &DestinationHash) -> String {
    h.as_bytes()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Send one single packet and log SP_TX with the ratchet key the sender
/// currently knows for the peer (the key this message is encrypted under).
async fn send_one(
    tl: &Timeline,
    ep: &leviculum_std::driver::PacketSender,
    dir: &str,
    seq: u64,
    side: &str,
    node: &leviculum_std::driver::ReticulumNode,
    peer: &DestinationHash,
) {
    let known = node.known_remote_ratchet(peer);
    let msg = format!("{dir} {seq}");
    match ep.send(msg.as_bytes()).await {
        Ok(_) => tl.push(
            "SP_TX",
            &format!("side={side} seq={seq} peer_ratchet={}", hex8(&known)),
        ),
        Err(e) => tl.push("SP_TX_FAIL", &format!("side={side} seq={seq} err={e:?}")),
    }
}

/// Wait until both ledgers together hold `expected` seqs, or the deadline.
async fn drain_until(
    ledger_a: &Arc<Mutex<RecvLedger>>,
    ledger_b: &Arc<Mutex<RecvLedger>>,
    expected: usize,
    budget: Duration,
) -> usize {
    let deadline = Instant::now() + budget;
    loop {
        let n = ledger_a.lock().unwrap().seqs.len() + ledger_b.lock().unwrap().seqs.len();
        if n >= expected || Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_n(settle_ms: u64) {
    let n: usize = std::env::var("ROTMVR_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    for iter in 0..n {
        let r = run_once(settle_ms).await;
        println!("--- ROTMVR iter={iter} timeline ---\n{}", r.timeline);
        println!(
            "ROTMVR_SUMMARY iter={iter} pre_recv={} post_recv={} rotated_a={} rotated_b={} missing={:?}",
            r.pre_recv, r.post_recv, r.rotated_a, r.rotated_b, r.post_missing
        );

        assert_eq!(
            r.pre_recv, 10,
            "scaffold control: pre-rotation exchange must deliver 10/10 over \
             loopback (iter {iter});\n{}",
            r.timeline
        );
        assert!(
            r.rotated_a && r.rotated_b,
            "scaffold control: re-announce after interval expiry must rotate \
             both ratchets (iter {iter}, a={} b={});\n{}",
            r.rotated_a,
            r.rotated_b,
            r.timeline
        );
        assert_eq!(
            r.post_recv, 10,
            "post-rotation single-packet loss reproduced off-radio (iter {iter}): \
             missing {:?};\n{}",
            r.post_missing, r.timeline
        );
    }
}

/// The mvr: the full selftest rotation sequence over loopback must deliver
/// 10/10 both before and after the rotation. `ROTMVR_N` repeats the cycle
/// (investigation runs use 20); default is one cycle to stay inside the
/// tier-1 time budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ratchet_rotation_delivers_all_singles() {
    run_n(400).await;
}

/// Same cycle with NO settle between the re-announces and the post-rotation
/// exchange: the first sends race the announce processing, so a sender can
/// still hold the peer's pre-rotation key. Delivery must still be 10/10 —
/// the receiver retains rotated-out keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ratchet_rotation_immediate_exchange_delivers_all_singles() {
    run_n(0).await;
}
