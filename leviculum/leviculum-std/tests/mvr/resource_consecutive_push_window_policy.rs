//! mvr for the `lora_window_ab` run-3 stall (Codeberg #85 bench).
//!
//! On the rig, the third consecutive 50 KiB `lncp` push over LoRa stalls
//! after its first windows and dies with `LinkClosed` — three-for-three,
//! always run 3. The instructed suspect was receiver-side state surviving
//! transfers (window state, resource bookkeeping, dedup/hash state
//! colliding with a third identical transfer flow).
//!
//! This mvr holds the whole protocol stack (lnsd, lncp listener, resource
//! machinery, window policy) unchanged and replaces only the carrier:
//! two lnsd daemons over host-local TCP instead of RNode LoRa. One
//! listener process survives all transfers, exactly as in the scenario;
//! each push sends a fresh 50 KiB payload of the same size and name, and
//! the received file is deleted between runs, so sizes, sequence shapes
//! and flow shapes repeat the way the scenario repeats them.
//!
//! It asserts that transfers 1..=3 ALL complete under both window
//! policies. It passes on HEAD: the rig stall does not reproduce at the
//! protocol layer, which localises the mechanism to the LoRa carrier
//! path (the RNode firmware's lawful-default long-term airtime lock —
//! see the 2026-08-21 report). The test stays as the regression net for
//! the receiver-state hypothesis: if consecutive-transfer state ever
//! does poison a later transfer, this goes red without any radio.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use leviculum_std::process::spawn_supervised;

/// Number of consecutive pushes. The rig failure is deterministic on the
/// third; one extra run guards against an off-by-one in the trigger.
const PUSHES: usize = 4;

/// Payload size, matching the scenario's `file_sizes = [51200]`.
const PAYLOAD_SIZE: usize = 51200;

/// Per-push completion budget. Host-local TCP moves 50 KiB in well under
/// a second; a stalled transfer is dead long before this.
const PUSH_TIMEOUT: Duration = Duration::from_secs(60);

fn release_bin(name: &str) -> PathBuf {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let triple = std::env::var("CARGO_BUILD_TARGET")
        .unwrap_or_else(|_| "x86_64-unknown-linux-musl".to_string());
    let triple_release = target_dir.join(&triple).join("release").join(name);
    if triple_release.exists() {
        triple_release
    } else {
        target_dir.join("release").join(name)
    }
}

fn write_config(dir: &Path, instance_name: &str, interface_ini: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::create_dir_all(dir.join("storage"))?;
    let cfg = format!(
        "[reticulum]\n  \
         enable_transport = yes\n  \
         share_instance = yes\n  \
         instance_name = {instance_name}\n  \
         respond_to_probes = yes\n\n\
         [logging]\n  loglevel = 5\n\n\
         [interfaces]\n{interface_ini}\n"
    );
    let mut f = fs::File::create(dir.join("config"))?;
    f.write_all(cfg.as_bytes())?;
    Ok(())
}

/// Spawn `lnsd --config <dir>` with the window-policy env set, mirroring
/// the integ executor, which forwards the variable to every process it
/// spawns.
fn spawn_lnsd(dir: &Path, label: &str, policy: &str) -> std::io::Result<Child> {
    let lnsd = release_bin("lnsd");
    assert!(
        lnsd.exists(),
        "{} not found - run `cargo build --release --bin lnsd --bin lncp` first",
        lnsd.display()
    );
    let stdout_path = dir.join(format!("{label}-stdout.log"));
    let stderr_path = dir.join(format!("{label}-stderr.log"));
    let mut cmd = Command::new(&lnsd);
    cmd.arg("-v")
        .arg("--config")
        .arg(dir)
        .env(
            leviculum_std::resource_policy::RESOURCE_WINDOW_POLICY_ENV,
            policy,
        )
        .stdout(Stdio::from(fs::File::create(stdout_path)?))
        .stderr(Stdio::from(fs::File::create(stderr_path)?));
    spawn_supervised(cmd)
}

fn spawn_lncp(
    config_dir: &Path,
    label: &str,
    policy: &str,
    args: &[&str],
) -> std::io::Result<(Child, PathBuf, PathBuf)> {
    let lncp = release_bin("lncp");
    assert!(
        lncp.exists(),
        "{} not found - run `cargo build --release --bin lncp` first",
        lncp.display()
    );
    let stdout_path = config_dir.join(format!("{label}-stdout.log"));
    let stderr_path = config_dir.join(format!("{label}-stderr.log"));
    let mut cmd = Command::new(&lncp);
    cmd.arg("-v")
        .arg("--config")
        .arg(config_dir)
        .args(args)
        .env(
            leviculum_std::resource_policy::RESOURCE_WINDOW_POLICY_ENV,
            policy,
        )
        .stdout(Stdio::from(fs::File::create(&stdout_path)?))
        .stderr(Stdio::from(fs::File::create(&stderr_path)?));
    let child = spawn_supervised(cmd)?;
    Ok((child, stdout_path, stderr_path))
}

