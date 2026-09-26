//! mvr for Bug #29 — TCP iface holds stale path after silent-then-resume.
//!
//! Isolates the failure shape from the Docker integ test
//! `executor::tests::link_failure_recovery` into a deterministic two-
//! node TCP-loopback scenario. The integ test fails ~14 % of the time
//! because, after iptables silently drops then restores the alice ↔ bob
//! direct link, alice's path table still holds the cached entry that
//! was valid before the block; nothing on the iface side notices the
//! silent-then-resume pattern, and bob's next periodic announce on the
//! direct iface does not arrive within the 50 s post-restore budget.
//!
//! The 3-node Docker scenario uses a python relay so an alternative
//! 2-hop path installs during the block. The 2-node mvr here drops
//! that complication: with one direct link only, "the iface holds a
//! stale path" still applies, and the failure surface tightens to
//! "alice receives no fresh announce from bob within wait_s of
//! `PROXY_BLOCK_END`". That is exactly H1's distinguishing surface.
//!
//! The proxy between alice and bob accepts both TCP connections,
//! forwards bytes during normal operation, and on a runner-controlled
//! gate **silently discards** all bytes in both directions without
//! closing either socket. This mimics iptables `-A DROP`'s effect
//! at the iface layer (the socket stays open, the iface sees no
//! disconnect, traffic just stops) closely enough for the H1
//! distinguishing experiment, even though TCP-level ACK semantics
//! differ from kernel-DROP.
//!
//! Test layout:
//!   * `silent_resume_with_forced_announce` — H1's distinguishing
//!     experiment as a passing test on master. Forces a fresh announce
//!     from bob immediately after `PROXY_BLOCK_END` and asserts alice
//!     receives it within the wait window. Stays green by default.
//!   * `silent_resume_baseline` — `#[ignore]`. Same scenario without
//!     the forced announce. Fails on master because no mechanism
//!     causes a fresh announce. Becomes green once H1's iface-level
//!     fix lands; un-ignore at that point.
//!   * `silent_resume_param_sweep` — `#[ignore]`. Runs N=20 reps of
//!     each (block_s, wait_s) point with and without the forced
//!     announce. Prints structured `SWEEP …` lines for the report.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener as StdTcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use leviculum_core::{Destination, DestinationHash, DestinationType, Direction, Identity};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::NodeEvent;

/// Listener port from the host-wide allocator shared by every suite
/// (`crate::harness::port_alloc`). This file used to carry its own
/// `AtomicU16` at a private base, which is why the bases were chosen
/// disjoint; one counter per host does not need disjoint bases and, unlike
/// a per-process one, does not hand the same number to a concurrently
/// running test binary.
fn next_port() -> u16 {
    crate::harness::port_alloc::free_tcp_port()
}

/// Runner-side proxy controls. `blocked` toggles silent-drop mode;
/// `stop` shuts down all proxy threads on test teardown.
struct ProxyCtrl {
    blocked: AtomicBool,
    stop: AtomicBool,
}

impl ProxyCtrl {
    fn new() -> Self {
        Self {
            blocked: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }
}

/// One half of the bidirectional copy. Reads `src`, drops bytes when
/// `ctrl.blocked` is set, otherwise writes through to `dst`. Reading
/// continues during the block so the source kernel's TCP send buffer
/// doesn't fill and trigger zero-window — that would be a different
/// failure mode than iptables-DROP.
fn copy_with_gate(mut src: TcpStream, mut dst: TcpStream, ctrl: Arc<ProxyCtrl>) {
    src.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let mut buf = [0u8; 4096];
    while !ctrl.stop.load(Ordering::Relaxed) {
        match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if ctrl.blocked.load(Ordering::Relaxed) {
                    continue;
                }
                if dst.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue;
            }
            Err(_) => break,
        }
    }
    let _ = src.shutdown(Shutdown::Read);
    let _ = dst.shutdown(Shutdown::Write);
}

