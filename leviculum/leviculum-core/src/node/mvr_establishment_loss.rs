//! mvr: deterministic characterization of what the link state machine does when
//! the establishment handshake loses exactly one packet (the LinkRequest or the
//! proof).
//!
//! Motivation (reviewer-established, instrumented on `dbg-init-announce-jitter`):
//! the foundational cold-LoRa-link bug fails as
//! `LINK_DIED reason=other detail=handshake_timeout` — path discovery succeeds
//! but the LinkRequest -> proof handshake does not complete over the cold lossy
//! multi-hop path. An announce-jitter A/B over RF was inconclusive (1/8 vs 0/8,
//! p~=1.0): the end-to-end PDR metric is too noisy to resolve the mechanism. So
//! we attack the mechanism DETERMINISTICALLY here, with no RF, no Docker, no
//! Python, sub-second wall clock.
//!
//! The question this module answers: *when the handshake loses exactly one
//! packet, does the initiator retransmit the LinkRequest, does the responder
//! retransmit the proof, or is there exactly ONE attempt that then times out?*
//!
//! Loss injection is trivial in this sans-I/O harness: per-packet delivery is
//! scripted (the same pattern as `mvr_lrproof`), so "dropping" a packet is
//! simply declining to route it to the peer. A `MockClock` is advanced past
//! `Link::establishment_timeout_ms()` to drive `NodeCore::handle_timeout()` ->
//! `check_timeouts()` without sleeping the real ~12 s timeout.
//!
//! Finding (asserted below): the state machine is NOT one-shot. On
//! establishment timeout the initiator REGENERATES its ephemeral keys and
//! RETRANSMITS the LinkRequest (Codeberg #66), up to
//! `max(LINK_REQUEST_MAX_RETRIES, hops)` times before emitting
//! `LINK_DIED detail=handshake_timeout`. A SINGLE proof or request loss is
//! therefore RECOVERED by the retransmit, not fatal. Only persistent loss that
//! outlasts every retry reaches the timeout-death path. This characterises that
//! a Python-compatible establishment retransmit already exists; the lever for
//! the cold-link bug is the retry COUNT / TIMING, not the absence of a retry.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::LINK_REQUEST_MAX_RETRIES;
use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::LinkCloseReason;
use crate::link::LinkId;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::NoStorage;
use crate::transport::{Action, InterfaceId, TickOutput};

// ----------------------------------------------------------------------------
// Tracing capture (prove the EXACT death reason, not just "no link").
// ----------------------------------------------------------------------------

// All capture routes through the shared global-subscriber helper so a callsite
// registered by an earlier parallel test cannot hide events. The field
// `RUST_LOG` shape is reproduced by filtering the CAPTURED lines afterwards,
// never by installing a per-test scoped `EnvFilter` subscriber (which would
// leave callsite Interest poisoned for later captures, see `test_log_capture`).
use crate::test_log_capture::with_captured_logs;

/// The EXACT `RUST_LOG` filter the cold-8node integ run uses
/// (periculum `hardware/lora_late_announce_8node.toml`). It enables
/// `leviculum_core::link=debug` (so `LINK_DIED` and the TX/RX handshake events
/// are visible) but does NOT enable `leviculum_core::node::link_management`,
/// where the retry / timeout diagnostics ("retrying with fresh keys",
/// "establishment timed out (no retries left)") are emitted. Those debug lines
/// fall through to the `info` catch-all and are dropped.
const FIELD_RUST_LOG: &str = "leviculum_core::link::channel=debug,leviculum_core::link=debug,\
     leviculum_core::transport=debug,leviculum_std::interfaces=debug,info";

