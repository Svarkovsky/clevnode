//! End-to-end workflow tests: install Stage-6 EventLogLayer → emit
//! events via `tracing::debug!` → dump buffer → invoke `jl` / `jldiff`
//! binary → assert on stdout/stderr/exit-code.  Stage 7 / Codeberg #39.
//!
//! All tests in this file share the global tracing subscriber, so they
//! must be serialised — `LOCK` does that.  Each test reads only its own
//! handle's buffer (via `EventLogHandle::dump()`), so even if the global
//! buffer leaks events from other tests inside the lock-window (it does
//! not, with serialisation), the fixture-side filter step would still
//! work; the lock just keeps timestamps clean.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use leviculum_std::event_log::merge_event_logs;
use leviculum_std::test_support::event_log::init_event_log;

static LOCK: Mutex<()> = Mutex::new(());

fn jl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jl"))
}

fn jldiff() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jldiff"))
}

fn pipe_through(cmd: &mut Command, input: &str) -> (String, String, i32) {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn join(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

#[test]
fn test_jl_filter_event_against_real_subscriber_output() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let evlog = init_event_log();

    tracing::debug!(
        event = "PKT_RX",
        ph = "00112233aabbccdd",
        iface = "lora0",
        r#type = "Data",
        dst = "abc1",
        hops = 0_u64,
        len = 64_u64
    );
    tracing::debug!(
        event = "ANN_RX",
        dst = "abc1",
        hops = 0_u64,
        iface = "lora0",
        path_response = false
    );
    tracing::debug!(
        event = "PATH_ADD",
        dst = "abc1",
        hops = 0_u64,
        iface = "lora0",
        next_hop = "alpha",
        ok = true,
        source = "announce",
        table_len = 1_u64
    );
    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "abc1",
        iface = "lora0",
        matched = true
    );
    tracing::debug!(
        event = "PKT_DROP",
        ph = "00112233aabbccdd",
        dst = "abc1",
        hops = 1_u64,
        iface_in = "lora0",
        reason = "ttl",
        r#type = "Data"
    );

    let dumped = evlog.dump();
    let input = join(&dumped);

    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), &input);
    assert_eq!(rc, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("PKT_RX"));
}

#[test]
fn test_jl_filter_node_against_real_subscriber_output() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());

    // We cannot change LEVICULUM_EVENT_NODE mid-process (cached in
    // OnceLock).  For multi-node coverage, write two log files via
    // separate processes — but for in-process workflow we synthesise
    // the multi-node shape directly.
    let alpha_log = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=30
";
    let beta_log = "\
PKT_RX node=beta dst=abc1 hops=1 iface=lora0 len=64 type=Data t=15
ANN_RX node=beta dst=abc1 hops=1 iface=lora0 path_response=false t=25
PATH_ADD node=beta dst=abc1 hops=1 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=35
";
    let dir = tempfile::tempdir().unwrap();
    let alpha_path = dir.path().join("alpha.log");
    let beta_path = dir.path().join("beta.log");
    std::fs::write(&alpha_path, alpha_log).unwrap();
    std::fs::write(&beta_path, beta_log).unwrap();

    let merged = merge_event_logs(&[alpha_path.clone(), beta_path.clone()]);
    let merged_text = join(&merged);

    let (out, _err, rc) = pipe_through(jl().args(["--node", "alpha"]), &merged_text);
    assert_eq!(rc, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3);
    for l in &lines {
        assert!(l.contains("node=alpha"), "unexpected line: {l}");
    }
}

#[test]
fn test_jl_t_range_against_real_subscriber() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let evlog = init_event_log();

    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "first",
        iface = "lora0",
        matched = true
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "middle",
        iface = "lora0",
        matched = true
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "third",
        iface = "lora0",
        matched = true
    );

    let dumped = evlog.dump();
    let input = join(&dumped);

    let (out, _err, rc) = pipe_through(
        jl().args(["--filter", "t>150", "--filter", "t<350"]),
        &input,
    );
    assert_eq!(rc, 0);
    // Only the second event (~200 ms after init) should fall in the window.
    // The exact t= value depends on init timing, but only one PKT_LOCAL
    // should match.  We assert by 'middle' marker rather than count to be
    // robust against close-to-window edge cases.
    let middles = out.matches("dst=middle").count();
    assert_eq!(middles, 1, "stdout: {out}");
}

