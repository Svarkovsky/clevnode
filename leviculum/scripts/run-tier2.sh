#!/bin/bash
# Tier-2 runner (on demand since 2026-06-12; the nightly stays scheduled).
#
# Skip-guards: a run silently no-ops if tier2 was already GREEN today or
# if there are no commits today. For an INTENTIONAL repeat run (e.g.
# re-validating after an environment change), bypass both guards with
# either:
#     bash scripts/run-tier2.sh --force
#     LEVICULUM_TIER2_FORCE=1 systemctl --user start leviculum-ci-tier2.service
# (for the systemd form, set the env var via
#  `systemctl --user set-environment LEVICULUM_TIER2_FORCE=1`, and
#  unset-environment afterwards).
FORCE=0
if [ "${1:-}" = "--force" ] || [ "${LEVICULUM_TIER2_FORCE:-0}" = "1" ]; then
    FORCE=1
fi
LOG_DIR=~/.local/state/leviculum-ci
mkdir -p "$LOG_DIR"
# Per-execution log: timestamp + PID guarantees no overlap if two
# instances ever collide (CLAUDE.md: failure logs must always survive).
LOG="$LOG_DIR/tier2-$(date +%Y%m%d-%H%M%S)-$$.log"
RESULTS="$LOG_DIR/last-results.txt"

cd "$(dirname "$0")/.." || exit 1

# Repo-sync at head of every run when the install was --vm-mode
# (worktree-scoped marker inside .git/).  Brings this worktree to
# origin/master before any test work.  Skipped on developer-machine
# installs where the marker is absent.
if [ -f "$(git rev-parse --git-dir)/leviculum-ci-vm-mode-marker" ]; then
    bash "$(dirname "$0")/_repo-sync.sh"
    echo "$(date -Iseconds) tier2 sync HEAD=$(git rev-parse --short HEAD)" >> "$RESULTS"
fi

# Rotate logs
find "$LOG_DIR" -name 'tier*.log' -mtime +14 -delete 2>/dev/null || true

if [ "$FORCE" = "1" ]; then
    echo "$(date -Iseconds) tier2 FORCED (skip-guards bypassed)" >> "$RESULTS"
else
    # Skip if already ran successfully today
    if grep -q "$(date +%Y-%m-%d).*tier2 GREEN" "$RESULTS" 2>/dev/null; then
        exit 0
    fi

    # Skip if no commits today
    if [ -z "$(git log --since=midnight --oneline 2>/dev/null)" ]; then
        exit 0
    fi
fi

MARKER="$LOG_DIR/lock-contention"
if CARGO_TARGET_DIR=~/.cache/leviculum-ci-target CARGO_INCREMENTAL=0 just extensive > "$LOG" 2>&1; then
    echo "$(date -Iseconds) tier2 GREEN $LOG" >> "$RESULTS"
elif [ -f "$MARKER" ]; then
    # Another run held the scenario-runner lock when Tier 2 tried to
    # start. Not a failure — deferred. See periculum/src/lock.rs.
    # Lock-contention is NOT a RED — no bundle emit.
    rm -f "$MARKER"
    echo "$(date -Iseconds) tier2 SKIPPED lock-held $LOG" >> "$RESULTS"
else
    echo "$(date -Iseconds) tier2 RED $LOG" >> "$RESULTS"
    bash "$(dirname "$0")/_emit-auto-bug-bundle.sh" tier2 "$LOG" || true
fi