/// Would the field `RUST_LOG` ([`FIELD_RUST_LOG`]) have emitted this captured
/// line? Parses the shared capture's fmt layout (`<timestamp> LEVEL target:
/// message`; the capture never enters spans) and applies `EnvFilter`'s
/// target semantics: the most specific matching `target=level` directive wins,
/// the bare level directive is the default for everything else.
fn field_rust_log_enables(line: &str) -> bool {
    let mut tokens = line.split_whitespace();
    let _timestamp = tokens.next();
    let Some(level) = tokens.next().and_then(|t| t.parse::<tracing::Level>().ok()) else {
        return false;
    };
    let Some(target) = tokens.next().and_then(|t| t.strip_suffix(':')) else {
        return false;
    };

    let mut max_level = tracing::Level::ERROR;
    let mut best_prefix_len = None;
    for directive in FIELD_RUST_LOG.split(',') {
        let directive = directive.trim();
        let (prefix, level_name) = match directive.split_once('=') {
            Some((prefix, level_name)) => (Some(prefix), level_name),
            None => (None, directive),
        };
        if let Some(prefix) = prefix {
            let matches = target == prefix
                || (target.starts_with(prefix) && target[prefix.len()..].starts_with("::"));
            if !matches || best_prefix_len.is_some_and(|best| prefix.len() <= best) {
                continue;
            }
            best_prefix_len = Some(prefix.len());
        } else if best_prefix_len.is_some() {
            continue;
        }
        max_level = level_name.parse().expect("FIELD_RUST_LOG level must parse");
    }
    level <= max_level
}

/// Run `body` with tracing captured through the shared global subscriber, then
/// reduce the captured lines to the SAME set the field `RUST_LOG` emitted. Used
/// to reproduce the field log shape (death with zero *visible* retransmits)
/// verbatim, proving it is an observability artifact of the filter rather than
/// a zero-retransmit code path. Filtering the captured text keeps the
/// process-global callsite Interest cache untouched; a scoped `EnvFilter`
/// subscriber here would re-introduce the parallel-scheduling flake that
/// `test_log_capture` exists to prevent.
fn with_field_filtered_logs<R>(body: impl FnOnce() -> R) -> (R, String) {
    let (out, logs) = with_captured_logs(body);
    let mut filtered = String::new();
    for line in logs.lines().filter(|line| field_rust_log_enables(line)) {
        filtered.push_str(line);
        filtered.push('\n');
    }
    (out, filtered)
}

// ----------------------------------------------------------------------------
// Sans-I/O node helpers (direct initiator <-> responder, one mesh hop).
// ----------------------------------------------------------------------------

type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

