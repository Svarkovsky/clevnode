#!/bin/bash
# tier3-hw-selftest.sh — rig-free selftest for run-tier3-hw.sh verdict logic.
#
# Drives run-tier3-hw.sh through its selftest seams (LEVICULUM_SELFTEST plus a
# stubbed periculum) and the simulated-vanish / simulated-stale-firmware hooks,
# with NO rig, NO build and NO real periculum. Asserts the rig-honesty verdict
# policy that is this script's whole remaining reason to exist:
#
#   - an UNACCOUNTED board vanish -> tier3 RED with the vanished board named
#     (board_vanish=<vid:pid> cause=<what the witness supports>), and NOTHING
#     marked INFRA_INVALID (the class is retired);
#   - a vanish periculum's own BOARD_RESET lines say it COMMANDED -> not RED.
#     periculum reboots every bound board per scenario by design and an LNode
#     reboot is a real USB disconnect; counting our own command as a device
#     failure turned two nightlies red on healthy boards (Codeberg #65);
#   - a board vanish -> the RED banner names the WITNESS FILE for that board
#     (Codeberg #353): a witness nobody can find is not a witness, and the
#     watchdog knows only a vid:pid, so the banner has to do the lookup;
#   - a clean run -> tier3 GREEN, no vanish tokens;
#   - a genuine scenario failure with no vanish -> tier3 RED (periculum's own
#     exit-1 contract, passed through unchanged);
#   - unverified LNode firmware -> tier3 RED naming the board;
#   - a corpus where nothing ran (periculum exit 3) -> tier3 SKIPPED, never
#     GREEN and never RED.
#
# Each case runs with HOME pointed at a throwaway dir so the script's state
# (~/.local/state/leviculum-ci) lands in the sandbox and never pollutes real CI.
#
# Usage: bash scripts/tier3-hw-selftest.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TARGET="$SCRIPT_DIR/run-tier3-hw.sh"

# Stub periculum: $1 is the --json-out path, the rest are scenario targets. It
# writes a schema-1-shaped results document with the chosen summary counters
# and exits with the chosen code, so the verdict block reads real JSON.
stub() {
    local rc="$1" marginal="$2" skipped="$3"
    cat <<EOF
json="\$1"; shift
cat > "\$json" <<JSON
{"schema":1,"summary":{"green":1,"red":0,"marginal":$marginal,"unsupported":0,"skipped_infra":$skipped,"total":1},"exit_code":$rc}
JSON
echo "SUMMARY green=1 red=0 marginal=$marginal unsupported=0 skipped_infra=$skipped total=1"
exit $rc
EOF
}

# Same, plus the `BOARD_RESET` event line periculum prints for every board it
# rebooted into a clean state before a scenario. `gone_ms` is a number exactly
# when the board really left the USB bus, which is what makes the disconnect a
# commanded one; the RNode line beside it (`gone_ms=-`) is there because an
# RNode reset never re-enumerates and must not license anything.
# Args: $1 = exit code, $2 = the LNode's USB serial
stub_with_reset() {
    local rc="$1" serial="$2"
    cat <<EOF
json="\$1"; shift
cat > "\$json" <<JSON
{"schema":1,"summary":{"green":1,"red":0,"marginal":0,"unsupported":0,"skipped_infra":0,"total":1},"exit_code":$rc}
JSON
echo "[reset] BOARD_RESET kind=rnode port=/dev/ttyACM3 serial=- result=ready ack_ms=- gone_ms=- back_ms=- ready_ms=1793 radio_off=yes"
echo "[reset] BOARD_RESET kind=lnode port=/dev/ttyACM1 serial=$serial result=ready ack_ms=1 gone_ms=297 back_ms=2765 ready_ms=2829 radio_off=-"
echo "SUMMARY green=1 red=0 marginal=0 unsupported=0 skipped_infra=0 total=1"
exit $rc
EOF
}

