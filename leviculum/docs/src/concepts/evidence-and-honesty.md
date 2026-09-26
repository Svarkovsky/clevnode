# Evidence and Honesty in Testing

A mesh stack fails in ways that are easy to explain away: radios,
timers, schedulers, six layers of asynchrony. The only defence is a
set of rules about what counts as evidence and what counts as
closure. These rules are engineering discipline, not process — they
bind anyone contributing a fix, a test, or a measurement.

## A check you have never seen fail is not a check

A green result is evidence only if the check *could* have been red.
Two defects in this codebase's history make the point:

- **The sx1262 RX-extend guard that could never fire.** The guard
  against truncating an in-flight slow-SF frame tested the
  `PreambleDetected` and `HeaderValid` IRQ flags — but the IRQ
  latch mask passed to `SetDioIrqParams` had disabled exactly those
  bits, so the condition was unsatisfiable for the guard's whole
  life (#144, fixed in 26ce3a0). It survived because the firmware
  crate cross-compiles and had no host test target: nothing had ever
  run the guard at all, let alone watched it fire. The fix moved the
  mask and the extend decision into `leviculum-core/src/sx126x.rs`
  as pure functions precisely so a host test could make them fail.
- **The benchmark step that always passed.** Periculum's
  `execute_benchmark` used to end in an unconditional `Ok(())`: it
  drove probe loops, printed a throughput table, and returned green
  whatever the table said. Six of ten recorded runs carried zero
  packets and reported GREEN. From the outside, a step that measures
  and asserts nothing is indistinguishable from a step that measures
  and passes (`periculum/src/assertions.rs`).

The rule that follows: when you add a check — a guard, an assertion,
a test — make it fail once, on the real failing condition, before
you believe its pass. A test that has only ever been green proves
only that it compiles.

## A diagnostic indicator is trusted only after both states

Before reasoning from any indicator — a log line, a counter, a
status field — verify two things:

1. **It measures the production path.** Check in the code that the
   indicator reads the same lookup, the same state, the same branch
   the production behaviour depends on — not a parallel reimplementation
   that can drift.
2. **You have observed it in both states.** An indicator you have
   only seen in one state might be stuck there. Drive the condition
   both ways and watch it follow.

And a diagnostic must not disturb what it measures. The canonical
in-tree example is the airtime meter that a radio restart zeroes —
taking the reading destroyed the reading (see
[Regulatory Airtime](regulatory-airtime.md#the-measurement-pitfall-reading-the-meter-restarts-it)).

## Symptom pairs are not mechanisms

"X happens and Y happens" is a correlation, not a diagnosis. A named
mechanism has a causal chain that ends at a file and line, with the
failing condition reproduced — you can point at the code and say
"this branch, under this input, produces this observation, and here
is the run where it did." Until then you have a hypothesis, and
hypotheses get tested, not implemented: write the test that would
confirm or refute it, and if it is refuted, move to the next one. Do
not ship a fix for a mechanism that measurement has not shown to be
the actual cause.

## Minimal reproduction before the fix

When a bug's mechanism is not obvious on sight, write a minimal
reproducing test first — one that reproduces exactly the failure
mode and nothing more — and only then the fix. Writing the fix first
lets you stop at "seems to work". "I already know the fix" is not a
reason to skip; that is precisely where a test prevents wishful
thinking. (Trivial fixes — a typo, an obvious null dereference — are
exempt, and an existing test reliably made red by the bug counts as
the minimal test.)

A green minimal test alone is **not closure**. The minimal test
characterises one mechanism in isolation; the real context may
contain more. Close a bug only when both the minimal test and the
full end-to-end scenario are green — this codebase has seen isolated
tests go green while the hardware stayed red.

## Reference-first for compatibility-bound behaviour

When the failing behaviour is something we match to a reference —
Python-RNS for protocol mechanics, the RNode firmware for LoRa
CSMA — measure the reference on the same failing scenario *before*
committing to a fix direction. Otherwise you cannot tell a bug in
our stack from a property of the protocol.

"Same scenario" is strict: every input equal except the stack under
test. Exploit the drop-in compatibility
([Python-RNS Compatibility](python-rns-compatibility.md)) — the
harness points the *same* client code at either daemon, never a
parallel per-stack driver, which would smuggle configuration
differences (cadences, phases, timeouts) into what claims to be a
stack comparison. And before interpreting any A/B result, **count
the event volumes on both sides**: if they differ by more than a few
percent, the comparison is invalid and any downstream timing
analysis is meaningless — fix the test, not the hypothesis.

## No noise framings

"Edge of window", "environmental variance", "probably a flake" are
not diagnoses; they are the absence of one. In a controlled lab
there is no noise floor to hide behind: any benchmark below 100 %
packet delivery is a bug, and a flaky test is a deterministic bug
with a flaky symptom. Re-running until green is forbidden — a
failure is root-caused, fixed at the root, and the fix verified to
address the cause rather than mask it. A pre-existing failure is
still a failure; broken tests are not accumulated as known issues.

The same honesty extends to results reporting: a run that skipped a
gate says so, a measurement taken under non-default conditions names
them (see
[Regulatory Airtime](regulatory-airtime.md#disabling-is-an-operator-act-not-a-test-convenience)),
and a bound that does not cover something says what it dropped.
Silent gaps read as "covered".

## See also

- [Wire Field Semantics](wire-field-semantics.md) — the field-level
  testing rule: pin meanings, recomposed independently.
- [Python-RNS Compatibility](python-rns-compatibility.md) — the
  drop-in property that makes honest A/B comparisons possible.
- [Regulatory Airtime](regulatory-airtime.md) — declared deviations
  and non-disturbing diagnostics, applied to radio law.
