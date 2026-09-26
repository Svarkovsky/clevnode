//! The consuming half of `defer_resource_builds` (Codeberg #196), end to end:
//! drain → build off the lock → commit — the loopback topology of
//! `two_node_loopback.rs` with a Resource-sized body and the flag on.
//!
//! The router half (capture, epoch, `StaleBuild`) is tested in
//! `leviculum-lxmf`; what is tested here is the one consumer those APIs have:
//! the helper's dispatch, its build worker, and its single-flight bound.
//!
//! * **The wiring test** proves a message over `DIRECT_PACKET_MDU` (431
//!   bytes; these bodies are 600) is delivered *through the deferred path* —
//!   the dispatch log line is the vacuity guard, because a body under the MDU
//!   would deliver as a packet and never touch the consumer.
//! * **The single-flight test** holds the build queue in its own hands (no
//!   worker), crosses the router's 10 s re-offer interval, and requires that
//!   the re-offer was dropped rather than dispatched twice — the
//!   stamp path's known defect (`processor.rs`, duplicate `StampJob`s into an
//!   unbounded queue) not cloned. Then it plays the worker itself and sees
//!   the held build deliver.
//! * **The control** runs the identical exchange with the flag off and
//!   requires that no build is ever dispatched: today's composed path,
//!   untouched.
//!
//! A `StaleBuild` refusal cannot be arranged in this harness (no stamp costs,
//! no cancel verb, and single-flight itself keeps a second build for one id
//! from existing) — refusal semantics are covered by the router's own tests,
//! and the helper-side half, "a refusal is a log line and never `lxmf_error`",
//! is a unit test beside `report_commit_refusal` in `processor.rs`.

mod common;

use std::time::{Duration, Instant};

use common::{body_b64, pump_until, Helper, Setup, Wire};
use leviculum_lxmf_node::processor::{BuildJob, Input};

/// Over `DIRECT_PACKET_MDU` (431 bytes, `leviculum-lxmf/src/node.rs`), so the
/// message takes the `DirectResource` representation — the only
/// representation the deferred path exists for.
fn resource_sized_body() -> String {
    "the deferred resource payload, well over one packet. "
        .repeat(12)
        .chars()
        .take(600)
        .collect()
}

