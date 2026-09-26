//! Multi-process demonstration that the test port allocator is process-safe.
//!
//! The defect this pins: until 2026-08 each test binary carried its own
//! `AtomicU16` starting at a fixed base, so two concurrently running test
//! binaries — which is exactly what `cargo test --workspace` produces — were
//! handed the *same* port numbers in the same order, and each one's probe bind
//! succeeded inside the other's alloc → bind handoff window.
//!
//! Measured on two concurrent runs of the `mvr` binary under
//! `strace -e trace=bind`, before the fix: 10 ports bound by both processes
//! and ~80 `EADDRINUSE` binds in the band per run. Nothing went red, because
//! the probe loop retried — which is what a race that has never been seen fail
//! looks like from the outside.
//!
//! This test does not model that. It spawns real processes running the real
//! allocator and checks the property the allocator claims: **no two processes
//! are ever handed the same number.**
//!
//! # The negative control
//!
//! Disjointness is trivially satisfiable — by a broken harness that fails to
//! start the children, by workers that allocate nothing. So the same
//! experiment runs a second time with the pre-fix strategy (a per-process
//! counter from a fixed base, [`legacy_next_port`]) and must produce
//! collisions. If the fixed allocator ever regresses to a per-process counter
//! the first assertion fails; if the demonstration itself rots, the control
//! fails.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

/// Host-wide listener-port allocator under test.
#[path = "support/port_alloc.rs"]
#[allow(dead_code)]
mod port_alloc;

/// Concurrent worker processes.
const WORKERS: usize = 4;
/// Port numbers each worker draws. Above [`port_alloc::CHUNK`] on purpose, so
/// every worker refills from the shared counter at least once and the test
/// covers the refill path rather than a single reservation.
const PER_WORKER: usize = 100;

/// Set on a worker child; names the file it writes its port numbers to.
const WORKER_OUT_ENV: &str = "LEVICULUM_PORT_ALLOC_WORKER_OUT";
/// Set alongside [`WORKER_OUT_ENV`] to select the pre-fix strategy.
const WORKER_LEGACY_ENV: &str = "LEVICULUM_PORT_ALLOC_WORKER_LEGACY";

/// The allocator as it stood before 2026-08, reproduced here as the negative
/// control: a counter private to the process, based at the bottom of the band.
///
/// No probe bind. The probe is not what decides who gets a number — it only
/// skips ports already occupied, and in the handoff window the port is by
/// definition not occupied yet.
fn legacy_next_port(counter: &mut u16) -> u16 {
    let port = *counter;
    *counter = if port + 1 >= port_alloc::PORT_RANGE_END {
        port_alloc::PORT_RANGE_BASE
    } else {
        port + 1
    };
    port
}

/// Body of a worker child: draw [`PER_WORKER`] numbers, write them to the file
/// named by [`WORKER_OUT_ENV`], one per line.
///
/// This runs inside the same test binary (re-executed with `--exact`), which
/// is what keeps the child using the same allocator code as the parent rather
/// than a copy of it.
fn run_as_worker(out: PathBuf) {
    let legacy = std::env::var_os(WORKER_LEGACY_ENV).is_some();
    let mut legacy_counter = port_alloc::PORT_RANGE_BASE;
    let mut lines = String::new();
    for _ in 0..PER_WORKER {
        let port = if legacy {
            legacy_next_port(&mut legacy_counter)
        } else {
            port_alloc::next_port_candidate()
        };
        lines.push_str(&port.to_string());
        lines.push('\n');
    }
    std::fs::write(&out, lines).expect("worker writes its ports");
}

/// Run [`WORKERS`] children concurrently and return each child's port numbers.
fn collect_from_workers(state_dir: &std::path::Path, legacy: bool) -> Vec<Vec<u16>> {
    let exe = std::env::current_exe().expect("current test binary");
    let mut children = Vec::new();
    let mut outputs = Vec::new();

    for index in 0..WORKERS {
        let out = state_dir.join(format!("worker-{index}.ports"));
        let mut cmd = Command::new(&exe);
        cmd.args(["--exact", "--test-threads=1", "port_alloc_worker"])
            .env(WORKER_OUT_ENV, &out)
            .env(port_alloc::STATE_DIR_ENV, state_dir)
            // A child is libtest too, and its `running 1 test` / `test result:`
            // lines land in the parent's stream. scripts/run-with-manifest.py
            // parses exactly those lines and reconciles the names it found
            // against the summary, so leaking them would corrupt the gate's
            // manifest. The child's verdict is its exit status.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if legacy {
            cmd.env(WORKER_LEGACY_ENV, "1");
        }
        children.push(cmd.spawn().expect("spawn worker"));
        outputs.push(out);
    }

    for (index, mut child) in children.into_iter().enumerate() {
        let status = child.wait().expect("wait for worker");
        assert!(status.success(), "worker {index} exited {status:?}");
    }

    outputs
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("worker output {}: {e}", path.display()));
            let ports: Vec<u16> = text.lines().map(|l| l.parse().expect("port")).collect();
            assert_eq!(ports.len(), PER_WORKER, "worker drew the wrong count");
            ports
        })
        .collect()
}

