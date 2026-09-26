//! Host-wide listener-port allocator for the test suites.
//!
//! Every suite that spawns a daemon needs a TCP or UDP port nothing else is
//! using. The allocation is a *handoff*: the allocator picks a number, proves
//! it is currently bindable, releases it, and the intended consumer binds it a
//! moment later — a config file gets written, a release binary is spawned, a
//! Python daemon boots. The consumer is usually another process, so the port
//! can only be passed as a number; there is no listener to hand over.
//!
//! That handoff window is what has to be protected, and the protection has to
//! reach across processes. Until 2026-08 it did not: each test binary carried
//! its own `AtomicU16` starting at a fixed base, so two concurrently running
//! test binaries walked the *same* numbers in the same order and each one's
//! probe bind succeeded in the other's handoff window. Measured on two
//! concurrent runs of the `mvr` binary under `strace -e trace=bind`: 10 ports
//! bound by both processes and ~80 `EADDRINUSE` probe failures per run, of
//! which only the probe loop's retry saved the suite. That is what made
//! `cargo test --workspace` (which runs test binaries in parallel) and any
//! `-j` mutation pass unusable.
//!
//! # Why a shared counter and not the alternatives
//!
//! *Hold the listener through the handoff* closes the window completely, but
//! only for in-process consumers. The majority of call sites hand the number
//! to a separate process through a config file, and a `TcpListener` cannot be
//! given to one without fd passing (rewriting every daemon) or `SO_REUSEPORT`
//! (which lets a third party in instead of keeping it out).
//!
//! *A band derived from the PID* is cheap but probabilistic: PIDs are reused,
//! two concurrent runs can land in the same band, and it fragments the 4000
//! usable ports into per-process slices sized for the worst case.
//!
//! What is implemented instead: **one counter per host**, in a file, bumped
//! under `flock`. A process reserves a [`CHUNK`] of numbers at a time and
//! hands them out from memory, so the file is touched once per 64 ports
//! rather than once per port. Two processes are never handed the same number
//! at all — not merely unlikely to collide in the window — and the counter is
//! shared by every checkout on the host, because ports are a host resource
//! and the rig worktree runs against the same kernel as the CI tree.
//!
//! The probe bind stays. It is what catches an occupant that never asked this
//! allocator for anything.
//!
//! # The band
//!
//! 61000-65000 sits above Linux's default `ip_local_port_range` ceiling of
//! 60999, so an OS-assigned `bind(0)` elsewhere in the suite cannot win a
//! number this allocator has handed out but whose consumer has not yet bound
//! it. Should a host be configured with an `ip_local_port_range` that spans
//! the band, the probe bind still skips whatever is already taken.
//!
//! A single counter also removes the reason the suites used to carry
//! *disjoint* bases (61000/61500/62500/63500): those existed only so that
//! several independent counters inside one test binary would not walk into
//! each other, which is a problem one counter does not have.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// First port of the band.
pub const PORT_RANGE_BASE: u16 = 61000;
/// One past the last port of the band.
pub const PORT_RANGE_END: u16 = 65000;

/// Numbers reserved from the shared counter per file access.
///
/// A full `rnsd_interop` run draws on the order of 2000 ports, so 64 costs
/// ~30 lock/rewrite round trips per run — negligible — while keeping a
/// crashed or short-lived process from stranding much of the band.
pub const CHUNK: u16 = 64;

/// Overrides the directory holding the shared counter. Set by the
/// multi-process test in `tests/port_alloc_multiprocess.rs` so it can exercise
/// the allocator without perturbing a real run on the same host.
pub const STATE_DIR_ENV: &str = "LEVICULUM_TEST_PORT_STATE";

struct Chunk {
    next: u16,
    end: u16,
}

/// The chunk this process is currently handing out. Empty until the first
/// allocation.
static CHUNK_STATE: Mutex<Chunk> = Mutex::new(Chunk { next: 0, end: 0 });

/// Directory holding the shared counter.
///
/// `XDG_RUNTIME_DIR` is preferred: it is per-user, on tmpfs, and cleaned by
/// the session, so a stale counter cannot outlive a reboot. The fallback
/// carries the user name because `/tmp` is shared and a counter file owned by
/// another user is unopenable, which would fail every test run on the host
/// with a permission error rather than a port error.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("leviculum-test-ports");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    std::env::temp_dir().join(format!("leviculum-test-ports-{user}"))
}

/// Path of the shared counter file.
pub fn counter_path() -> PathBuf {
    state_dir().join("next-port")
}

/// Reserve the next [`CHUNK`] of port numbers from the counter at `path`,
/// returning the half-open range `[base, end)`.
///
/// The file holds one decimal number: the next unreserved port. It is created
/// at [`PORT_RANGE_BASE`] on first use, and a value outside the band — a
/// truncated write, a hand-edited file, a band that has since been narrowed —
/// is treated as absent rather than trusted.
///
/// `flock` is what makes this atomic between processes. It is released by the
/// kernel when the file is closed *or the process dies*, so a killed test run
/// cannot wedge the counter for the next one.
pub fn reserve_chunk(path: &Path) -> std::io::Result<(u16, u16)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.lock()?;
    let result = reserve_locked(&mut file);
    let _ = file.unlock();
    result
}