/// TCP proxy listening on `listen_port`, forwarding to
/// `127.0.0.1:upstream_port`. One accept = one bidirectional pair of
/// `copy_with_gate` threads sharing the same `ctrl`.
fn spawn_blocking_proxy(
    listen_port: u16,
    upstream_port: u16,
    ctrl: Arc<ProxyCtrl>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let listener = match StdTcpListener::bind(("127.0.0.1", listen_port)) {
            Ok(l) => l,
            Err(e) => panic!("blocking proxy bind {listen_port} failed: {e}"),
        };
        listener
            .set_nonblocking(true)
            .expect("set proxy nonblocking");
        while !ctrl.stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((client, _addr)) => {
                    let upstream = match TcpStream::connect(("127.0.0.1", upstream_port)) {
                        Ok(s) => s,
                        Err(_) => {
                            thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                    };
                    client.set_nodelay(true).ok();
                    upstream.set_nodelay(true).ok();
                    let c2u_ctrl = Arc::clone(&ctrl);
                    let u2c_ctrl = Arc::clone(&ctrl);
                    let client_r = client.try_clone().unwrap();
                    let upstream_r = upstream.try_clone().unwrap();
                    thread::spawn(move || copy_with_gate(client_r, upstream, c2u_ctrl));
                    thread::spawn(move || copy_with_gate(upstream_r, client, u2c_ctrl));
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
    })
}

#[derive(Clone, Copy)]
struct ScenarioOpts {
    block_s: u64,
    wait_s: u64,
    /// If true, the runner forces a fresh announce from bob immediately
    /// after `PROXY_BLOCK_END`. This is H1's distinguishing experiment.
    force_announce_after_restore: bool,
}

#[derive(Default)]
struct ScenarioResult {
    /// Did the test pass — did alice receive ≥ 1 fresh announce for
    /// bob in the post-restore window?
    passed: bool,
    /// Structured event log (one entry per line) for the report.
    events: Vec<String>,
}

fn structured_event<K: AsRef<str>>(name: &str, t0: Instant, pairs: &[(K, String)]) -> String {
    let t_ms = t0.elapsed().as_millis();
    let body = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k.as_ref(), v))
        .collect::<Vec<_>>()
        .join(" ");
    if body.is_empty() {
        format!("{name} t={t_ms}")
    } else {
        format!("{name} {body} t={t_ms}")
    }
}

