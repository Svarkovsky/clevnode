#!/usr/bin/env bash
# Catch an LNode reboot under sustained LoRa load and report its cause.
#
# Drives the SF10 airtime-max repro while continuously capturing the board debug
# port (USB if00, reopen-on-EOF so it survives the reboot). After the first
# reboot it reports [RESET_SITE] (which sys_reset fired: touch/panic/hardfault, or
# none(external)=SoftDevice/dependency), [RESETREAS], [PANIC_COUNT], [SD_FAULT]
# from the boot banner, plus the pre-reboot log context.
#
# Reads the marker from USB (the boot banner), NOT RTT: probe-rs `attach` halts the
# core, which perturbs the SoftDevice; USB if00 capture is non-invasive.
#
# Usage: catch-reboot.sh [board=rak4631]
#   env: RUNS (16), LORA_SF (10), LORA_CR (8), LORA_BANDWIDTH (125000)
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BOARD="${1:-rak4631}"
RUNS="${RUNS:-16}"
CAP="${CAP:-$HOME/catch-reboot-cap.txt}"
OUT="${OUT:-$HOME/catch-reboot.log}"
case "$BOARD" in rak4631) IDPAT="RAK" ;; t114) IDPAT="T114" ;; *) echo "unknown board $BOARD" >&2; exit 2 ;; esac
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/leviculum-ci-target}"
export CARGO_TERM_COLOR=never
export LORA_SF="${LORA_SF:-10}" LORA_CR="${LORA_CR:-8}" LORA_BANDWIDTH="${LORA_BANDWIDTH:-125000}"
# The repro scenario lives in the sibling periculum checkout.
PERICULUM_ROOT="${PERICULUM_ROOT:-$REPO/../periculum}"
PERICULUM_BIN="${PERICULUM_BIN:-$PERICULUM_ROOT/target/release/periculum}"
if [ ! -x "$PERICULUM_BIN" ]; then
  echo "periculum binary not found at $PERICULUM_BIN (build it, or set PERICULUM_BIN)" >&2
  exit 2
fi

echo "=== catch-reboot board=$BOARD SF=$LORA_SF CR=$LORA_CR T0=$(date +%H:%M:%S) ===" > "$OUT"
: > "$CAP"
T0=$(date +%s)

# Continuous debug-port (if00) capture, reopen-on-EOF, timestamped. Survives reboot.
( while true; do
    P=$(ls /dev/serial/by-id/*${IDPAT}*if00 2>/dev/null | head -1)
    if [ -n "$P" ]; then
      stty -F "$P" 115200 raw -echo 2>/dev/null
      ts=$(date +%s)
      timeout 1200 cat "$P" 2>/dev/null | sed "s/^/[+$(($ts-T0))s] /;s/\x1b\[[0-9;]*m//g"
    fi
    sleep 0.1
  done >> "$CAP" ) &
CAP_PID=$!
sleep 4
# Baseline: the initial boot banner already in the ring (e.g. from the flash/reset
# before this run). Only NEW "booting" lines DURING the stress count as a reboot.
BASE_BOOTS=$(grep -ac "booting" "$CAP" 2>/dev/null)
echo "baseline boots=$BASE_BOOTS (initial boot, ignored)" >> "$OUT"

cd "$REPO"
for n in $(seq 1 "$RUNS"); do
  docker container prune -f >/dev/null 2>&1; docker network prune -f >/dev/null 2>&1
  # rig.lock is the current name; test.lock lingers only for pinned
  # snapshot binaries that predate the rename. Removal is cosmetic
  # either way — flock state is kernel-held, not file-held.
  rm -f "$HOME/.local/state/leviculum-ci/rig.lock" \
        "$HOME/.local/state/leviculum-ci/test.lock"
  ts=$(date +%s)
  timeout 200 "$PERICULUM_BIN" run "$PERICULUM_ROOT/hardware/lora_lnode_lncp_bidir.toml" \
    >/dev/null 2>&1
  rc=$?
  boots=$(grep -ac "booting" "$CAP" 2>/dev/null)
  newb=$((boots - BASE_BOOTS))
  echo "run $n rc=$rc dur=$(($(date +%s)-ts))s +$(($(date +%s)-T0))s boots=$boots new_reboots=$newb" >> "$OUT"
  if [ "$newb" -gt 0 ]; then echo ">>> REBOOT CAUGHT after run $n (+$(($(date +%s)-T0))s, $newb new boot)" >> "$OUT"; break; fi
done

sleep 2; kill "$CAP_PID" 2>/dev/null
echo "=== DONE +$(($(date +%s)-T0))s ===" >> "$OUT"
echo "--- boot-banner cause lines ---" >> "$OUT"
grep -aE "RESET_SITE|RESETREAS|PANIC_COUNT\] total|SD_FAULT\] count" "$CAP" >> "$OUT" 2>/dev/null
fb=$(grep -an "booting" "$CAP" 2>/dev/null | head -1 | cut -d: -f1)
if [ -n "$fb" ]; then
  echo "--- 25 lines before the first reboot (context) ---" >> "$OUT"
  sed -n "$((fb>25?fb-25:1)),$((fb-1))p" "$CAP" | grep -avE 'FW_BUILD' >> "$OUT" 2>/dev/null
fi
echo "capture: $CAP   summary: $OUT"
