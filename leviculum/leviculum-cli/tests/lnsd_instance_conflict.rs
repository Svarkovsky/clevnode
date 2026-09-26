//! What the operator actually reads when `lnsd` lands on a taken
//! instance name.
//!
//! The library-level guard for this lives in
//! `leviculum-std/tests/shared_instance_conflict.rs` and asserts the typed
//! error and its Display text. That test alone was not enough: it went
//! green while the binary still printed
//!
//! ```text
//! Error: SharedInstanceNameInUse { name: "realcollision" }
//! ```
//!
//! because returning `Err` from `main` prints the *Debug* form. The
//! sentence only reaches the journal if `main` formats with Display, and
//! nothing below the binary can prove that it does. Hence this test,
//! which runs the real executable via `CARGO_BIN_EXE_lnsd`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leviculum_std::process::spawn_supervised;

/// Minimal daemon config: no interfaces, no transport, just a shared
/// instance under `name`. The socket bind is the whole subject.
fn write_config(dir: &std::path::Path, name: &str) {
    let mut f = std::fs::File::create(dir.join("config")).expect("write config");
    writeln!(
        f,
        "[reticulum]\n  enable_transport = False\n  share_instance = Yes\n  instance_name = {name}\n[logging]\n  loglevel = 3\n[interfaces]"
    )
    .expect("write config body");
}

/// Poll until the daemon has taken the abstract socket, so the second
/// process meets a bound name rather than a race.
fn wait_for_instance(name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(out) = Command::new("ss").arg("-xl").output() {
            if String::from_utf8_lossy(&out.stdout).contains(&format!("@rns/{name}")) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn lnsd_reports_a_taken_instance_name_in_words() {
    // Abstract sockets are per network namespace and outlive nothing, so
    // the name is keyed by pid to keep parallel runs apart.
    let name = format!("cliconflict{}", std::process::id());

    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    write_config(dir_a.path(), &name);
    write_config(dir_b.path(), &name);

    // Kills the holder however the assertions below turn out, so a failing
    // test does not leave a daemon squatting the name.
    struct Reaper(std::process::Child);
    impl Drop for Reaper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let mut holder = Command::new(env!("CARGO_BIN_EXE_lnsd"));
    holder
        .arg("--config")
        .arg(dir_a.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _reaper = Reaper(spawn_supervised(holder).expect("first lnsd starts"));

    assert!(
        wait_for_instance(&name, Duration::from_secs(10)),
        "the first daemon never took @rns/{name}"
    );

    let second = Command::new(env!("CARGO_BIN_EXE_lnsd"))
        .arg("--config")
        .arg(dir_b.path())
        .output()
        .expect("second lnsd runs");

    assert!(
        !second.status.success(),
        "the second daemon must not start on a name that is already served"
    );

    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains(&name),
        "stderr must name the instance, got: {stderr}"
    );
    assert!(
        stderr.contains("rnsd") || stderr.contains("lnsd"),
        "stderr must point at the daemon already holding it, got: {stderr}"
    );
    assert!(
        stderr.contains("instance_name"),
        "stderr must name the config key that resolves it, got: {stderr}"
    );
    assert!(
        !stderr.contains("AddrInUse") && !stderr.contains("SharedInstanceNameInUse {"),
        "the operator must get the sentence, not a Debug dump, got: {stderr}"
    );
}