async fn run_scenario(opts: ScenarioOpts) -> ScenarioResult {
    let t0 = Instant::now();
    let mut events = Vec::<String>::new();

    let bob_listen_port = next_port();
    let proxy_port = next_port();
    let bob_addr: SocketAddr = format!("127.0.0.1:{bob_listen_port}").parse().unwrap();
    let alice_target: SocketAddr = format!("127.0.0.1:{proxy_port}").parse().unwrap();

    events.push(structured_event(
        "MVR_START",
        t0,
        &[
            ("bob_port", bob_listen_port.to_string()),
            ("proxy_port", proxy_port.to_string()),
            ("block_s", opts.block_s.to_string()),
            ("wait_s", opts.wait_s.to_string()),
            (
                "force_announce",
                opts.force_announce_after_restore.to_string(),
            ),
        ],
    ));

    let proxy_ctrl = Arc::new(ProxyCtrl::new());
    let _proxy = spawn_blocking_proxy(proxy_port, bob_listen_port, Arc::clone(&proxy_ctrl));

    // Bob: server side, holds the destination we will announce.
    let bob_storage = tempfile::tempdir().expect("tempdir bob");
    let mut bob = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_server(bob_addr)
        .storage_path(bob_storage.path().to_path_buf())
        .build()
        .await
        .expect("build bob");
    bob.start().await.expect("start bob");

    // Alice: client connecting *through* the proxy.
    let alice_storage = tempfile::tempdir().expect("tempdir alice");
    let mut alice = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(alice_target)
        .storage_path(alice_storage.path().to_path_buf())
        .build()
        .await
        .expect("build alice");
    alice.start().await.expect("start alice");

    // Drain alice's events into a shared log so we can correlate
    // AnnounceReceived occurrences against PROXY_BLOCK_END.
    let alice_announces: Arc<Mutex<Vec<(Instant, DestinationHash)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let alice_announces_w = Arc::clone(&alice_announces);
    let mut alice_rx = alice.take_event_receiver().expect("alice event rx");
    let drain_handle = tokio::spawn(async move {
        while let Some(ev) = alice_rx.recv().await {
            if let NodeEvent::AnnounceReceived {
                announce,
                interface_index: _,
            } = ev
            {
                let when = Instant::now();
                alice_announces_w
                    .lock()
                    .unwrap()
                    .push((when, *announce.destination_hash()));
            }
        }
    });

    // Bob registers a destination and announces it once. The Single-
    // direction-In, Single type matches the destinations the existing
    // path-install mvrs use.
    let bob_identity = Identity::generate(&mut rand_core::OsRng);
    let bob_dest = Destination::new(
        Some(bob_identity),
        Direction::In,
        DestinationType::Single,
        "mvr",
        &["bug29", "bob"],
    )
    .expect("bob destination");
    let bob_hash = *bob_dest.hash();
    bob.register_destination(bob_dest);

    // Let TCP peering settle through the (open) proxy before the
    // initial announce. 1 s mirrors the path-install mvrs' wire-up
    // wait at finer granularity than the lncp 2 s.
    tokio::time::sleep(Duration::from_secs(1)).await;

    bob.announce_destination(&bob_hash, Some(b"bug29-initial"))
        .await
        .expect("bob initial announce");
    events.push(structured_event(
        "ANN_TX",
        t0,
        &[
            ("node", "bob".into()),
            ("dest", hex::encode(bob_hash.as_bytes())),
            ("ord", "initial".into()),
        ],
    ));

    // Wait for alice's path table to install (via_relay mvr style).
    // Five seconds is generous on host-local TCP.
    let install_deadline = Instant::now() + Duration::from_secs(5);
    let mut path_installed = false;
    while Instant::now() < install_deadline {
        if alice.has_path(&bob_hash) {
            path_installed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    events.push(structured_event(
        "PATH_TABLE_SNAPSHOT",
        t0,
        &[
            ("node", "alice".into()),
            ("dst", hex::encode(bob_hash.as_bytes())),
            (
                "hops",
                alice
                    .hops_to(&bob_hash)
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| "none".into()),
            ),
            ("phase", "post_initial_announce".into()),
        ],
    ));
    if !path_installed {
        // Setup itself is broken; report and bail out. This isn't H1 —
        // it's the test scaffold failing to reach the precondition.
        events.push(structured_event(
            "MVR_SETUP_FAILED",
            t0,
            &[("reason", "alice did not install initial path".into())],
        ));
        proxy_ctrl.stop.store(true, Ordering::Relaxed);
        drain_handle.abort();
        let _ = drain_handle.await;
        let _ = alice.stop().await;
        let _ = bob.stop().await;
        return ScenarioResult {
            passed: false,
            events,
        };
    }

    // PROXY_BLOCK: silent drop in both directions.
    let block_start = Instant::now();
    proxy_ctrl.blocked.store(true, Ordering::Relaxed);
    events.push(structured_event(
        "PROXY_BLOCK_START",
        t0,
        &[("duration_s", opts.block_s.to_string())],
    ));

    // Snapshot the path table while blocked, so the report can show
    // the pre-restore state.
    tokio::time::sleep(Duration::from_secs(opts.block_s)).await;
    events.push(structured_event(
        "PATH_TABLE_SNAPSHOT",
        t0,
        &[
            ("node", "alice".into()),
            ("dst", hex::encode(bob_hash.as_bytes())),
            (
                "hops",
                alice
                    .hops_to(&bob_hash)
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| "none".into()),
            ),
            ("phase", "pre_restore_during_block".into()),
        ],
    ));

    // PROXY_RESUME: traffic flows again.
    proxy_ctrl.blocked.store(false, Ordering::Relaxed);
    let restore_t = Instant::now();
    events.push(structured_event(
        "PROXY_BLOCK_END",
        t0,
        &[(
            "block_actual_ms",
            block_start.elapsed().as_millis().to_string(),
        )],
    ));

    if opts.force_announce_after_restore {
        // H1 distinguishing experiment: force a fresh announce now.
        bob.announce_destination(&bob_hash, Some(b"bug29-forced"))
            .await
            .expect("bob forced announce");
        events.push(structured_event(
            "ANN_TX",
            t0,
            &[
                ("node", "bob".into()),
                ("dest", hex::encode(bob_hash.as_bytes())),
                ("ord", "forced_post_restore".into()),
            ],
        ));
    }

    // Post-restore wait: poll alice's announce log + emit a
    // PATH_TABLE_SNAPSHOT every ~1 s for the structured log.
    let wait_deadline = restore_t + Duration::from_secs(opts.wait_s);
    let mut next_snapshot = restore_t + Duration::from_secs(1);
    while Instant::now() < wait_deadline {
        let snap: Vec<(Instant, DestinationHash)> = alice_announces
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, h)| *t > restore_t && *h == bob_hash)
            .copied()
            .collect();
        let received_post_restore = snap.len();
        if received_post_restore >= 1 && opts.force_announce_after_restore {
            // Forced variant succeeds at first event; no need to wait
            // out the full window once we've seen one.
            break;
        }
        if Instant::now() >= next_snapshot {
            events.push(structured_event(
                "PATH_TABLE_SNAPSHOT",
                t0,
                &[
                    ("node", "alice".into()),
                    ("dst", hex::encode(bob_hash.as_bytes())),
                    (
                        "hops",
                        alice
                            .hops_to(&bob_hash)
                            .map(|h| h.to_string())
                            .unwrap_or_else(|| "none".into()),
                    ),
                    ("phase", "post_restore".into()),
                    ("ann_rx_post_restore", received_post_restore.to_string()),
                ],
            ));
            next_snapshot += Duration::from_secs(1);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Final accounting.
    let post_restore: Vec<(Instant, DestinationHash)> = alice_announces
        .lock()
        .unwrap()
        .iter()
        .filter(|(t, h)| *t > restore_t && *h == bob_hash)
        .copied()
        .collect();
    let received_post_restore = post_restore.len();
    let first_post_restore_ms = post_restore
        .first()
        .map(|(t, _)| t.duration_since(restore_t).as_millis().to_string())
        .unwrap_or_else(|| "none".into());

    let passed = received_post_restore >= 1;
    events.push(structured_event(
        "ASSERT_OUTCOME",
        t0,
        &[
            ("ann_rx_post_restore", received_post_restore.to_string()),
            ("first_post_restore_ms", first_post_restore_ms),
            ("passed", passed.to_string()),
        ],
    ));

    // Teardown.
    proxy_ctrl.stop.store(true, Ordering::Relaxed);
    drain_handle.abort();
    let _ = drain_handle.await;
    let _ = alice.stop().await;
    let _ = bob.stop().await;

    ScenarioResult { passed, events }
}

