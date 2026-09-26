//! A second daemon on an instance name that is already taken must fail,
//! and must say why in a sentence an operator can act on.
//!
//! The instance name *is* the identity of a shared instance, so two
//! daemons serving one name is a contradiction rather than a topology —
//! two daemons under *different* names is the supported case, covered by
//! `interfaces::local::tests::test_two_instances_different_names_no_collision`.
//! Refusing is therefore correct. What was not correct is how the refusal
//! reached the operator: the raw `io::Error` travelled up through
//! `driver::initialize_interfaces` and out of `lnsd`'s `main`, which prints
//! the Debug form, so a packaged daemon that landed on an already-served
//! name reported
//!
//! ```text
//! Error: Io(Os { code: 98, kind: AddrInUse, message: "Address in use" })
//! ```
//!
//! and restarted every five seconds forever. That names neither the
//! instance nor the remedy.
//!
//! Deliberately Python-free and network-free: the failure is entirely
//! local to the abstract socket bind, so the reproduction needs one
//! process and no daemon under test but our own.

use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::Error;

/// Build a transport-less shared-instance daemon under `name`.
///
/// Transport is off and no interfaces are added: the bind of the
/// shared-instance socket is the only thing under test, and adding a
/// listener would just introduce a second way for the test to fail.
async fn daemon_for(
    name: &str,
    storage: &tempfile::TempDir,
) -> leviculum_std::driver::ReticulumNode {
    ReticulumNodeBuilder::new()
        .enable_transport(false)
        .share_instance(true)
        .instance_name(name.to_string())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("daemon builds")
}

#[tokio::test]
async fn second_daemon_on_a_taken_instance_name_says_so() {
    // Keyed by pid so parallel test binaries cannot collide with each
    // other, which would make this test pass for the wrong reason.
    let name = format!("conflict_{}", std::process::id());

    let storage_a = tempfile::tempdir().expect("temp storage");
    let storage_b = tempfile::tempdir().expect("temp storage");

    let mut first = daemon_for(&name, &storage_a).await;
    first
        .start()
        .await
        .expect("the first daemon takes the instance name");

    let mut second = daemon_for(&name, &storage_b).await;
    let err = second
        .start()
        .await
        .expect_err("the second daemon must not take a name that is already served");

    match &err {
        Error::SharedInstanceNameInUse { name: reported } => {
            assert_eq!(reported, &name, "the error names the instance it tried");
        }
        other => panic!("expected SharedInstanceNameInUse, got {other:?}"),
    }

    // The wording is part of the fix, not decoration: this string is what
    // an operator finds in `systemctl status` after an install that
    // landed on a host already running a daemon.
    let message = err.to_string();
    assert!(
        message.contains(&name),
        "the message must name the instance, got: {message}"
    );
    assert!(
        message.contains("lnsd") || message.contains("rnsd"),
        "the message must point at the daemon already holding it, got: {message}"
    );
    assert!(
        message.contains("instance_name"),
        "the message must name the config key that resolves it, got: {message}"
    );
    assert!(
        !message.contains("AddrInUse"),
        "the raw io error must not be what reaches the operator, got: {message}"
    );
}