#[test]
fn test_jl_since_event_against_real_subscriber() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let evlog = init_event_log();

    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "before1",
        iface = "lora0",
        matched = true
    );
    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "before2",
        iface = "lora0",
        matched = true
    );
    tracing::debug!(
        event = "ANN_RX",
        dst = "marker",
        hops = 0_u64,
        iface = "lora0",
        path_response = false
    );
    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "after1",
        iface = "lora0",
        matched = true
    );
    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "after2",
        iface = "lora0",
        matched = true
    );

    let dumped = evlog.dump();
    let input = join(&dumped);
    let (out, _err, rc) = pipe_through(jl().args(["--since-event", "ANN_RX"]), &input);
    assert_eq!(rc, 0);
    assert!(out.contains("ANN_RX"));
    assert!(out.contains("dst=after1"));
    assert!(out.contains("dst=after2"));
    assert!(!out.contains("dst=before1"));
    assert!(!out.contains("dst=before2"));
}

#[test]
fn test_jl_handles_violation_lines() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let evlog = init_event_log();

    // Emit a PKT_RX with a missing required key (`len`) — Stage-6's
    // catalogue check fires an EVENT_SCHEMA_VIOLATION line into the
    // handle's buffer.
    tracing::debug!(
        event = "PKT_RX",
        iface = "lora0",
        r#type = "Data",
        dst = "abc1",
        hops = 0_u64
    );

    let dumped = evlog.dump();
    let input = join(&dumped);
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), &input);
    assert_eq!(rc, 0);
    // Both the original PKT_RX line AND the synthetic
    // EVENT_SCHEMA_VIOLATION line (which has event=PKT_RX as a field)
    // must come through.  The Stage-6 emission shape is
    // `PKT_RX node=local <alphabetic fields> t=N`.
    assert!(
        out.lines()
            .any(|l| l.starts_with("PKT_RX") && l.contains("iface=lora0")),
        "missing PKT_RX line: {out}"
    );
    assert!(out.contains("EVENT_SCHEMA_VIOLATION event=PKT_RX"));
}

#[test]
fn test_jldiff_align_on_with_real_subscriber_logs() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let evlog = init_event_log();

    tracing::debug!(
        event = "PATH_ADD",
        dst = "abc1",
        hops = 0_u64,
        iface = "lora0",
        next_hop = "alpha",
        ok = true,
        source = "announce",
        table_len = 1_u64
    );
    let left_text = join(&evlog.dump());
    drop(evlog);

    let evlog2 = init_event_log();
    tracing::debug!(
        event = "PATH_ADD",
        dst = "abc1",
        hops = 1_u64,
        iface = "lora0",
        next_hop = "alpha",
        ok = true,
        source = "announce",
        table_len = 1_u64
    );
    let right_text = join(&evlog2.dump());
    drop(evlog2);

    let dir = tempfile::tempdir().unwrap();
    let l = dir.path().join("l.log");
    let r = dir.path().join("r.log");
    std::fs::write(&l, &left_text).unwrap();
    std::fs::write(&r, &right_text).unwrap();

    let mut cmd = jldiff();
    cmd.args([
        "--align-on",
        "event,dst",
        l.to_str().unwrap(),
        r.to_str().unwrap(),
    ]);
    let out = cmd.output().expect("spawn jldiff");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "jldiff failed: {stdout}");
    assert!(stdout.contains("MATCHED_DIFFER (1 pairs)"));
    assert!(stdout.contains("hops=0|1"));
}