fn print_events(label: &str, events: &[String]) {
    println!("--- mvr events: {label} ---");
    for ev in events {
        println!("{ev}");
    }
    println!("--- end events ---");
}

/// H1's distinguishing experiment as a green-by-default assertion:
/// a forced fresh announce after `PROXY_BLOCK_END` reaches alice
/// within the wait window. Stays green on master and is the canary
/// against scaffolding regressions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_failure_recovery_silent_resume_with_forced_announce() {
    // Wire the structured event-log subscriber.  All threads —
    // main and tokio worker — route through the same global
    // subscriber registered once via Layer composition (see
    // leviculum_std::test_support::tracing_setup), so events
    // emitted from inside `run_scenario` (which spawns work onto
    // worker threads) end up in `_evlog`'s buffer.  On test
    // panic, the Drop dumps the buffer to stderr.
    let _evlog = leviculum_std::test_support::event_log::init_event_log();

    let opts = ScenarioOpts {
        block_s: 10,
        wait_s: 5,
        force_announce_after_restore: true,
    };
    let result = run_scenario(opts).await;
    if !result.passed {
        print_events("forced_announce", &result.events);
    }
    assert!(
        result.passed,
        "H1 distinguishing experiment failed: with a forced announce after \
         PROXY_BLOCK_END alice should receive ≥ 1 announce within {} s. \
         If this is red on master the mvr scaffold is broken before any \
         iface-level mechanism is even tested.",
        opts.wait_s,
    );
    leviculum_std::assert_no_schema_violations!(_evlog);
}

