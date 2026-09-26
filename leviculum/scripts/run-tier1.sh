#!/bin/bash
LOG_DIR=~/.local/state/leviculum-ci
mkdir -p "$LOG_DIR"
LOCK="$LOG_DIR/tier1.lock"
DIRTY="$LOG_DIR/tier1.dirty"
RESULTS="$LOG_DIR/last-results.txt"

touch "$DIRTY"

exec 9>"$LOCK"
flock -n 9 || exit 0

cd "$(dirname "$0")/.." || exit 1

# Rotate logs: keep only the last 14 days
find "$LOG_DIR" -name 'tier*.log' -mtime +14 -delete 2>/dev/null || true

while [ -f "$DIRTY" ]; do
    rm -f "$DIRTY"
    # Per-iteration LOG so each run keeps its own file — never overwrite a
    # previous run's log (CLAUDE.md: failure logs must always survive).
    LOG="$LOG_DIR/tier1-$(date +%Y%m%d-%H%M%S)-$$.log"
    if CARGO_TARGET_DIR=~/.cache/leviculum-ci-target CARGO_INCREMENTAL=0 just standard > "$LOG" 2>&1; then
        echo "$(date -Iseconds) tier1 GREEN $LOG" >> "$RESULTS"
    else
        echo "$(date -Iseconds) tier1 RED $LOG" >> "$RESULTS"
        bash "$(dirname "$0")/_emit-auto-bug-bundle.sh" tier1 "$LOG" || true
    fi
done
