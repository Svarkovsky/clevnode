//! The command line as a user meets it: every exit code, through the real
//! binary.
//!
//! The unit tests in `src/` assert on the parsers and on the send loop against
//! a fake outbox. This asserts what a script sees — the exit code, and that
//! stdout stays empty — because that contract is the whole point of a
//! non-interactive subcommand and no in-process test can check the process's
//! exit status. Since 2026-08-21 stdout is empty on success too; that half
//! needs a daemon to reach and lives in `python_interop.rs`.

use std::io::Write;
use std::process::{Command, Stdio};

const LNMSG: &str = env!("CARGO_BIN_EXE_lnmsg");

/// A well-formed address that nothing in these tests can reach.
const NOWHERE: &str = "aabbccddeeff00112233445566778899";

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run `lnmsg` with an isolated state directory, feeding `stdin`.
///
/// The state directory matters: a shared one would make the tests order
/// dependent through the identity file, and would write into the developer's
/// real `~/.config/lnmsg`.
fn run(args: &[&str], stdin: &[u8]) -> Run {
    let home = tempfile::tempdir().expect("state dir");
    let mut command = Command::new(LNMSG);
    command
        .args(args)
        .env("LNMSG_HOME", home.path())
        // No inherited event-log file: these runs would append to whatever a
        // developer had set for something else.
        .env_remove("LEVICULUM_EVENT_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Supervised: a child that outlives a killed test run would hold the state
    // directory the tempdir is about to remove.
    let mut child = leviculum_std::process::spawn_supervised(command).expect("spawn lnmsg");
    // A run whose arguments are rejected exits before it ever reads stdin, so
    // this write races the child's exit and legitimately loses under load with
    // EPIPE. Losing that race IS the behaviour under test, not a failure: the
    // body was never going to be consumed. Every assertion in these tests reads
    // the exit code and stderr, neither of which depends on the write landing,
    // so a broken pipe is dropped and any other write error still panics.
    // Without this the suite went red only when the machine was busy -- which
    // is to say, only in a full workspace run.
    match child.stdin.take().expect("stdin is piped").write_all(stdin) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(e) => panic!("write stdin: {e}"),
    }
    let output = child.wait_with_output().expect("wait for lnmsg");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn help_exits_zero_and_documents_the_send_subcommand() {
    let run = run(&["--help"], b"");
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("send"), "{}", run.stdout);
    assert!(
        run.stdout.contains("does not start a Reticulum stack"),
        "the help has to say a daemon is required: {}",
        run.stdout
    );
}

#[test]
fn a_malformed_address_is_a_usage_error() {
    let run = run(&["send", "not-an-address"], b"body\n");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(
        run.stdout.is_empty(),
        "nothing may reach stdout when no message exists: {:?}",
        run.stdout
    );
    assert!(run.stderr.contains("hex"), "{}", run.stderr);
}

#[test]
fn a_short_address_is_a_usage_error_naming_the_length() {
    let run = run(&["send", "aabbccdd"], b"body\n");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("32 hex characters"), "{}", run.stderr);
}

#[test]
fn an_empty_body_is_a_usage_error() {
    let run = run(&["send", NOWHERE], b"");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("empty"), "{}", run.stderr);
}

#[test]
fn a_whitespace_only_body_is_an_empty_body() {
    let run = run(&["send", NOWHERE], b"   \n");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
}

/// The brief's rule: what is not built says so, rather than silently doing
/// something else. A script that asked for a mailbox and got a direct delivery
/// would believe an offline recipient had been reached.
#[test]
fn via_propagated_says_it_is_not_built_yet() {
    let run = run(&["send", NOWHERE, "--via", "propagated"], b"body\n");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(
        run.stderr.contains("not built yet"),
        "the refusal must name itself: {}",
        run.stderr
    );
}

#[test]
fn an_unknown_via_value_is_rejected_by_the_parser() {
    let run = run(&["send", NOWHERE, "--via", "carrier-pigeon"], b"body\n");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
}