# Exit 2 and exit 3 write no usable document (periculum writes none on 2), so
# those stubs emit output only.
stub_no_json() {
    local rc="$1"
    printf 'echo "stub periculum exit %s"; exit %s\n' "$rc" "$rc"
}

FAILED=0

# run_case -- <env assignments...>
# Captures combined output + exit code of one stubbed run. Echoes nothing;
# leaves $OUT and $RC set for the caller's assertions.
run_case() {
    local sandbox
    sandbox=$(mktemp -d)
    OUT=$(env HOME="$sandbox" \
        LEVICULUM_SELFTEST=1 \
        "$@" \
        bash "$TARGET" 2>&1)
    RC=$?
    rm -rf "$sandbox"
}

assert_contains() {
    local hay="$1" needle="$2" label="$3"
    if grep -qF -- "$needle" <<<"$hay"; then
        echo "  PASS: $label"
    else
        echo "  FAIL: $label (missing: $needle)"
        FAILED=1
    fi
}

assert_absent() {
    local hay="$1" needle="$2" label="$3"
    if grep -qF -- "$needle" <<<"$hay"; then
        echo "  FAIL: $label (unexpected: $needle)"
        FAILED=1
    else
        echo "  PASS: $label"
    fi
}

assert_rc() {
    local got="$1" want="$2" label="$3"
    if [[ "$got" == "$want" ]]; then
        echo "  PASS: $label (rc=$got)"
    else
        echo "  FAIL: $label (rc=$got want=$want)"
        FAILED=1
    fi
}

echo "== Case 1: unaccounted board vanish -> RED with board attribution, no INFRA_INVALID =="
run_case \
    LEVICULUM_SELFTEST_PERICULUM="$(stub 0 0 0)" \
    LEVICULUM_SIMULATE_VANISH=1 \
    LEVICULUM_SIMULATE_VANISH_VIDPID=1209:0001
assert_rc "$RC" 1 "vanish exits non-zero (RED)"
assert_contains "$OUT" "tier3 RED (expected_marginal=0 skipped=0 board_vanish=1209:0001 cause=unknown)" "verdict line names the board and admits the cause is unknown"
assert_contains "$OUT" "BOARD VANISH (RED)" "loud board-vanish banner emitted"
assert_contains "$OUT" "UNTRUSTED" "banner says the post-vanish verdicts are untrusted"
assert_absent  "$OUT" "INFRA_INVALID" "no INFRA_INVALID class anywhere"
assert_absent  "$OUT" "infra_invalid=" "no infra_invalid verdict counter"
# The claim the 2026-08-30 forensics chased for a day. The banner may not make
# it unless a witness supports it, and here no witness does.
assert_absent  "$OUT" "firmware_self_reset_suspected" "no unevidenced firmware-failure claim in the verdict line"
assert_absent  "$OUT" "suspected firmware self-reset" "no unevidenced firmware-failure claim in the banner"

echo "== Case 1b: the RED banner names the vanished board's witness file (#353) =="
# A planted witness file, because selftest mode starts no readers (the rig host
# is also the host `just fast` runs on). What is under test is the lookup the
# banner does from a vid:pid — the only thing the watchdog knows — to a path.
WITNESS_SANDBOX=$(mktemp -d)
: >"$WITNESS_SANDBOX/1209_0001-183004F712B4A7FE.log"
: >"$WITNESS_SANDBOX/1209_0002-D57C9A1104E2B6F3.log"
run_case \
    LEVICULUM_SELFTEST_PERICULUM="$(stub 0 0 0)" \
    LEVICULUM_SIMULATE_VANISH=1 \
    LEVICULUM_SIMULATE_VANISH_VIDPID=1209:0001 \
    LEVICULUM_WITNESS_DIR="$WITNESS_SANDBOX"
assert_contains "$OUT" "1209:0001 -> $WITNESS_SANDBOX/1209_0001-183004F712B4A7FE.log" \
    "the banner names the vanished board's witness file, not the directory"