/// Ports appearing in more than one worker's output.
fn shared_ports(per_worker: &[Vec<u16>]) -> BTreeSet<u16> {
    let mut seen: BTreeSet<u16> = BTreeSet::new();
    let mut shared: BTreeSet<u16> = BTreeSet::new();
    for ports in per_worker {
        for port in ports.iter().copied().collect::<BTreeSet<_>>() {
            if !seen.insert(port) {
                shared.insert(port);
            }
        }
    }
    shared
}

/// Worker entry point, and — when run without the worker environment, i.e. in
/// an ordinary suite run — the in-process property: a single process never
/// hands out a number twice, and never one outside the band.
#[test]
fn port_alloc_worker() {
    if let Some(out) = std::env::var_os(WORKER_OUT_ENV) {
        run_as_worker(PathBuf::from(out));
        return;
    }

    let ports: Vec<u16> = (0..PER_WORKER)
        .map(|_| port_alloc::next_port_candidate())
        .collect();
    let distinct: BTreeSet<u16> = ports.iter().copied().collect();
    assert_eq!(distinct.len(), ports.len(), "a number was handed out twice");
    for port in ports {
        assert!(
            (port_alloc::PORT_RANGE_BASE..port_alloc::PORT_RANGE_END).contains(&port),
            "{port} is outside the test port band"
        );
    }
}

/// The pair primitive under the concurrency it lives in: the successor of a
/// pair is reserved too, so no other drawer in the process is handed it.
///
/// Codeberg #207 needs two consecutive numbers, because AutoInterface derives
/// its unicast discovery port as `discovery_port + 1` and a test therefore
/// occupies both. The first attempt at that drew twice from
/// [`port_alloc::free_udp_port`] and checked adjacency in the caller. That is
/// right in a single-threaded run and wrong in every real one: a test binary
/// runs its tests in parallel, another thread draws between the two calls,
/// and the two numbers come back adjacent almost never — three AutoInterface
/// tests panicked on it. Hence threads here rather than a loop: a
/// single-threaded version of this test passes against the broken
/// implementation.
#[test]
fn port_pairs_reserve_their_successor_under_concurrency() {
    const PAIR_THREADS: usize = 4;
    const SINGLE_THREADS: usize = 4;
    const DRAWS: usize = 12;

    let mut handles = Vec::new();
    for _ in 0..PAIR_THREADS {
        handles.push(std::thread::spawn(|| {
            let mut claimed = Vec::new();
            for _ in 0..DRAWS {
                let first = port_alloc::free_udp_port_pair();
                claimed.push(first);
                claimed.push(first + 1);
            }
            claimed
        }));
    }
    for _ in 0..SINGLE_THREADS {
        handles.push(std::thread::spawn(|| {
            (0..DRAWS).map(|_| port_alloc::free_udp_port()).collect()
        }));
    }

    let claimed: Vec<u16> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("worker thread"))
        .collect();

    let distinct: BTreeSet<u16> = claimed.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        claimed.len(),
        "a number was handed to two drawers: either a pair's successor was \
         left in the pool, or the pair was not adjacent"
    );
    for port in &claimed {
        assert!(
            (port_alloc::PORT_RANGE_BASE..port_alloc::PORT_RANGE_END).contains(port),
            "{port} is outside the test port band"
        );
    }
}

#[test]
fn concurrent_processes_are_never_handed_the_same_port() {
    let state = tempfile::tempdir().expect("state dir");

    let fixed = collect_from_workers(state.path(), false);
    let collisions = shared_ports(&fixed);
    assert!(
        collisions.is_empty(),
        "{} port(s) handed to more than one of {WORKERS} concurrent processes: {:?}",
        collisions.len(),
        collisions.iter().take(20).collect::<Vec<_>>()
    );
    for ports in &fixed {
        for port in ports {
            assert!(
                (port_alloc::PORT_RANGE_BASE..port_alloc::PORT_RANGE_END).contains(port),
                "{port} is outside the test port band"
            );
        }
    }

    // Negative control: the same harness, the pre-fix strategy. It must
    // collide, or the experiment above proves nothing about the allocator.
    let legacy_state = tempfile::tempdir().expect("legacy state dir");
    let legacy = collect_from_workers(legacy_state.path(), true);
    let legacy_collisions = shared_ports(&legacy);
    assert!(
        !legacy_collisions.is_empty(),
        "negative control did not collide: the pre-fix per-process counter is \
         supposed to hand every process the same numbers, so this harness is \
         no longer demonstrating anything"
    );
}