/// Baseline: no forced announce. The unfixed iface holds whatever
/// path it had pre-block; without H1's "soft bounce → solicit fresh
/// announce" mechanism, no announce arrives in the wait window and
/// the assertion fails. Currently `#[ignore]` because the production
/// code does not yet implement H1; un-ignore in the same commit that
/// lands the iface fix so this becomes a regression guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Bug #29 baseline — fails on master until H1's iface-level fix lands"]
async fn link_failure_recovery_silent_resume_baseline() {
    let opts = ScenarioOpts {
        block_s: 10,
        wait_s: 5,
        force_announce_after_restore: false,
    };
    let result = run_scenario(opts).await;
    if !result.passed {
        print_events("baseline_unfixed", &result.events);
    }
    assert!(
        result.passed,
        "Bug #29 unfixed: alice received no fresh announce within {} s of \
         PROXY_BLOCK_END. The TCP iface saw a silent-then-resume pattern \
         and did nothing about it. H1 fix target: \
         leviculum-std/src/interfaces/tcp_*.rs.",
        opts.wait_s,
    );
}

/// Parameter sweep + per-variant N=20 evidence for the report. Runs
/// each parameter point with and without the forced announce. Prints
/// `SWEEP …` lines suitable for grep + tabulation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Bug #29 parameter sweep — ~25 minutes runtime"]
async fn link_failure_recovery_silent_resume_param_sweep() {
    let parameter_points: &[(u64, u64)] = &[(10, 5), (15, 10)];
    let n = 20_usize;
    println!(
        "BUG29_SWEEP_HEADER block_s wait_s force N pass fail rate first_post_ms_min \
         first_post_ms_max"
    );
    for &(block_s, wait_s) in parameter_points {
        for force in [false, true] {
            let mut pass = 0_usize;
            let mut fail = 0_usize;
            let mut first_post_ms_samples: Vec<u128> = Vec::new();
            for i in 0..n {
                let opts = ScenarioOpts {
                    block_s,
                    wait_s,
                    force_announce_after_restore: force,
                };
                let result = run_scenario(opts).await;
                if result.passed {
                    pass += 1;
                } else {
                    fail += 1;
                }
                for ev in &result.events {
                    if let Some(rest) = ev.strip_prefix("ASSERT_OUTCOME ") {
                        if let Some(start) = rest.find("first_post_restore_ms=") {
                            let after = &rest[start + "first_post_restore_ms=".len()..];
                            let token = after.split_whitespace().next().unwrap_or("none");
                            if token != "none" {
                                if let Ok(v) = token.parse::<u128>() {
                                    first_post_ms_samples.push(v);
                                }
                            }
                        }
                    }
                }
                println!(
                    "BUG29_SWEEP_RUN block_s={} wait_s={} force={} iter={} passed={}",
                    block_s, wait_s, force, i, result.passed
                );
            }
            let rate_pct = (fail as f64) / (n as f64) * 100.0;
            let min_ms = first_post_ms_samples.iter().min().copied().unwrap_or(0);
            let max_ms = first_post_ms_samples.iter().max().copied().unwrap_or(0);
            println!(
                "BUG29_SWEEP_SUMMARY block_s={} wait_s={} force={} N={} pass={} fail={} \
                 fail_rate_pct={:.1} first_post_ms_min={} first_post_ms_max={}",
                block_s, wait_s, force, n, pass, fail, rate_pct, min_ms, max_ms
            );
        }
    }
}