assert_absent "$OUT" "1209_0002-D57C9A1104E2B6F3.log" \
    "and not some other board's file"

echo "== Case 1c: a vanished board with no witness file says so, and where it looked =="
WITNESS_EMPTY=$(mktemp -d)
run_case \
    LEVICULUM_SELFTEST_PERICULUM="$(stub 0 0 0)" \
    LEVICULUM_SIMULATE_VANISH=1 \
    LEVICULUM_SIMULATE_VANISH_VIDPID=1a86:55d4 \
    LEVICULUM_WITNESS_DIR="$WITNESS_EMPTY"
assert_contains "$OUT" "1a86:55d4 -> no witness file in $WITNESS_EMPTY" \
    "an RNode vanish admits there is no witness rather than implying one"
rm -rf "$WITNESS_SANDBOX" "$WITNESS_EMPTY"

echo "== Case 1d: a disconnect periculum COMMANDED is accounted, not RED (#65) =="
# The 2026-08-30 nightly in miniature. periculum reboots every bound board
# before each scenario; an LNode takes that as a full sys_reset and really
# leaves the USB bus. The watchdog sees a genuine disconnect — and it must not
# read as a device failure, because we ordered it.
WITNESS_CMD=$(mktemp -d)
printf '1209:0001\t183004F712B4A7FE\n' > "$WITNESS_CMD/boards.tsv"
run_case \
    LEVICULUM_SELFTEST_PERICULUM="$(stub_with_reset 0 183004F712B4A7FE)" \
    LEVICULUM_SIMULATE_VANISH=1 \
    LEVICULUM_SIMULATE_VANISH_VIDPID=1209:0001 \
    LEVICULUM_WITNESS_DIR="$WITNESS_CMD"
assert_rc "$RC" 0 "a commanded reset does not fail the tier"
assert_contains "$OUT" "tier3 GREEN (expected_marginal=0 skipped=0)" "verdict is GREEN with no vanish fields"
assert_contains "$OUT" "ACCOUNTING 1209:0001 observed=1 commanded=1 verdict=accounted" "the accounting is shown, not silent"
assert_absent  "$OUT" "BOARD VANISH (RED)" "no vanish banner for a reset we ordered"
assert_absent  "$OUT" "board_vanish=" "no board_vanish token for a reset we ordered"
rm -rf "$WITNESS_CMD"

echo "== Case 1e: one disconnect MORE than commanded is still RED =="
# The detector keeps its teeth: the same commanded reset, but the board left
# twice. The surplus is exactly the Codeberg #65 symptom.
WITNESS_SURPLUS=$(mktemp -d)
printf '1209:0001\t183004F712B4A7FE\n' > "$WITNESS_SURPLUS/boards.tsv"
# A witness that shows a real firmware fault, so the banner has something to
# name and the `cause=` token is read off evidence rather than invented.
cat > "$WITNESS_SURPLUS/1209_0001-183004F712B4A7FE.log" <<'EOF'
2026-08-30T03:13:49+0200 [RESET_REASON] raw=0x00000004 resetpin=0 dog=0 sreq=1 lockup=0
2026-08-30T03:13:49+0200 [INFO!] [PANIC_COUNT] total=2 t=10
EOF
# Two vanishes: the simulated one plus a second planted in the journal the
# watchdog will write to. The journal is truncated at watchdog start, so the
# second is injected the same way the first is.
run_case \
    LEVICULUM_SELFTEST_PERICULUM="$(stub_with_reset 0 183004F712B4A7FE)" \
    LEVICULUM_SIMULATE_VANISH=1 \
    LEVICULUM_SIMULATE_VANISH_VIDPID=1209:0001 \
    LEVICULUM_SIMULATE_VANISH_COUNT=2 \
    LEVICULUM_WITNESS_DIR="$WITNESS_SURPLUS"