/// Bring up the connected pair, announce both, and resolve `wait_for_peer`
/// both ways. `alice` is the sender under test; `bob` is always a stock
/// helper (flag off, worker on), because the receive direction is the
/// control, not the subject.
async fn bring_up(defer: bool, build_worker: bool) -> (Helper, Helper, String, String) {
    // Alice listens on `:0` and reports her port; Bob dials it (Codeberg
    // #221 — no free-port probe, so no probe-to-bind window).
    let mut alice_setup = Setup::new("Alice", Wire::listen_any());
    alice_setup.defer_resource_builds = defer;
    alice_setup.build_worker = build_worker;
    let mut alice = Helper::start(alice_setup).await;
    let mut bob = Helper::start(Setup::new("Bob", Wire::Dial(alice.listen_addr()))).await;

    let ready = pump_until(&mut alice, &mut bob, Duration::from_secs(20), |a, b| {
        a.delivery_hash.is_some() && b.delivery_hash.is_some()
    })
    .await;
    assert!(ready, "both helpers must emit lxmf_ready");
    let alice_hash = alice.delivery_hash.clone().unwrap();
    let bob_hash = bob.delivery_hash.clone().unwrap();

    alice.command(&format!("wait_for_peer {bob_hash} 10"));
    bob.command(&format!("wait_for_peer {alice_hash} 10"));
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut next_announce = Instant::now();
    loop {
        if Instant::now() >= next_announce {
            alice.command("announce");
            bob.command("announce");
            next_announce = Instant::now() + Duration::from_secs(3);
        }
        alice.drain();
        bob.drain();
        if alice.seen("lxmf_wait_for_peer_ok") && bob.seen("lxmf_wait_for_peer_ok") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "both helpers must resolve wait_for_peer over loopback"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (alice, bob, alice_hash, bob_hash)
}

async fn stop(mut alice: Helper, mut bob: Helper) {
    let _ = alice.node.stop().await;
    let _ = bob.node.stop().await;
}

/// The wiring test: with the flag on, a Resource-sized message goes drain →
/// build → commit and arrives body-exact. Red while the consumer does not
/// exist — the router captures the build, nothing drains it, and the message
/// strands exactly as `~/.local/state/leviculum/instructions.md` describes.
#[tokio::test]
async fn a_resource_sized_message_delivers_with_deferral_on() {
    let (mut alice, mut bob, alice_hash, bob_hash) = bring_up(true, true).await;
    let body = resource_sized_body();

    alice.command(&format!("send {bob_hash} {}", body_b64(&body)));
    let delivered = pump_until(&mut alice, &mut bob, Duration::from_secs(60), |_, b| {
        b.received(&alice_hash, &body_b64(&body)).is_some()
    })
    .await;
    assert!(
        delivered,
        "the deferred Resource message must arrive body-exact; alice logs: {:?}",
        alice.logs
    );

    // Vacuity guard: the delivery must have gone through the deferred path.
    // A packet-sized body, or a silently ignored flag, would deliver green
    // without ever dispatching a build.
    assert!(
        alice.logs_containing("resource build dispatched") >= 1,
        "the sender must have dispatched at least one build job: {:?}",
        alice.logs
    );
    for (who, helper) in [("alice", &alice), ("bob", &bob)] {
        assert!(
            !helper.seen("lxmf_error"),
            "{who} reported an error: {:?}",
            helper.find("lxmf_error")
        );
    }
    stop(alice, bob).await;
}

/// Single-flight: while one build job is in flight, the router's re-offer
/// (every 10 s while the entry stays due) must be dropped at dispatch, not
/// queued as a second job. The test is the worker: it holds the job across
/// the re-offer interval, then builds and returns it, and the held build
/// still delivers.
#[tokio::test]
async fn a_held_build_gets_no_second_job_and_still_delivers() {
    let (mut alice, mut bob, alice_hash, bob_hash) = bring_up(true, false).await;
    let body = resource_sized_body();

    alice.command(&format!("send {bob_hash} {}", body_b64(&body)));

    // The dispatch: one job must reach the (test-held) build queue.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut job = None;
    while job.is_none() {
        alice.drain();
        bob.drain();
        job = alice.builds.as_ref().unwrap().try_recv().ok();
        assert!(
            Instant::now() < deadline,
            "a build job must be dispatched: {:?}",
            alice.logs
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let BuildJob::Resource(pending) = job.unwrap();

    // The hold: 12 s crosses the router's 10 s retry interval, so the entry
    // comes due again and the tick re-captures it. That re-offer must be
    // dropped by the in-flight set...
    let hold_until = Instant::now() + Duration::from_secs(12);
    while Instant::now() < hold_until {
        alice.drain();
        bob.drain();
        assert!(
            alice.builds.as_ref().unwrap().try_recv().is_err(),
            "a second build job was dispatched while the first was in flight"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // ...and the drop must actually have happened, or the hold proved
    // nothing: a run where the router never re-offered would pass vacuously.
    assert!(
        alice.logs_containing("already in flight") >= 1,
        "the router must have re-offered the build during the hold: {:?}",
        alice.logs
    );

    // The test plays the worker: build off the lock, hand the result back.
    let built = pending
        .build(&mut rand_core::OsRng)
        .expect("the held build must still build");
    alice
        .inputs
        .send(Input::ResourceBuildReady {
            built: Box::new(built),
        })
        .expect("helper input channel is open");

    let delivered = pump_until(&mut alice, &mut bob, Duration::from_secs(60), |_, b| {
        b.received(&alice_hash, &body_b64(&body)).is_some()
    })
    .await;
    assert!(
        delivered,
        "the held build must commit and deliver; alice logs: {:?}",
        alice.logs
    );
    assert!(
        !alice.seen("lxmf_error"),
        "no error may surface: {:?}",
        alice.find("lxmf_error")
    );
    stop(alice, bob).await;
}

/// The control: flag off, same Resource-sized body, worker withheld so a
/// dispatch would be visible. Delivery must come through today's composed
/// path with the consumer never involved — byte-identical behaviour to the
/// tree before this change.
#[tokio::test]
async fn flag_off_dispatches_no_builds_and_still_delivers() {
    let (mut alice, mut bob, alice_hash, bob_hash) = bring_up(false, false).await;
    let body = resource_sized_body();

    alice.command(&format!("send {bob_hash} {}", body_b64(&body)));
    let delivered = pump_until(&mut alice, &mut bob, Duration::from_secs(60), |_, b| {
        b.received(&alice_hash, &body_b64(&body)).is_some()
    })
    .await;
    assert!(
        delivered,
        "the composed path must deliver with the flag off: {:?}",
        alice.logs
    );
    assert!(
        alice.builds.as_ref().unwrap().try_recv().is_err(),
        "no build job may be dispatched with the flag off"
    );
    assert_eq!(
        alice.logs_containing("resource build"),
        0,
        "no deferred-path diagnostic may appear with the flag off: {:?}",
        alice.logs
    );
    for (who, helper) in [("alice", &alice), ("bob", &bob)] {
        assert!(
            !helper.seen("lxmf_error"),
            "{who} reported an error: {:?}",
            helper.find("lxmf_error")
        );
    }
    stop(alice, bob).await;
}