/// With no daemon there is nothing to queue into, and the message has to say
/// what is missing and how to fix it — never fall back to a private stack.
#[test]
fn with_no_daemon_running_the_error_names_the_daemon() {
    let instance = format!("lnmsg-nothing-here-{}", std::process::id());
    let run = run(&["send", NOWHERE, "--instance", &instance], b"body\n");
    assert_eq!(run.code, Some(1), "stderr: {}", run.stderr);
    assert!(
        run.stdout.is_empty(),
        "no id may be printed when nothing was queued: {:?}",
        run.stdout
    );
    assert!(
        run.stderr.contains(&instance),
        "the error must name the instance it looked for: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("lnsd") && run.stderr.contains("rnsd"),
        "the error must say what to start: {}",
        run.stderr
    );
}

/// An empty `--from` is refused rather than silently replaced by the account
/// name: an operator who set the name explicitly has to hear that the value
/// cannot be used, not discover weeks later that the announces went out under
/// something else.
#[test]
fn an_empty_from_is_a_usage_error() {
    let run = run(&["send", NOWHERE, "--from", "", "body"], b"");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(
        run.stderr.contains("--from") && run.stderr.contains("indistinguishable"),
        "the error must name the flag and say why: {}",
        run.stderr
    );
}

#[test]
fn a_whitespace_only_from_is_the_same_error() {
    let run = run(&["send", NOWHERE, "--from", "   ", "body"], b"");
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
}