fn wait_for_line(path: &Path, pred: impl Fn(&str) -> bool, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path) {
            for line in text.lines() {
                if pred(line) {
                    return Some(line.to_string());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn parse_listening_hash(line: &str) -> Option<String> {
    line.split_whitespace()
        .next_back()
        .map(String::from)
        .filter(|s| s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Wait for `child` to exit within `timeout`; kill it and return None on
/// overrun so the failure surfaces as "push N timed out", not a hung test.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return None,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// One full scenario shape under one window policy: a surviving listener,
/// PUSHES consecutive same-size same-name pushes, delete-received between
/// runs. Panics with the run index and the listener log tail on the first
/// push that fails, so a red run names the transfer that broke.
fn consecutive_pushes_complete(policy: &str) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path();
    let dir_a = base.join("A");
    let dir_b = base.join("B");
    let tcp_port = crate::harness::port_alloc::free_tcp_port();

    let instance_a = format!("mvr-winpol-a-{tcp_port}");
    let instance_b = format!("mvr-winpol-b-{tcp_port}");
    write_config(
        &dir_a,
        &instance_a,
        &format!(
            "  [[Peer]]\n    type = TCPClientInterface\n    enabled = yes\n    \
             target_host = 127.0.0.1\n    target_port = {tcp_port}\n    \
             ingress_control = false\n"
        ),
    )
    .unwrap();
    write_config(
        &dir_b,
        &instance_b,
        &format!(
            "  [[Peer]]\n    type = TCPServerInterface\n    enabled = yes\n    \
             listen_ip = 127.0.0.1\n    listen_port = {tcp_port}\n    \
             ingress_control = false\n"
        ),
    )
    .unwrap();

    let mut lnsd_a = spawn_lnsd(&dir_a, "lnsd", policy).expect("spawn lnsd A");
    let mut lnsd_b = spawn_lnsd(&dir_b, "lnsd", policy).expect("spawn lnsd B");
    std::thread::sleep(Duration::from_secs(2));

    let save_dir = dir_b.join("received");
    fs::create_dir_all(&save_dir).unwrap();
    let (mut listener, _lo, listener_stderr) = spawn_lncp(
        &dir_b,
        "lncp-listener",
        policy,
        &["-l", "-n", "-s", save_dir.to_str().unwrap(), "-b", "0"],
    )
    .expect("spawn lncp listener");

    let dest_hash = wait_for_line(
        &listener_stderr,
        |l| l.contains("lncp listening on"),
        Duration::from_secs(10),
    )
    .as_deref()
    .and_then(parse_listening_hash)
    .unwrap_or_else(|| {
        let _ = listener.kill();
        let _ = lnsd_a.kill();
        let _ = lnsd_b.kill();
        panic!("listener never printed its destination hash within 10 s");
    });

    // Announce propagation A <- B.
    std::thread::sleep(Duration::from_secs(2));

    let send_file = dir_a.join("test_transfer.bin");
    let received_file = save_dir.join("test_transfer.bin");

    for run in 1..=PUSHES {
        // Fresh payload every run, same size and name — deterministic
        // stand-in for the scenario's per-run urandom regeneration, with
        // the run index mixed in so a stale-delivery bug shows up as a
        // content mismatch naming the run it came from.
        let payload: Vec<u8> = (0..PAYLOAD_SIZE)
            .map(|i| ((i + run * 7919) & 0xff) as u8)
            .collect();
        fs::write(&send_file, &payload).unwrap();

        let (mut sender, sender_stdout, sender_stderr) = spawn_lncp(
            &dir_a,
            &format!("lncp-push-{run}"),
            policy,
            &[send_file.to_str().unwrap(), &dest_hash],
        )
        .expect("spawn lncp push");

        let status = wait_with_timeout(&mut sender, PUSH_TIMEOUT);
        let ok = status.map(|s| s.success()).unwrap_or(false);
        if !ok {
            let sender_out = fs::read_to_string(&sender_stdout).unwrap_or_default();
            let sender_err = fs::read_to_string(&sender_stderr).unwrap_or_default();
            let listener_err = fs::read_to_string(&listener_stderr).unwrap_or_default();
            let _ = listener.kill();
            let _ = lnsd_a.kill();
            let _ = lnsd_b.kill();
            panic!(
                "push {run}/{PUSHES} (policy={policy}) failed: status={status:?}\n\
                 --- sender stdout tail ---\n{}\n\
                 --- sender stderr tail ---\n{}\n\
                 --- listener stderr tail ---\n{}",
                tail(&sender_out, 40),
                tail(&sender_err, 40),
                tail(&listener_err, 60),
            );
        }

        // The listener writes the file on completion; give the write a
        // moment, then compare contents and clear for the next run, as
        // the scenario does.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(received) = fs::read(&received_file) {
                if received == payload {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "push {run}/{PUSHES} (policy={policy}) exited 0 but the received \
                 file never matched the sent payload"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        fs::remove_file(&received_file).unwrap();
    }

    let _ = listener.kill();
    let _ = listener.wait();
    let _ = lnsd_a.kill();
    let _ = lnsd_a.wait();
    let _ = lnsd_b.kill();
    let _ = lnsd_b.wait();
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[test]
fn consecutive_pushes_complete_current_policy() {
    consecutive_pushes_complete("current");
}

#[test]
fn consecutive_pushes_complete_pythonlike_policy() {
    consecutive_pushes_complete("pythonlike");
}