/// Register a named interface on a NodeCore and return its index.
fn add_iface(node: &mut EndpointNode, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

/// All bytes the node wants to put on the wire this step (SendPacket + Broadcast).
fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

/// Single outbound packet; panics if not exactly one.
fn one_packet(output: &TickOutput) -> Vec<u8> {
    let data = action_data(output);
    assert_eq!(
        data.len(),
        1,
        "expected exactly one outbound packet, got {}",
        data.len()
    );
    data.into_iter().next().unwrap()
}

fn has_link_established(output: &TickOutput) -> bool {
    output
        .events
        .iter()
        .any(|e| matches!(e, NodeEvent::LinkEstablished { .. }))
}

fn has_timeout_close(output: &TickOutput) -> bool {
    output.events.iter().any(|e| {
        matches!(
            e,
            NodeEvent::LinkClosed {
                reason: LinkCloseReason::Timeout,
                ..
            }
        )
    })
}

/// Build a responder node owning a link-accepting destination.
/// Returns (node, dest_hash, signing_key).
fn make_responder() -> (EndpointNode, crate::DestinationHash, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["establoss"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();

    node.register_destination(dest);
    (node, dest_hash, signing_key)
}

fn make_initiator() -> EndpointNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().build(OsRng, clock, NoStorage)
}

/// Advance the initiator's virtual clock just past its current establishment
/// timeout and run one maintenance tick. Returns the tick output (which carries
/// any retransmitted LinkRequest as an action, or the LinkClosed event on the
/// final timeout). Reads the live timeout from the (possibly re-keyed) link so
/// the test never hardcodes the constant.
fn tick_past_establishment_timeout(
    initiator: &mut EndpointNode,
    caller_link_id: &LinkId,
) -> TickOutput {
    let timeout_ms = initiator
        .link(caller_link_id)
        .expect("link must still be pending")
        .establishment_timeout_ms();
    initiator.transport().clock().advance(timeout_ms + 1);
    initiator.handle_timeout()
}

// ----------------------------------------------------------------------------
// (baseline) No loss -> the link establishes.
// ----------------------------------------------------------------------------

/// Control: with every packet delivered, a single establishment round trip
/// (request -> proof) establishes the link. Proves the harness drives a real
/// establishment and anchors the loss cases below. Also asserts the four
/// per-side packet-level instrumentation events fire (initiator TX/RX and
/// responder RX/TX), documenting which side emits each for the hardware run.
#[test]
fn establishment_baseline_no_loss_establishes() {
    let ((), logs) = with_captured_logs(|| {
        let (mut responder, dest_hash, signing_key) = make_responder();
        let mut initiator = make_initiator();
        let r_iface = add_iface(&mut responder, "R_mesh");
        let i_iface = add_iface(&mut initiator, "I_mesh");

        // Initiator connects -> LinkRequest (broadcast, no path known).
        let (_link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
        let request = one_packet(&out);

        // Responder receives the request and auto-accepts (Stage 1): the proof
        // is produced directly by handle_packet, no accept_link call needed.
        let out = responder.handle_packet(InterfaceId(r_iface), &request);
        let proof = one_packet(&out);

        // Initiator validates the proof -> link established.
        let out = initiator.handle_packet(InterfaceId(i_iface), &proof);
        assert!(
            has_link_established(&out),
            "baseline: link must establish when no packet is lost"
        );
        assert_eq!(initiator.active_link_count(), 1);
    });

    // Per-side establishment instrumentation must trace every handshake packet.
    for marker in [
        "LINK_REQUEST_TX", // initiator
        "LINK_REQUEST_RX", // responder
        "LINK_PROOF_TX",   // responder
        "LINK_PROOF_RX",   // initiator
    ] {
        assert!(
            logs.contains(marker),
            "baseline must emit the {marker} establishment event.\n--- logs ---\n{logs}"
        );
        assert!(
            logs.contains(&std::format!("{marker} link=")) && logs.contains("t_ms="),
            "{marker} must carry a link id and a t_ms timestamp.\n--- logs ---\n{logs}"
        );
    }
}

// ----------------------------------------------------------------------------
// (proof dropped once) The responder's proof is lost a single time.
// ----------------------------------------------------------------------------

/// KEY CHARACTERIZATION. The responder's proof is dropped exactly once. The
/// hypothesis under test was "one attempt, no establishment retransmit, then
/// ESTABLISHMENT_TIMEOUT". The ASSERTED observed behaviour REFUTES it:
///   * the initiator emits EXACTLY ONE retransmitted LinkRequest on the
///     establishment timeout (proving an establishment-level retransmit exists),
///   * that retransmit carries FRESH ephemeral keys / a new link id (Codeberg
///     #66) and is logged as "retrying with fresh keys",
///   * the responder accepts the retransmitted request and its second proof IS
///     delivered, so the link ESTABLISHES.
///
/// Conclusion: a single proof loss is RECOVERED by the retransmit, not fatal.
#[test]
fn establishment_proof_dropped_once_recovers_via_retransmit() {
    let ((), logs) = with_captured_logs(|| {
        let (mut responder, dest_hash, signing_key) = make_responder();
        let mut initiator = make_initiator();
        let r_iface = add_iface(&mut responder, "R_mesh");
        let i_iface = add_iface(&mut initiator, "I_mesh");

        let (link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
        let request = one_packet(&out);

        // Responder auto-accepts (Stage 1) and builds the proof...
        let out = responder.handle_packet(InterfaceId(r_iface), &request);
        let _dropped_proof = one_packet(&out); // DROP: never delivered to initiator.

        // Initiator's establishment timer fires -> it must retransmit.
        let out = tick_past_establishment_timeout(&mut initiator, &link_id);
        let retried = action_data(&out);
        assert_eq!(
            retried.len(),
            1,
            "initiator must retransmit EXACTLY ONE LinkRequest on establishment timeout \
             (proves an establishment-level retransmit exists)"
        );
        assert!(
            !has_timeout_close(&out),
            "the first establishment timeout must retransmit, NOT close the link \
             (retries remain)"
        );
        let retried_request = retried.into_iter().next().unwrap();
        assert_ne!(
            retried_request, request,
            "the retransmit must carry fresh bytes (re-keyed), not an identical resend"
        );

        // Responder treats the fresh-keys request as a new link and proves it;
        // this second proof IS delivered -> the link recovers and establishes.
        let out = responder.handle_packet(InterfaceId(r_iface), &retried_request);
        let proof2 = one_packet(&out);

        let out = initiator.handle_packet(InterfaceId(i_iface), &proof2);
        assert!(
            has_link_established(&out),
            "a SINGLE proof loss must be RECOVERED by the establishment retransmit"
        );
        assert_eq!(initiator.active_link_count(), 1);
    });

    assert!(
        logs.contains("retrying with fresh keys"),
        "the retransmit must be the Codeberg #66 fresh-keys retry.\n--- logs ---\n{logs}"
    );
}

// ----------------------------------------------------------------------------
// (LinkRequest dropped once) The initiator's LinkRequest is lost a single time.
// ----------------------------------------------------------------------------

/// Mirror of the proof-loss case for the other establishment packet: the
/// initiator's LinkRequest never reaches the responder. Same observed
/// behaviour: the initiator retransmits exactly one fresh-keys LinkRequest on
/// the establishment timeout, the retransmit reaches the responder, and the
/// link establishes. A single request loss is RECOVERED, not fatal.
#[test]
fn establishment_link_request_dropped_once_recovers_via_retransmit() {
    let ((), logs) = with_captured_logs(|| {
        let (mut responder, dest_hash, signing_key) = make_responder();
        let mut initiator = make_initiator();
        let r_iface = add_iface(&mut responder, "R_mesh");
        let i_iface = add_iface(&mut initiator, "I_mesh");

        let (link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
        let _dropped_request = one_packet(&out); // DROP: never delivered to responder.

        // The responder never saw the request, so it has no pending link.
        assert_eq!(
            responder.pending_link_count(),
            0,
            "responder must not have seen the dropped request"
        );

        // Initiator's establishment timer fires -> retransmit.
        let out = tick_past_establishment_timeout(&mut initiator, &link_id);
        let retried = action_data(&out);
        assert_eq!(
            retried.len(),
            1,
            "initiator must retransmit EXACTLY ONE LinkRequest after the request was lost"
        );
        assert!(
            !has_timeout_close(&out),
            "first timeout retransmits, not closes"
        );
        let retried_request = retried.into_iter().next().unwrap();

        // This retransmit IS delivered -> the responder auto-proves it -> establishes.
        let out = responder.handle_packet(InterfaceId(r_iface), &retried_request);
        let proof = one_packet(&out);

        let out = initiator.handle_packet(InterfaceId(i_iface), &proof);
        assert!(
            has_link_established(&out),
            "a SINGLE LinkRequest loss must be RECOVERED by the establishment retransmit"
        );
        assert_eq!(initiator.active_link_count(), 1);
    });

    assert!(
        logs.contains("retrying with fresh keys"),
        "the retransmit must be the Codeberg #66 fresh-keys retry.\n--- logs ---\n{logs}"
    );
}

// ----------------------------------------------------------------------------
// (persistent loss) Every proof is lost -> the timeout-death path is reached.
// ----------------------------------------------------------------------------

/// The timeout-death path that the field bug actually hits. EVERY proof is
/// dropped (loss outlasts every retry). The initiator retransmits the
/// LinkRequest a bounded number of times and then, with no retries left, emits
/// `LINK_DIED reason=other detail=handshake_timeout` and a `LinkClosed(Timeout)`
/// event. Asserts the exact attempt budget: `1 + max(LINK_REQUEST_MAX_RETRIES,
/// hops)` total LinkRequests (initial + retries). For this direct 1-hop link
/// that is `1 + 2 = 3`. This is the deterministic stand-in for the cold-link
/// `handshake_timeout` failure: the lever is the retry budget vs. the path's
/// loss rate, not a missing retransmit.
#[test]
fn establishment_persistent_proof_loss_dies_after_bounded_retries() {
    let (died, logs) = with_captured_logs(|| {
        let (mut responder, dest_hash, signing_key) = make_responder();
        let mut initiator = make_initiator();
        let r_iface = add_iface(&mut responder, "R_mesh");

        let (link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
        let request = one_packet(&out);

        // Responder auto-accepts (Stage 1) and proves once; that proof is dropped.
        let _ = responder.handle_packet(InterfaceId(r_iface), &request); // proof dropped.

        let mut died = false;

        // Drive timeouts until the link dies. Each retransmit is also dropped
        // (we never route it onward), so loss is persistent. Bounded loop with a
        // generous cap so a regression that loops forever fails loudly.
        for _ in 0..16 {
            if initiator.link(&link_id).is_none() {
                break;
            }
            let out = tick_past_establishment_timeout(&mut initiator, &link_id);
            if has_timeout_close(&out) {
                died = true;
                break;
            }
        }
        died
    });

    assert!(
        died,
        "persistent proof loss must end in the LinkClosed(Timeout) death path.\n--- logs ---\n{logs}"
    );

    // Count the establishment retransmits unambiguously from the retry log line
    // (the raw outbound packet count is contaminated by unrelated path-request
    // rebroadcasts that `handle_timeout` also emits over the elapsed virtual
    // time). hops == 1 for this direct link, so the retry budget is
    // max(LINK_REQUEST_MAX_RETRIES, 1) == LINK_REQUEST_MAX_RETRIES.
    let retransmits = logs.matches("retrying with fresh keys").count() as u64;
    let expected_retransmits = core::cmp::max(LINK_REQUEST_MAX_RETRIES as u64, 1);
    assert_eq!(
        retransmits, expected_retransmits,
        "initiator must retransmit the LinkRequest exactly {expected_retransmits} times \
         (1 initial + {expected_retransmits} retries = {} total attempts) before death.\n--- logs ---\n{logs}",
        expected_retransmits + 1
    );

    assert!(
        logs.contains("detail=handshake_timeout"),
        "death must be reported as LINK_DIED detail=handshake_timeout (the field symptom).\n--- logs ---\n{logs}"
    );
}

// ----------------------------------------------------------------------------
// (Stage 1 auto-accept) The `lnstest selftest` Phase-3 acceptor bug — a ONE-SHOT
// responder that proved only the FIRST LinkRequest and deadlocked under
// first-proof-loss + re-key (reviewer-confirmed on the LoRa rig:
// `lr_rx=4 proof_tx=1 retransmits=3`) — is eliminated at the core. With
// auto-accept LIVE (Python parity), `handle_link_request` proves EVERY inbound
// request, so each re-keyed retry is proved automatically with NO accept_link
// call anywhere. The one-shot acceptance pattern that produced the deadlock is
// no longer expressible at this layer.
// ----------------------------------------------------------------------------

/// TEST A (auto-accept). A responder destination relying on the DEFAULT
/// `accepts_links` flag (Stage 1 = true; `set_accepts_links` is never called)
/// establishes a link with NO `accept_link` call. The first proof is dropped, the
/// initiator re-keys/retransmits (Codeberg #66), and the stack AUTO-proves the
/// re-keyed retry too — its proof IS delivered and the link establishes. This is
/// the auto-accept replacement for the former one-shot/loop-accept characterisation:
/// the responder now proofs more than once (the re-keyed retry included) without
/// any application acceptance step.
#[test]
fn auto_accept_default_proves_rekeyed_retry_and_establishes() {
    let ((), logs) = with_captured_logs(|| {
        // Responder destination using the DEFAULT accepts_links flag — note that
        // set_accepts_links is NEVER called here, proving the Python-parity default.
        let identity = Identity::generate(&mut OsRng);
        let signing_key = identity.ed25519_verifying().to_bytes();
        let clock = MockClock::new(TEST_TIME_MS);
        let mut responder = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);
        let dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "mvrapp",
            &["autoacc"],
        )
        .unwrap();
        assert!(
            dest.accepts_links(),
            "Stage 1: a fresh Destination must accept links by default"
        );
        let dest_hash = *dest.hash();
        responder.register_destination(dest);

        let mut initiator = make_initiator();
        let r_iface = add_iface(&mut responder, "R_mesh");
        let i_iface = add_iface(&mut initiator, "I_mesh");

        let (link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
        let request = one_packet(&out);

        // Auto-accept: handle_packet proves the link inline (no accept_link call).
        // This first proof is dropped.
        let out = responder.handle_packet(InterfaceId(r_iface), &request);
        let _dropped_proof = one_packet(&out); // DROP: never delivered to initiator.

        // Initiator re-keys on the establishment timeout.
        let out = tick_past_establishment_timeout(&mut initiator, &link_id);
        let retried = action_data(&out)
            .into_iter()
            .next()
            .expect("re-keyed LinkRequest");

        // The re-keyed retry is AUTO-proved too — again with no accept_link call.
        let out = responder.handle_packet(InterfaceId(r_iface), &retried);
        let proof2 = one_packet(&out);

        // Delivered -> the link establishes.
        let out = initiator.handle_packet(InterfaceId(i_iface), &proof2);
        assert!(
            has_link_established(&out),
            "auto-accept must establish the link via the re-keyed retry, no accept_link"
        );
        assert_eq!(initiator.active_link_count(), 1);
    });

    // The responder proved BOTH the first request and the re-keyed retry — the
    // behaviour the former loop-accept fix needed, now automatic.
    assert!(
        logs.matches("LINK_PROOF_TX").count() >= 2,
        "auto-accept must prove the first request AND the re-keyed retry \
         (proof_tx >= 2), with no accept_link call.\n--- logs ---\n{logs}"
    );
}

/// TEST B (OFF switch). A destination with `set_accepts_links(false)` emits NO
/// proof and creates NO link on an inbound request — the gate in
/// `handle_link_request` returns before insert and before auto-prove.
#[test]
fn destination_rejecting_links_emits_no_proof_and_no_link() {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut responder = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["noaccept"],
    )
    .unwrap();
    dest.set_accepts_links(false); // OFF switch
    let dest_hash = *dest.hash();
    responder.register_destination(dest);

    let mut initiator = make_initiator();
    let r_iface = add_iface(&mut responder, "R_mesh");

    let (_link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
    let request = one_packet(&out);

    let out = responder.handle_packet(InterfaceId(r_iface), &request);

    assert!(
        action_data(&out).is_empty(),
        "a rejecting destination must emit NO establishment proof"
    );
    assert_eq!(
        responder.pending_link_count(),
        0,
        "a rejecting destination must not create a link"
    );
    assert_eq!(responder.active_link_count(), 0);
}

// ----------------------------------------------------------------------------
// (field log artifact) The cold-8node log shows LINK_DIED with ZERO visible
// "retrying with fresh keys" lines. That is NOT a zero-retransmit code path: it
// is the field RUST_LOG filtering out the retry diagnostics' tracing target.
// ----------------------------------------------------------------------------

/// Drive a 1-hop initiator->responder establishment under PERSISTENT proof loss
/// until the link reaches the timeout-death path. Same mechanism as
/// `establishment_persistent_proof_loss_dies_after_bounded_retries`, factored so
/// the characterisation below can replay the identical run under two different
/// tracing filters. Returns whether the link died (it always should within the
/// bounded retry budget). The initiator's `connect()` populates
/// `link_retry_state` with `max(LINK_REQUEST_MAX_RETRIES, hops)` — the very
/// budget the field path was hypothesised to bypass; it does not.
fn drive_persistent_proof_loss_to_death() -> bool {
    let (mut responder, dest_hash, signing_key) = make_responder();
    let mut initiator = make_initiator();
    let r_iface = add_iface(&mut responder, "R_mesh");

    let (link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
    let request = one_packet(&out);

    // Responder auto-accepts (Stage 1) and proves once; that proof (and every
    // retried proof) is dropped, so loss is persistent and the budget is exhausted.
    let _ = responder.handle_packet(InterfaceId(r_iface), &request);

    for _ in 0..16 {
        if initiator.link(&link_id).is_none() {
            break;
        }
        let out = tick_past_establishment_timeout(&mut initiator, &link_id);
        if has_timeout_close(&out) {
            return true;
        }
    }
    false
}

/// FIELD REPRODUCTION (characterisation, not a fix). Reproduces the misleading
/// shape of the cold-8node `exec_a1` log: `LINK_DIED detail=handshake_timeout`
/// with ZERO "retrying with fresh keys" lines preceding it. The reviewer read
/// that absence as "retries_left=0, zero retransmits, death at the FIRST
/// establishment timeout" — i.e. the field link-open path bypassing the retry
/// budget. This test refutes that reading deterministically:
///
///   * CONTROL — under full DEBUG capture the IDENTICAL run visibly retransmits
///     `max(LINK_REQUEST_MAX_RETRIES, hops)` times (Codeberg #66 fresh keys)
///     before it dies. The retries DO engage on the `connect()` path.
///   * FIELD — replayed under the verbatim field `RUST_LOG`, the same run still
///     retransmits and still dies, yet the captured log contains ONLY the
///     `LINK_DIED` line (target `leviculum_core::link`, which the filter
///     enables) and NONE of the retry/no-retries-left diagnostics (target
///     `leviculum_core::node::link_management`, which the filter drops to
///     `info`). The "zero retransmit" observation is therefore a tracing-target
///     artifact, present even though the retransmits happened.
///
/// Conclusion locked by this test: the field path (`lnstest selftest` ->
/// `Node::connect` -> `NodeCore::connect`) sets the retry budget exactly like
/// every other caller; the cold-link death is NOT explained by an unpopulated
/// `link_retry_state`. The retry budget vs. the path's loss rate remains the
/// lever, as the persistent-loss mvr already characterised.
#[test]
fn establishment_field_zero_retransmit_log_is_filter_artifact() {
    // CONTROL: full debug shows the retransmits explicitly.
    let (died_full, full_logs) = with_captured_logs(drive_persistent_proof_loss_to_death);
    assert!(
        died_full,
        "control: persistent proof loss must reach the death path.\n--- logs ---\n{full_logs}"
    );
    // hops == 1 for this direct link -> budget == max(LINK_REQUEST_MAX_RETRIES, 1).
    let expected_retransmits = core::cmp::max(LINK_REQUEST_MAX_RETRIES as usize, 1);
    assert_eq!(
        full_logs.matches("retrying with fresh keys").count(),
        expected_retransmits,
        "control: the run retransmits {expected_retransmits}x under full debug \
         (the retry budget DID engage on the connect path).\n--- logs ---\n{full_logs}"
    );

    // FIELD: identical run, captured through the verbatim field RUST_LOG.
    let (died_field, field_logs) = with_field_filtered_logs(drive_persistent_proof_loss_to_death);
    assert!(
        died_field,
        "field filter: the link must still die (behaviour is identical).\n--- logs ---\n{field_logs}"
    );

    // The death IS visible (its target is leviculum_core::link) — exactly the one
    // line the field log surfaced.
    assert!(
        field_logs.contains("LINK_DIED") && field_logs.contains("detail=handshake_timeout"),
        "field filter must still surface LINK_DIED (target leviculum_core::link).\n--- logs ---\n{field_logs}"
    );

    // ...but the retransmits are INVISIBLE under this filter, reproducing the
    // field log's "zero retransmits" verbatim even though they happened.
    assert!(
        !field_logs.contains("retrying with fresh keys"),
        "field filter must HIDE the retransmit diagnostics (target \
         leviculum_core::node::link_management), reproducing the misleading field log.\n\
         --- field logs ---\n{field_logs}\n--- full logs (same run) ---\n{full_logs}"
    );
    assert!(
        !field_logs.contains("no retries left"),
        "field filter must also hide the 'no retries left' line (same target).\n--- logs ---\n{field_logs}"
    );
}