/// An empty `LNMSG_DISPLAY_NAME` is refused for the same reason. The env var
/// exists for the cron case, which is exactly where a silent fallback would go
/// unnoticed longest.
#[test]
fn an_empty_display_name_variable_is_a_usage_error() {
    let home = tempfile::tempdir().expect("state dir");
    let output = Command::new(LNMSG)
        .args(["send", NOWHERE, "body"])
        .env("LNMSG_HOME", home.path())
        .env("LNMSG_DISPLAY_NAME", "")
        .env_remove("LEVICULUM_EVENT_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("run lnmsg");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("LNMSG_DISPLAY_NAME"), "{stderr}");
}

/// Read the `from=` and `source=` fields of the run's `LNMSG_SENDER` line.
fn sender_line(log: &std::path::Path) -> (String, String) {
    let text = std::fs::read_to_string(log).expect("the event log file must exist");
    let line = text
        .lines()
        .find(|line| line.starts_with("LNMSG_SENDER "))
        .unwrap_or_else(|| panic!("no LNMSG_SENDER line in:\n{text}"));
    let field = |key: &str| {
        line.split_whitespace()
            .find_map(|token| token.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} field in {line}"))
            .to_string()
    };
    assert!(
        !text.contains("EVENT_SCHEMA_VIOLATION") && !text.contains("EVENT_FIELD_VIOLATION"),
        "the emitted events must satisfy their catalogue entries:\n{text}"
    );
    (field("from="), field("source="))
}

/// The cron case, at the level a cron job actually meets it: **no environment
/// at all**. `$USER` and `$LOGNAME` are gone, and the name still has to be the
/// account's — resolved from the password database — rather than the tool's.
///
/// `env_clear` is `env -i`: the child gets only what is set after it. The run
/// itself fails (there is no daemon), which is irrelevant here — the name is
/// resolved and logged before anything can be attached to.
#[test]
fn with_no_environment_at_all_the_name_still_comes_from_the_password_database() {
    let Some(expected) = leviculum_std::user::passwd_name() else {
        // No passwd entry for this uid: the fallback chain is what is left,
        // and it is covered by the unit tests. Nothing to prove here.
        return;
    };
    let home = tempfile::tempdir().expect("state dir");
    let log = home.path().join("events.log");

    let output = Command::new(LNMSG)
        .args([
            "send",
            NOWHERE,
            "body",
            "--instance",
            "lnmsg-no-such-daemon",
        ])
        .env_clear()
        // Only the two paths the program cannot invent: where its identity
        // lives, and where to write the log this test reads. Neither carries a
        // user name.
        .env("LNMSG_HOME", home.path())
        .env("LEVICULUM_EVENT_LOG", &log)
        .stdin(Stdio::null())
        .output()
        .expect("run lnmsg");
    assert_eq!(
        output.status.code(),
        Some(1),
        "with no daemon this fails after resolving the name: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (from, source) = sender_line(&log);
    assert_eq!(
        from, expected,
        "an empty environment must not change who we are"
    );
    assert_eq!(
        source, "passwd",
        "with USER and LOGNAME gone, only the password database can have answered"
    );
    assert_ne!(from, "lnmsg", "the tool's name is not the sender's name");
}

/// `--from` beats `LNMSG_DISPLAY_NAME`, through the real binary.
#[test]
fn the_flag_beats_the_environment_variable_in_the_shipped_binary() {
    let home = tempfile::tempdir().expect("state dir");
    let log = home.path().join("events.log");

    let output = Command::new(LNMSG)
        .args(["send", NOWHERE, "body", "--from", "hamster"])
        // A daemon may well be running on a developer's host; naming one that
        // is not keeps the run short and its exit code predictable.
        .args(["--instance", "lnmsg-no-such-daemon"])
        .env("LNMSG_HOME", home.path())
        .env("LNMSG_DISPLAY_NAME", "from-the-env")
        .env("LEVICULUM_EVENT_LOG", &log)
        .stdin(Stdio::null())
        .output()
        .expect("run lnmsg");
    assert_eq!(output.status.code(), Some(1));

    assert_eq!(
        sender_line(&log),
        ("hamster".to_string(), "flag".to_string())
    );
}

/// …and the variable beats the resolved account name.
#[test]
fn the_environment_variable_beats_the_account_name_in_the_shipped_binary() {
    let home = tempfile::tempdir().expect("state dir");
    let log = home.path().join("events.log");

    let output = Command::new(LNMSG)
        .args(["send", NOWHERE, "body"])
        .args(["--instance", "lnmsg-no-such-daemon"])
        .env("LNMSG_HOME", home.path())
        .env("LNMSG_DISPLAY_NAME", "lew_at_schneckenschreck")
        .env("LEVICULUM_EVENT_LOG", &log)
        .stdin(Stdio::null())
        .output()
        .expect("run lnmsg");
    assert_eq!(output.status.code(), Some(1));

    assert_eq!(
        sender_line(&log),
        ("lew_at_schneckenschreck".to_string(), "env".to_string())
    );
}

/// A corrupt identity file is the user's address. It must stop the program
/// rather than quietly become a different address.
#[test]
fn a_corrupt_identity_file_stops_the_run() {
    let home = tempfile::tempdir().expect("state dir");
    std::fs::write(home.path().join("identity"), b"not an identity").expect("write");

    let output = Command::new(LNMSG)
        .args(["send", NOWHERE, "hello"])
        .env("LNMSG_HOME", home.path())
        .env_remove("LEVICULUM_EVENT_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("run lnmsg");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("address"),
        "the error must explain what the file is: {stderr}"
    );
    assert_eq!(
        std::fs::read(home.path().join("identity")).expect("read back"),
        b"not an identity",
        "the file must be left for the user to rescue"
    );
}

/// `LEVICULUM_EVENT_LOG` is the switch the concept asks for: start the program
/// with a log file and every transition is in it, in the documented format.
#[test]
fn the_event_log_records_the_run_in_the_documented_format() {
    let home = tempfile::tempdir().expect("state dir");
    let log = home.path().join("events.log");

    let output = Command::new(LNMSG)
        .args(["send", "nonsense"])
        .env("LNMSG_HOME", home.path())
        .env("LEVICULUM_EVENT_LOG", &log)
        .env("LEVICULUM_EVENT_NODE", "cli-test")
        .stdin(Stdio::null())
        .output()
        .expect("run lnmsg");
    assert_eq!(output.status.code(), Some(2));

    let text = std::fs::read_to_string(&log).expect("the event log file must exist");
    let line = text
        .lines()
        .find(|line| line.starts_with("LNMSG_DONE "))
        .unwrap_or_else(|| panic!("no LNMSG_DONE line in:\n{text}"));
    // EVENT_NAME first, node= second, t= last: the format's three fixed rules.
    let fields: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(fields[0], "LNMSG_DONE");
    assert_eq!(fields[1], "node=cli-test");
    assert!(
        fields[fields.len() - 1].starts_with("t="),
        "the line must end with the relative timestamp: {line}"
    );
    assert!(line.contains("outcome=usage"), "{line}");
    assert!(line.contains("code=2"), "{line}");
    assert!(
        !text.contains("EVENT_SCHEMA_VIOLATION") && !text.contains("EVENT_FIELD_VIOLATION"),
        "the emitted events must satisfy their catalogue entries:\n{text}"
    );
}