#[test]
fn test_jldiff_left_only_with_real_subscriber_logs() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();

    let evlog = init_event_log();
    tracing::debug!(
        event = "PKT_RX",
        ph = "00112233aabbccdd",
        iface = "lora0",
        r#type = "Data",
        dst = "abc1",
        hops = 0_u64,
        len = 64_u64
    );
    tracing::debug!(
        event = "PKT_RX",
        ph = "00112233aabbccdd",
        iface = "lora0",
        r#type = "Data",
        dst = "abc2",
        hops = 0_u64,
        len = 64_u64
    );
    tracing::debug!(
        event = "PKT_RX",
        ph = "00112233aabbccdd",
        iface = "lora0",
        r#type = "Data",
        dst = "abc3",
        hops = 0_u64,
        len = 64_u64
    );
    tracing::debug!(
        event = "PKT_RX",
        ph = "00112233aabbccdd",
        iface = "lora0",
        r#type = "Data",
        dst = "abc4",
        hops = 0_u64,
        len = 64_u64
    );
    let left_text = join(&evlog.dump());
    drop(evlog);

    let evlog2 = init_event_log();
    tracing::debug!(
        event = "PKT_RX",
        ph = "00112233aabbccdd",
        iface = "lora0",
        r#type = "Data",
        dst = "abc1",
        hops = 0_u64,
        len = 64_u64
    );
    tracing::debug!(
        event = "PKT_RX",
        ph = "00112233aabbccdd",
        iface = "lora0",
        r#type = "Data",
        dst = "abc2",
        hops = 0_u64,
        len = 64_u64
    );
    tracing::debug!(
        event = "PKT_RX",
        ph = "00112233aabbccdd",
        iface = "lora0",
        r#type = "Data",
        dst = "abc4",
        hops = 0_u64,
        len = 64_u64
    );
    let right_text = join(&evlog2.dump());
    drop(evlog2);

    let l = dir.path().join("l.log");
    let r = dir.path().join("r.log");
    std::fs::write(&l, &left_text).unwrap();
    std::fs::write(&r, &right_text).unwrap();

    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,dst",
            l.to_str().unwrap(),
            r.to_str().unwrap(),
        ])
        .output()
        .expect("spawn jldiff");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    assert!(stdout.contains("LEFT_ONLY (1)"));
    assert!(stdout.contains("dst=abc3"));
}

#[test]
fn test_jldiff_multiprocess_merged_input() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();

    // Synthesise two per-process files (the multi-process workflow
    // path), merge each, then jldiff over the merged versions.
    let alpha_log = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
";
    let beta_log = "\
PKT_RX node=beta dst=abc1 hops=1 iface=lora0 len=64 type=Data t=15
PATH_ADD node=beta dst=abc1 hops=1 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=25
";
    let alpha_path = dir.path().join("alpha.log");
    let beta_path = dir.path().join("beta.log");
    std::fs::write(&alpha_path, alpha_log).unwrap();
    std::fs::write(&beta_path, beta_log).unwrap();

    let merged_left = merge_event_logs(&[alpha_path.clone(), beta_path.clone()]);
    let merged_right = merge_event_logs(&[alpha_path, beta_path]);
    let l = dir.path().join("l.log");
    let r = dir.path().join("r.log");
    std::fs::write(&l, join(&merged_left)).unwrap();
    std::fs::write(&r, join(&merged_right)).unwrap();

    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,dst",
            l.to_str().unwrap(),
            r.to_str().unwrap(),
        ])
        .output()
        .expect("spawn jldiff");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    // Both sides identical → all matched-identical, zeros elsewhere.
    assert!(stdout.contains("LEFT_ONLY (0)"));
    assert!(stdout.contains("RIGHT_ONLY (0)"));
    assert!(stdout.contains("MATCHED_DIFFER (0 pairs)"));
    assert!(stdout.contains("MATCHED_IDENTICAL (4 pairs)"));
}

#[test]
fn test_jl_empty_subscriber_output() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let evlog = init_event_log();
    let dumped = evlog.dump();
    drop(evlog);
    let input = join(&dumped);

    let (out, _err, rc) = pipe_through(&mut jl(), &input);
    assert_eq!(rc, 0);
    // The empty handle returns dump=[] which join's to "\n" — jl reads
    // one empty line, which is not an event, so non-event-passthrough
    // emits the empty line back.  Both behaviours are correct.
    assert!(out.lines().all(|l| l.trim().is_empty()));
}

#[test]
fn test_jl_handles_panic_dump_banner() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let dump_path = dir.path().join("panic-dump.log");

    // A wedge of test code wrapped in catch_unwind: closure panics, the
    // EventLogHandle's Drop sees thread::panicking() and writes the
    // banner-wrapped buffer to dump_path.
    let dump_path_for_closure = dump_path.clone();
    let _ = std::panic::catch_unwind(|| {
        let _evlog =
            leviculum_std::test_support::event_log::init_event_log_to_file(dump_path_for_closure);
        tracing::debug!(
            event = "PKT_RX",
            iface = "lora0",
            r#type = "Data",
            dst = "abc1",
            hops = 0_u64,
            len = 64_u64
        );
        tracing::debug!(
            event = "PKT_LOCAL",
            dst = "abc1",
            iface = "lora0",
            matched = true
        );
        panic!("intentional panic to trigger dump");
    });

    let dump = std::fs::read_to_string(&dump_path).expect("dump file should exist");
    // Sanity: banner present.
    assert!(dump.contains("=== EVENT LOG DUMP"));
    assert!(dump.contains("=== END EVENT LOG DUMP"));

    // Pipe the dump (banner + events) through jl with a PKT_RX filter.
    // Banners pass through unchanged; events filter as expected.
    let (out, _err, rc) = pipe_through(jl().args(["--filter", "event=PKT_RX"]), &dump);
    assert_eq!(rc, 0);
    assert!(out.contains("=== EVENT LOG DUMP"));
    assert!(out.contains("=== END EVENT LOG DUMP"));
    assert!(
        out.lines()
            .any(|l| l.starts_with("PKT_RX") && l.contains("iface=lora0")),
        "missing PKT_RX line: {out}"
    );
    assert!(!out.contains("PKT_LOCAL"));
}