assert_rc "$RC" 1 "a surplus disconnect fails the tier"
assert_contains "$OUT" "ACCOUNTING 1209:0001 observed=2 commanded=1 verdict=unexplained" "the surplus is named"
assert_contains "$OUT" "tier3 RED (expected_marginal=0 skipped=0 board_vanish=1209:0001 cause=firmware_panic)" "the cause token comes from the witness"
assert_contains "$OUT" "firmware panic/hardfault" "the banner names what the board actually reported"
rm -rf "$WITNESS_SURPLUS"

echo "== Case 2: clean run -> GREEN, no vanish tokens =="
run_case LEVICULUM_SELFTEST_PERICULUM="$(stub 0 0 0)"
assert_rc "$RC" 0 "clean run exits zero (GREEN)"
assert_contains "$OUT" "tier3 GREEN (expected_marginal=0 skipped=0)" "plain GREEN verdict, no vanish fields"
assert_absent  "$OUT" "board_vanish=" "no board_vanish token on a clean run"
assert_absent  "$OUT" "firmware_unverified=" "no firmware_unverified token on a clean run"

echo "== Case 3: periculum reports RED -> tier3 RED (contract passed through) =="
run_case LEVICULUM_SELFTEST_PERICULUM="$(stub 1 0 0)"
assert_rc "$RC" 1 "scenario failure exits non-zero (RED)"
assert_contains "$OUT" "tier3 RED (expected_marginal=0 skipped=0)" "plain RED verdict, no vanish fields"
assert_absent  "$OUT" "board_vanish=" "no board_vanish token for a plain scenario failure"

echo "== Case 4: governed carve-outs and skips are surfaced, not folded into the verdict =="
run_case LEVICULUM_SELFTEST_PERICULUM="$(stub 0 3 7)"
assert_rc "$RC" 0 "a run whose only non-green verdicts are marginal/skipped is GREEN"
assert_contains "$OUT" "tier3 GREEN (expected_marginal=3 skipped=7)" "counters come from periculum's summary"

echo "== Case 5: stale/unverified LNode firmware -> RED with firmware_unverified attribution =="
run_case \
    LEVICULUM_SELFTEST_PERICULUM="$(stub 0 0 0)" \
    LEVICULUM_SIMULATE_FW_STALE=1209:0001
assert_rc "$RC" 1 "unverified firmware exits non-zero (RED)"
assert_contains "$OUT" "tier3 RED (expected_marginal=0 skipped=0 firmware_unverified=1209:0001)" "verdict line names the unverified board"
assert_contains "$OUT" "FIRMWARE UNVERIFIED (RED)" "loud firmware-unverified banner emitted"
assert_absent  "$OUT" "tier3 GREEN" "unverified firmware is not GREEN"

echo "== Case 6: nothing ran (periculum exit 3) -> SKIPPED, neither GREEN nor RED =="
run_case LEVICULUM_SELFTEST_PERICULUM="$(stub_no_json 3)"
assert_rc "$RC" 0 "nothing-ran does not fail the tier"
assert_contains "$OUT" "tier3 SKIPPED" "verdict line says SKIPPED"
assert_absent  "$OUT" "tier3 GREEN" "nothing-ran must not read as a passing nightly"
assert_absent  "$OUT" "tier3 RED" "nothing-ran is a rig statement, never a protocol one"

echo "== Case 7: periculum exit 2 without a lock marker -> RED (harness error) =="
run_case LEVICULUM_SELFTEST_PERICULUM="$(stub_no_json 2)"
assert_rc "$RC" 1 "harness error exits non-zero (RED)"
assert_contains "$OUT" "periculum exited 2" "the cause is named in the log"

echo
if (( FAILED == 0 )); then
    echo "tier3-hw-selftest: ALL PASS"
    exit 0
else
    echo "tier3-hw-selftest: FAILURES"
    exit 1
fi
