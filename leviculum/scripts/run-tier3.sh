#!/bin/bash
LOG_DIR=~/.local/state/leviculum-ci
mkdir -p "$LOG_DIR"
# Per-execution log: timestamp + PID guarantees no overlap if two
# instances ever collide (CLAUDE.md: failure logs must always survive).
LOG="$LOG_DIR/nightly-$(date +%Y%m%d-%H%M%S)-$$.log"
RESULTS="$LOG_DIR/last-results.txt"

cd "$(dirname "$0")/.."

# Rotate logs (keep nightly logs longer: 60 days)
find "$LOG_DIR" -name 'nightly-*.log' -mtime +60 -delete 2>/dev/null || true

MARKER="$LOG_DIR/lock-contention"
if CARGO_TARGET_DIR=~/.cache/leviculum-ci-target CARGO_INCREMENTAL=0 just nightly > "$LOG" 2>&1; then
    # Parse pass/skip counts for hardware-availability awareness.
    # Format may vary across cargo versions; adjust if parsing fails.
    PASSED=$(grep -oP 'test result: ok\. \K\d+' "$LOG" | awk '{s+=$1} END{print s}')
    SKIPPED=$(grep -oP 'test result: ok\..+?\K\d+(?= ignored)' "$LOG" | awk '{s+=$1} END{print s}')
    notify-send -u normal "Leviculum CI" "Nightly: GREEN (passed: ${PASSED:-?}, skipped: ${SKIPPED:-?})"
    echo "$(date -Iseconds) tier3 GREEN passed=${PASSED:-?} skipped=${SKIPPED:-?} $LOG" >> "$RESULTS"
elif [ -f "$MARKER" ]; then
    # Another run held the scenario-runner lock when nightly tried to
    # start — e.g. a manual LoRa bench was running past 02:00.
    # Not a failure; deferred. See periculum/src/lock.rs.
    rm -f "$MARKER"
    notify-send -u normal "Leviculum CI" "Nightly: SKIPPED — another test held the lock"
    echo "$(date -Iseconds) tier3 SKIPPED lock-held $LOG" >> "$RESULTS"
else
    notify-send -u critical "Leviculum CI" "Nightly: RED — see $LOG"
    echo "$(date -Iseconds) tier3 RED $LOG" >> "$RESULTS"
fi