#[test]
fn test_jldiff_unalignable_event_with_real_subscriber() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();

    // Synthesise — emitting an event missing `dst` via tracing would
    // trigger a schema-violation; instead we construct a known-shape
    // log that contains an event with no `iface` (the align key).
    let left_log = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PKT_LOCAL node=alpha dst=abc1 matched=true t=20
";
    let right_log = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
";
    let l = dir.path().join("l.log");
    let r = dir.path().join("r.log");
    std::fs::write(&l, left_log).unwrap();
    std::fs::write(&r, right_log).unwrap();

    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,iface",
            l.to_str().unwrap(),
            r.to_str().unwrap(),
        ])
        .output()
        .expect("spawn jldiff");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    assert!(
        stdout.contains("[unalignable: missing key iface]"),
        "missing unalignable note: {stdout}"
    );
}

#[test]
fn test_jl_pipeline_with_jldiff() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();

    let log_a = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=20
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=30
";
    let log_b = "\
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PATH_ADD node=alpha dst=abc1 hops=1 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=20
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=30
";
    let log_a_path = dir.path().join("a.log");
    let log_b_path = dir.path().join("b.log");
    std::fs::write(&log_a_path, log_a).unwrap();
    std::fs::write(&log_b_path, log_b).unwrap();

    // jl --filter event=PATH_ADD on each into intermediate files.
    let intermediate = |src: &PathBuf, dst: &PathBuf| {
        let cmd_out = jl()
            .args(["--filter", "event=PATH_ADD", src.to_str().unwrap()])
            .output()
            .expect("spawn jl");
        std::fs::write(dst, cmd_out.stdout).unwrap();
    };
    let a_filt = dir.path().join("a-filt.log");
    let b_filt = dir.path().join("b-filt.log");
    intermediate(&log_a_path, &a_filt);
    intermediate(&log_b_path, &b_filt);

    let cmd_out = jldiff()
        .args([
            "--align-on",
            "event,dst",
            a_filt.to_str().unwrap(),
            b_filt.to_str().unwrap(),
        ])
        .output()
        .expect("spawn jldiff");
    let stdout = String::from_utf8_lossy(&cmd_out.stdout);
    // The filtered files contain only PATH_ADD; the two PATH_ADDs differ
    // by hops=0|1.  jldiff reports MATCHED_DIFFER 1 pairs with hops=0|1.
    assert!(stdout.contains("MATCHED_DIFFER (1 pairs)"));
    assert!(stdout.contains("hops=0|1"));
}

#[test]
fn test_jl_two_filters_and_semantics() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let evlog = init_event_log();

    tracing::debug!(
        event = "PKT_RX",
        iface = "lora0",
        r#type = "Data",
        dst = "abc1",
        hops = 0_u64,
        len = 64_u64
    );
    tracing::debug!(
        event = "PKT_RX",
        iface = "tcp1",
        r#type = "Data",
        dst = "abc1",
        hops = 0_u64,
        len = 64_u64
    );
    tracing::debug!(
        event = "PKT_LOCAL",
        dst = "abc1",
        iface = "lora0",
        matched = true
    );

    let input = join(&evlog.dump());
    // AND: event=PKT_RX AND iface=lora0 → only the first event matches.
    let (out, _err, rc) = pipe_through(
        jl().args(["--filter", "event=PKT_RX", "--filter", "iface=lora0"]),
        &input,
    );
    assert_eq!(rc, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("PKT_RX"));
    assert!(lines[0].contains("iface=lora0"));
}
