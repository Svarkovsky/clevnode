#!/bin/bash
# run-soak.sh — TCP-hub endurance soak entrypoint (Codeberg #101).
#
# Boots the real `lnsd` binary as an internet-facing transport hub, drives
# sustained load + connection churn, and asserts 100% delivery, an RSS plateau
# (no per-connection leak), fd bounded-under-churn + released-after-teardown,
# and a clean hub log. The assertions live in
#   leviculum-std/tests/rnsd_interop/loadtest_tcp_hub_tests.rs
# this script just builds the binary the test needs and runs the right variant.
#
# Usage:
#   bash scripts/run-soak.sh            # smoke variant (~15 s + build)
#   bash scripts/run-soak.sh --full     # heavy soak (env-tunable, minutes)
#
# Tuning (defaults are the test's own; override to make the soak heavier):
#   LOADTEST_CONNS LOADTEST_SECS LOADTEST_PKT_MS LOADTEST_CHURN_WORKERS
#   LOADTEST_CHURN_PKTS LOADTEST_MAX_RSS_GROWTH_PCT LOADTEST_MAX_RSS_ABS_MIB
#   LOADTEST_SAMPLE_MS LOADTEST_DRAIN_SECS
# See the module docs in loadtest_tcp_hub_tests.rs for meanings.
#
# Honours the ambient CARGO_TARGET_DIR so the binary lands where the test's
# locate_lnsd() looks (both use the same dir); do NOT hardcode it here.
set -euo pipefail

cd "$(dirname "$0")/.." || exit 1

FULL=0
if [ "${1:-}" = "--full" ]; then
    FULL=1
elif [ -n "${1:-}" ]; then
    echo "usage: $0 [--full]" >&2
    exit 2
fi

if [ "$FULL" = "1" ]; then
    TEST="loadtest_tcp_hub_tests::loadtest_tcp_hub_soak"
    VARIANT="soak (heavy)"
else
    TEST="loadtest_tcp_hub_tests::loadtest_tcp_hub_smoke"
    VARIANT="smoke"
fi

echo "[run-soak] variant: $VARIANT"
echo "[run-soak] effective params:"
echo "  CARGO_TARGET_DIR         = ${CARGO_TARGET_DIR:-<default: ./target>}"
echo "  LOADTEST_CONNS           = ${LOADTEST_CONNS:-<test default>}"
echo "  LOADTEST_SECS            = ${LOADTEST_SECS:-<test default>}"
echo "  LOADTEST_PKT_MS          = ${LOADTEST_PKT_MS:-<test default>}"
echo "  LOADTEST_CHURN_WORKERS   = ${LOADTEST_CHURN_WORKERS:-<test default>}"
echo "  LOADTEST_CHURN_PKTS      = ${LOADTEST_CHURN_PKTS:-<test default>}"
echo "  LOADTEST_MAX_RSS_GROWTH_PCT = ${LOADTEST_MAX_RSS_GROWTH_PCT:-<test default>}"
echo "  LOADTEST_MAX_RSS_ABS_MIB = ${LOADTEST_MAX_RSS_ABS_MIB:-<test default>}"

# 1. Build the lnsd binary (release) so the test's locate_lnsd() finds it. The
#    workspace default target is x86_64-unknown-linux-musl (.cargo/config.toml),
#    so this lands under <target>/<triple>/release/lnsd, which locate_lnsd walks.
echo "[run-soak] building lnsd (release)..."
cargo build -p leviculum-cli --release --bin lnsd

# 2. Run the chosen variant. It is #[ignore]d (spawns the lnsd binary), so
#    --ignored --exact selects exactly it. --nocapture surfaces the PASS block
#    with the "rss plateau:" and "fds:" lines.
#
#    Through run-with-manifest.py (Guarantee B step 1): `--exact` is the
#    by-name form where a typo runs nothing and still exits 0, which is what
#    the manifest exists to make visible. Two variants, two manifests, because
#    a smoke run must not be readable as evidence that the heavy soak ran.
echo "[run-soak] running $TEST ..."
python3 scripts/run-with-manifest.py --gate "soak-$([ "$FULL" = 1 ] && echo full || echo smoke)" -- \
    cargo test -p leviculum-std --test rnsd_interop -- \
    --ignored --exact "$TEST" --nocapture
