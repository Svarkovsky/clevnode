#!/usr/bin/env bash
# Bench reproduction for Codeberg #373: a relayed two-fragment packet on
# the relay's BLE hop, proof-counted end to end.
#
# Topology (the reviewer wires and flashes; this script runs the host side):
#
#   [source lnsd, RNodeInterface on a t-beam]
#        --LoRa-->  [T114 relay, firmware with BLE_TX_PKT]
#        --BLE-->   [receiver lnsd, BLEInterface — the "phone" stand-in]
#
# The receiver answers probes (respond_to_probes), the source sends COUNT
# probes sized so the relay's forwarded packet needs TWO BLE fragments,
# and the proof count is the measure. The desk case was 3/5; the lab bar
# is COUNT/COUNT (any lab loss is a bug, #24).
#
# Attribution comes from the relay's debug port, captured separately
# (lnflash/serial capture per the rig runbook). For every probe expect
# one line:
#   BLE_TX_PKT conn=<h> len=<n> frags=2 sent=2     - handed over whole
# A failure with sent<frags carries a BLE_TX_DROP naming the reason; a
# BLE_TX_PKT sent=2 with no proof moves the loss past the relay - then
# check the receiver's own log (this script greps it) for BLE_RX_ABANDON.
# frags=1 in the capture means PROBE_SIZE is too small to exercise the
# bug: raise it until the relay logs frags=2 (a BLE packet fragments
# above 177 payload bytes at the fragmenter's DEFAULT_MTU of 185). The
# desk failure was a 291-byte LoRa packet; tune PROBE_SIZE until the
# relay's "[LORA] RX <n> bytes" line says 291 to match it exactly.
#
# Environment:
#   RNODE_PORT   serial port of the t-beam (required), e.g. /dev/serial/by-id/...
#   FREQ/BW/SF/CR/TXPOWER  radio parameters - MUST match the relay's
#                flashed radio settings (defaults are the rig's bench set)
#   COUNT        probes to send (default 20)
#   PROBE_SIZE   probe payload bytes (default 220; see tuning note above)
#   WORK         scratch dir (default /tmp/ble-relay-frag-bench)
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/x86_64-unknown-linux-musl/debug"
WORK="${WORK:-/tmp/ble-relay-frag-bench}"
RNODE_PORT="${RNODE_PORT:?set RNODE_PORT to the t-beam serial port}"
FREQ="${FREQ:-867200000}"
BW="${BW:-125000}"
SF="${SF:-8}"
CR="${CR:-5}"
TXPOWER="${TXPOWER:-7}"
COUNT="${COUNT:-20}"
PROBE_SIZE="${PROBE_SIZE:-220}"
PROBE_TIMEOUT="${PROBE_TIMEOUT:-30}"

rm -rf "$WORK"
mkdir -p "$WORK/src-cfg" "$WORK/rcv-cfg" "$WORK/out"

PIDS=""
cleanup() {
  for pid in $PIDS; do kill "$pid" 2>/dev/null; done
  wait 2>/dev/null
}
trap cleanup EXIT

# Receiver: the "phone" - BLE only, probe responder. Same shape as the
# ~/ble-accept-rns acceptance config, plus respond_to_probes.
cat > "$WORK/rcv-cfg/config" <<EOF
[reticulum]
  enable_transport = True
  share_instance = Yes
  instance_name = frag_bench_rcv
  respond_to_probes = Yes
  panic_on_interface_error = No

[logging]
  loglevel = 6

[interfaces]
  [[BLE Interface]]
    type = BLEInterface
    enabled = yes
EOF

# Source: LoRa only, via the t-beam RNode. The radio parameters must
# match the relay's flashed settings or the two never hear each other.
cat > "$WORK/src-cfg/config" <<EOF
[reticulum]
  enable_transport = True
  share_instance = Yes
  instance_name = frag_bench_src
  panic_on_interface_error = No

[logging]
  loglevel = 6

[interfaces]
  [[LoRa Source]]
    type = RNodeInterface
    enabled = yes
    port = $RNODE_PORT
    frequency = $FREQ
    bandwidth = $BW
    spreadingfactor = $SF
    codingrate = $CR
    txpower = $TXPOWER
EOF

echo "=== receiver lnsd (BLE, probe responder) ==="
"$BIN/lnsd" --config "$WORK/rcv-cfg" -v > "$WORK/out/rcv.log" 2>&1 &
PIDS="$PIDS $!"
sleep 3

# The probe destination hash, from the drop-in status surface (the
# "Probe responder at <...> active" line lnstatus and rnstatus share).
"$BIN/lnstatus" --config "$WORK/rcv-cfg" > "$WORK/out/lnstatus-rcv.txt" 2>&1
PROBE_HASH=$(grep -io "Probe responder at <[0-9a-f]*>" "$WORK/out/lnstatus-rcv.txt" | grep -o "[0-9a-f]\{32\}" | head -1)
if [ -z "$PROBE_HASH" ]; then
  echo "FATAL: lnstatus shows no probe responder; is respond_to_probes wired?"
  exit 1
fi
echo "receiver probe hash: $PROBE_HASH"
echo "waiting for the receiver's BLE link to the relay..."
for _ in $(seq 1 60); do
  grep -q "BLE_LINK_UP" "$WORK/out/rcv.log" && break
  sleep 2
done
grep -m1 "BLE_LINK_UP" "$WORK/out/rcv.log" || {
  echo "FATAL: no BLE_LINK_UP on the receiver within 120 s - is the relay up and in range?"
  exit 1
}

echo "=== source lnsd (t-beam RNode) ==="
"$BIN/lnsd" --config "$WORK/src-cfg" -v > "$WORK/out/src.log" 2>&1 &
PIDS="$PIDS $!"
sleep 5

echo "=== $COUNT probes of $PROBE_SIZE payload bytes ==="
"$BIN/lnprobe" --config "$WORK/src-cfg" -n "$COUNT" -s "$PROBE_SIZE" -t "$PROBE_TIMEOUT" \
  rnstransport.probe "$PROBE_HASH" > "$WORK/out/probes.txt" 2>&1
echo "lnprobe exit=$?" >> "$WORK/out/probes.txt"
cat "$WORK/out/probes.txt"

PROVED=$(grep -c "Valid reply" "$WORK/out/probes.txt")
echo
echo "DELIVERY test=ble_relay_frag sent=$COUNT recv=$PROVED pct=$((PROVED * 100 / COUNT))"
echo
echo "receiver-side reassembly losses (expect none):"
grep "BLE_RX_ABANDON" "$WORK/out/rcv.log" || echo "  (no BLE_RX_ABANDON lines)"
echo
echo "Now cross-check the relay's debug capture:"
echo "  grep -E 'BLE_TX_PKT|BLE_TX_DROP|BLE_RX_ABANDON' <capture>"
echo "  - every probe should show one BLE_TX_PKT frags=2 sent=2"
echo "  - frags=1 means PROBE_SIZE is too small for the two-fragment case"
echo "  - sent<frags without a BLE_TX_DROP is a contradiction: report it"
echo
echo "=== DONE - outputs under $WORK/out ==="
