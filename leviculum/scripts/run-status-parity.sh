#!/usr/bin/env bash
# Route the status_parity suite into a tier (Codeberg #191a).
#
# status_parity is the only suite that drives BOTH daemons -- our lnsd and
# the vendor Python rnsd -- through one byte-identical traffic script and
# then compares what the two clients report. That makes it the executable
# form of the drop-in-compatibility goal, and at ~196 s the cheapest
# coverage per minute in the ignored bucket (Codeberg #189, #191).
#
# The #[ignore] stays on the three tests, and they are run by name here,
# for two reasons the ignore reason string does not state:
#
#   * they must run SERIALLY. The scenario paces announces at 25 Hz to
#     trip the ingress burst threshold and then polls both daemons until
#     every frequency has decayed to exactly 0. Sharing a machine with the
#     rest of the parallel rnsd_interop suite puts CPU contention inside
#     those windows. `just standard` runs that suite with default threads;
#     lifting the ignore would drop these three into that pool.
#   * they need the DEBUG lnsd and lnstatus binaries next to the test
#     executable, and refuse a binary older than leviculum-cli's sources
#     (the #53 stale-binary rule). No other tier builds those, so the
#     build belongs with the run.
#
# Python RNS is NOT an extra prerequisite this adds: the non-ignored tests
# in the same binary already spawn python3 against reference/Reticulum and
# have no skip path, so `just standard` cannot pass without it today.

set -euo pipefail
cd "$(dirname "$0")/.."

# The three status_parity_tests::* tests. A cargo filter that matches
# nothing exits 0, so a green run here would otherwise be indistinguishable
# from a run that measured nothing -- the exact failure mode
# docs/src/concepts/evidence-and-honesty.md opens with. Pinned, and checked
# against the summary line below.
#
# scripts/run-with-manifest.py now closes the matches-nothing hole for every
# gate, this one included: it records the names that ran and fails a run that
# executed none. EXPECTED stays because it pins a NUMBER -- the manifest says
# what ran, not how many were meant to.
EXPECTED=3

cargo build -p leviculum-cli --bin lnsd --bin lnstatus

# Kept, not mktemp'd-and-deleted: this gate's whole job is to produce an
# assertion diff when the two stacks disagree, and a red run whose output is
# thrown away is close to unactionable (Codeberg #195). The durable copy is
# scripts/run-with-manifest.py's <gate>.failed.log; this one is the tee target
# for the parsing below and is reported alongside it.
log=$(mktemp -t status-parity-XXXXXX.log)

start=$(date +%s)
set +e
python3 scripts/run-with-manifest.py --gate status-parity -- \
    cargo test -p leviculum-std --test rnsd_interop status_parity_tests:: \
    -- --ignored --test-threads=1 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
set -e
elapsed=$(( $(date +%s) - start ))

if [ "$rc" -ne 0 ]; then
    echo "[status-parity] FAILED after ${elapsed}s (exit $rc)" >&2
    grep -E '^(test |failures:|    status_parity)' "$log" >&2 || true
    echo "[status-parity] full output: $log" >&2
    exit "$rc"
fi

ran=$(sed -n 's/^test result: ok\. \([0-9]*\) passed.*/\1/p' "$log" | tail -1)
if [ "${ran:-0}" -ne "$EXPECTED" ]; then
    echo "[status-parity] expected $EXPECTED tests to run, ${ran:-0} did." >&2
    echo "[status-parity] The suite passed, but it did not measure what this" >&2
    echo "[status-parity] gate claims to measure. If a status_parity test was" >&2
    echo "[status-parity] added, removed or renamed, update EXPECTED in" >&2
    echo "[status-parity] scripts/run-status-parity.sh in the same commit -- and" >&2
    echo "[status-parity] if the ignored count changed, scripts/ignored-counts.txt" >&2
    echo "[status-parity] will tell you so too." >&2
    echo "[status-parity] full output: $log" >&2
    exit 1
fi

rm -f "$log"
echo "[status-parity] OK: $ran/$EXPECTED tests, ${elapsed}s"