fn reserve_locked(file: &mut std::fs::File) -> std::io::Result<(u16, u16)> {
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let base = text
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|v| (u32::from(PORT_RANGE_BASE)..u32::from(PORT_RANGE_END)).contains(v))
        .map(|v| v as u16)
        .unwrap_or(PORT_RANGE_BASE);

    let end = base.saturating_add(CHUNK).min(PORT_RANGE_END);
    // Wrapping is safe by age, not by absence: a lap of the band is thousands
    // of allocations, by which time the ports from the previous lap are either
    // still bound (the probe skips them) or long released.
    let next = if end >= PORT_RANGE_END {
        PORT_RANGE_BASE
    } else {
        end
    };

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    write!(file, "{next}")?;
    file.flush()?;
    Ok((base, end))
}

/// The next port number for this process, from the shared counter.
///
/// No probe: this is the number the allocator has decided nobody else on the
/// host will be given. [`try_free_tcp_port`] adds the "and it is actually
/// bindable right now" half.
pub fn next_port_candidate() -> u16 {
    let mut state = CHUNK_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    next_candidate_locked(&mut state)
}

/// [`next_port_candidate`] with the chunk lock already held, so a caller that
/// needs *two related* numbers can take them without another thread drawing
/// between the two.
fn next_candidate_locked(state: &mut Chunk) -> u16 {
    if state.next >= state.end {
        let path = counter_path();
        let (base, end) = reserve_chunk(&path).unwrap_or_else(|err| {
            panic!(
                "test port allocator cannot use its shared counter at {}: {err}\n\
                 (set {STATE_DIR_ENV} to a writable directory to move it)",
                path.display()
            )
        });
        state.next = base;
        state.end = end;
    }
    let port = state.next;
    state.next += 1;
    port
}

/// Next port the OS confirms is bindable for TCP on 127.0.0.1, skipping past
/// any occupant. `None` only if the whole band is occupied.
pub fn try_free_tcp_port() -> Option<u16> {
    for _ in 0..(PORT_RANGE_END - PORT_RANGE_BASE) {
        let candidate = next_port_candidate();
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", candidate)) {
            // Released immediately: the consumer binds it. Which process gets
            // this number is decided by the shared counter, not by this bind.
            drop(listener);
            return Some(candidate);
        }
    }
    None
}

/// UDP counterpart of [`try_free_tcp_port`]. Separate because a free TCP port
/// says nothing about the same UDP port.
pub fn try_free_udp_port() -> Option<u16> {
    for _ in 0..(PORT_RANGE_END - PORT_RANGE_BASE) {
        let candidate = next_port_candidate();
        if let Ok(socket) = UdpSocket::bind(("127.0.0.1", candidate)) {
            drop(socket);
            return Some(candidate);
        }
    }
    None
}

/// Two *consecutive* UDP ports, both confirmed bindable; returns the first.
///
/// For consumers that do not choose their second port: AutoInterface derives
/// its unicast discovery port as `discovery_port + 1`, so a test needs `p`
/// and `p + 1` (Codeberg #207). Drawing only `p` and using `p + 1` implicitly
/// would leave the successor in the pool for the next caller — the collision
/// the allocator exists to prevent, moved one number along.
///
/// Both numbers are therefore consumed, and both are drawn under one hold of
/// the chunk lock: two separate calls interleave with the other threads of a
/// parallel test binary, which is why they almost never come back adjacent.
/// The counter is sequential, so a pair that is *not* adjacent means either
/// the chunk ran out between the two (the next chunk starts wherever other
/// processes left the shared counter) or the probe skipped an occupant; both
/// are handled by trying again with the next pair.
pub fn try_free_udp_port_pair() -> Option<u16> {
    let mut state = CHUNK_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for _ in 0..(PORT_RANGE_END - PORT_RANGE_BASE) {
        let first = next_candidate_locked(&mut state);
        let second = next_candidate_locked(&mut state);
        if second != first + 1 {
            continue;
        }
        // Held together: releasing the first before probing the second would
        // let an outside process take it back in between.
        let Ok(a) = UdpSocket::bind(("127.0.0.1", first)) else {
            continue;
        };
        let Ok(b) = UdpSocket::bind(("127.0.0.1", second)) else {
            continue;
        };
        drop(a);
        drop(b);
        return Some(first);
    }
    None
}

/// [`try_free_tcp_port`], panicking on an exhausted band. For call sites that
/// have no error path worth taking.
pub fn free_tcp_port() -> u16 {
    try_free_tcp_port().expect("exhausted the test port band 61000-65000")
}

/// [`try_free_udp_port`], panicking on an exhausted band.
pub fn free_udp_port() -> u16 {
    try_free_udp_port().expect("exhausted the test port band 61000-65000")
}

/// [`try_free_udp_port_pair`], panicking on an exhausted band.
pub fn free_udp_port_pair() -> u16 {
    try_free_udp_port_pair().expect("exhausted the test port band 61000-65000")
}
