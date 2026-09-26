#!/usr/bin/env bash
# Acceptance harness for lnprobe (drop-in proof against lnsd and rnsd).
# Self-contained: starts each daemon, runs the probes, tears everything
# down before exiting. Nothing outlives this script.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/x86_64-unknown-linux-musl/debug"
WORK="${1:-/tmp/lnprobe-accept}"
PYRNS="PYTHONPATH=$ROOT/reference/Reticulum"

rm -rf "$WORK"
mkdir -p "$WORK/lnsd-cfg" "$WORK/rnsd-cfg" "$WORK/out"

DAEMON_PID=""
cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

mkcfg() {
  cat > "$1/config" <<EOF
[reticulum]
  enable_transport = True
  share_instance = Yes
  instance_name = $2
  respond_to_probes = Yes
  panic_on_interface_error = No

[logging]
  loglevel = 4

[interfaces]
EOF
}

mkcfg "$WORK/lnsd-cfg" lnprobe_accept_rust
mkcfg "$WORK/rnsd-cfg" lnprobe_accept_py

wait_socket() { # config_dir daemon_log
  for _ in $(seq 1 50); do
    if grep -q "shared instance\|Started" "$2" 2>/dev/null; then break; fi
    sleep 0.2
  done
  sleep 1
}

echo "=== PHASE 1: lnsd ==="
"$BIN/lnsd" --config "$WORK/lnsd-cfg" -v > "$WORK/out/lnsd.log" 2>&1 &
DAEMON_PID=$!
sleep 3

echo "--- lnstatus against lnsd (probe responder line, part D) ---"
"$BIN/lnstatus" --config "$WORK/lnsd-cfg" > "$WORK/out/lnstatus-lnsd.txt" 2>&1
grep -i "probe" "$WORK/out/lnstatus-lnsd.txt" || echo "NO PROBE LINE IN LNSTATUS"

PROBE_HASH=$(grep -o "probe_destination=[0-9a-f]*" "$WORK/out/lnsd.log" | head -1 | cut -d= -f2)
if [ -z "$PROBE_HASH" ]; then
  PROBE_HASH=$(grep -io "Probe responder at <[0-9a-f]*>" "$WORK/out/lnstatus-lnsd.txt" | grep -o "[0-9a-f]\{32\}" | head -1)
fi
echo "lnsd probe hash: $PROBE_HASH"

echo "--- lnprobe against lnsd ---"
"$BIN/lnprobe" --config "$WORK/lnsd-cfg" rnstransport.probe "$PROBE_HASH" > "$WORK/out/lnprobe-vs-lnsd.txt" 2>&1
echo "exit=$?" >> "$WORK/out/lnprobe-vs-lnsd.txt"
cat "$WORK/out/lnprobe-vs-lnsd.txt"

echo "--- Python rnprobe against lnsd ---"
env PYTHONPATH="$ROOT/reference/Reticulum" python3 -m RNS.Utilities.rnprobe --config "$WORK/lnsd-cfg" rnstransport.probe "$PROBE_HASH" > "$WORK/out/rnprobe-vs-lnsd.txt" 2>&1
echo "exit=$?" >> "$WORK/out/rnprobe-vs-lnsd.txt"
cat "$WORK/out/rnprobe-vs-lnsd.txt"

echo "--- negative: unknown hash, -t 3 ---"
"$BIN/lnprobe" --config "$WORK/lnsd-cfg" -t 3 rnstransport.probe 00112233445566778899aabbccddeeff > "$WORK/out/lnprobe-negative.txt" 2>&1
echo "exit=$?" >> "$WORK/out/lnprobe-negative.txt"
cat "$WORK/out/lnprobe-negative.txt"

echo "--- lnprobe -n 3 against lnsd ---"
"$BIN/lnprobe" --config "$WORK/lnsd-cfg" -n 3 rnstransport.probe "$PROBE_HASH" > "$WORK/out/lnprobe-n3-vs-lnsd.txt" 2>&1
echo "exit=$?" >> "$WORK/out/lnprobe-n3-vs-lnsd.txt"
cat "$WORK/out/lnprobe-n3-vs-lnsd.txt"

kill "$DAEMON_PID" 2>/dev/null
wait "$DAEMON_PID" 2>/dev/null
DAEMON_PID=""

echo
echo "=== PHASE 2: rnsd (Python, reference 1.3.5) ==="
env PYTHONPATH="$ROOT/reference/Reticulum" PYTHONUNBUFFERED=1 python3 -m RNS.Utilities.rnsd --config "$WORK/rnsd-cfg" -v > "$WORK/out/rnsd.log" 2>&1 &
DAEMON_PID=$!
sleep 4

echo "--- lnstatus against rnsd (drop-in status) ---"
"$BIN/lnstatus" --config "$WORK/rnsd-cfg" > "$WORK/out/lnstatus-rnsd.txt" 2>&1
grep -i "probe" "$WORK/out/lnstatus-rnsd.txt" || echo "NO PROBE LINE IN LNSTATUS vs rnsd"

PY_PROBE_HASH=$(grep -io "Probe responder at <[0-9a-f]*>" "$WORK/out/lnstatus-rnsd.txt" | grep -o "[0-9a-f]\{32\}" | head -1)
if [ -z "$PY_PROBE_HASH" ]; then
  PY_PROBE_HASH=$(env PYTHONPATH="$ROOT/reference/Reticulum" python3 - "$WORK/rnsd-cfg" <<'PYEOF'
import sys
import RNS
r = RNS.Reticulum(configdir=sys.argv[1], loglevel=0)
ident = RNS.Identity.from_file(sys.argv[1] + "/storage/transport_identity")
dest = RNS.Destination(ident, RNS.Destination.IN, RNS.Destination.SINGLE, "rnstransport", "probe")
print(RNS.hexrep(dest.hash, delimit=False))
PYEOF
  )
fi
echo "rnsd probe hash: $PY_PROBE_HASH"

echo "--- lnprobe against rnsd (THE drop-in proof) ---"
"$BIN/lnprobe" --config "$WORK/rnsd-cfg" rnstransport.probe "$PY_PROBE_HASH" > "$WORK/out/lnprobe-vs-rnsd.txt" 2>&1
echo "exit=$?" >> "$WORK/out/lnprobe-vs-rnsd.txt"
cat "$WORK/out/lnprobe-vs-rnsd.txt"

echo "--- Python rnprobe against rnsd (reference output) ---"
env PYTHONPATH="$ROOT/reference/Reticulum" python3 -m RNS.Utilities.rnprobe --config "$WORK/rnsd-cfg" rnstransport.probe "$PY_PROBE_HASH" > "$WORK/out/rnprobe-vs-rnsd.txt" 2>&1
echo "exit=$?" >> "$WORK/out/rnprobe-vs-rnsd.txt"
cat "$WORK/out/rnprobe-vs-rnsd.txt"

kill "$DAEMON_PID" 2>/dev/null
wait "$DAEMON_PID" 2>/dev/null
DAEMON_PID=""

echo
echo "=== DONE — outputs under $WORK/out ==="
