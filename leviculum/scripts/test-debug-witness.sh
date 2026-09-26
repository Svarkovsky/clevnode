#!/usr/bin/env bash
# Fixture test for the tier-3 debug-port witness (Codeberg #353).
#
# Two halves, and they are tested by different means because they are
# different kinds of claim:
#
#   1. WHICH PORTS GET A WITNESS is a decision, so it is driven against a
#      fixture device list — two LNodes, their data ports, three boards that
#      are not LNodes, and a device udev knows nothing about — and the
#      assertion is on the invocation that WOULD be run. Nothing is spawned.
#
#   2. THAT A READER SURVIVES LOSING ITS PORT is a behaviour, and asserting it
#      against a stub would be asserting the code the test just ran. So the
#      real reader runs against a real pty that is taken away and given back
#      twice, and the assertions are on what ends up in its witness file.
#
# `lsof` is stubbed to report no holders. The pty fixture necessarily holds the
# slave fd open, so the real lsof would name it as a competing holder and the
# reader would correctly yield — correct behaviour, wrong question for this
# test. The stub is on PATH, not a flag in the reader: the reader's holder
# check is production code and stays unconditional.
#
# Usage: bash scripts/test-debug-witness.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/debug-witness.sh
. "$SCRIPT_DIR/debug-witness.sh"
READER="$SCRIPT_DIR/debug-witness-reader.py"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0

ok() {
    PASS=$((PASS + 1))
    printf 'ok    %s\n' "$1"
}
bad() {
    FAIL=$((FAIL + 1))
    printf 'FAIL  %s\n' "$1"
}
check_eq() {
    if [ "$2" = "$3" ]; then
        ok "$1"
    else
        bad "$1"
        printf '        want %q\n        got  %q\n' "$2" "$3"
    fi
}
check_contains() {
    if [[ "$3" == *"$2"* ]]; then
        ok "$1"
    else
        bad "$1"
        printf '        wanted substring %q\n        in %q\n' "$2" "$3"
    fi
}
check_absent() {
    if [[ "$3" == *"$2"* ]]; then
        bad "$1"
        printf '        unwanted substring %q\n        in %q\n' "$2" "$3"
    else
        ok "$1"
    fi
}

# --- 1. Which ports get a witness -------------------------------------------
#
# The fixture is the rig as it actually stands (project_lora_rig_inventory: two
# LNodes, three RNodes) plus the two cases that have to be excluded by a rule
# rather than by luck: an LNode's own data port, which carries the identical
# vid:pid and differs only in interface number, and a device udev has nothing
# to say about.

T114_DEBUG=/dev/serial/by-id/usb-leviculum_LNode_183004F712B4A7FE-if00
T114_DATA=/dev/serial/by-id/usb-leviculum_LNode_183004F712B4A7FE-if02
RAK_DEBUG=/dev/serial/by-id/usb-leviculum_LNode_D57C9A1104E2B6F3-if00
TBEAM=/dev/serial/by-id/usb-1a86_USB_Single_Serial_54F1-if00
HELTEC=/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_A4-if00
UNKNOWN_LNODE=/dev/serial/by-id/usb-leviculum_Prototype_0000000000000001-if00
NO_UDEV=/dev/ttyACM9

witness_device_nodes() {
    printf '%s\n' "$T114_DEBUG" "$T114_DATA" "$RAK_DEBUG" "$TBEAM" "$HELTEC" \
        "$UNKNOWN_LNODE" "$NO_UDEV"
}

witness_device_props() {
    case "$1" in
        "$T114_DEBUG")
            printf 'ID_VENDOR_ID=1209\nID_MODEL_ID=0001\nID_USB_INTERFACE_NUM=00\nID_SERIAL_SHORT=183004F712B4A7FE\n' ;;
        "$T114_DATA")
            printf 'ID_VENDOR_ID=1209\nID_MODEL_ID=0001\nID_USB_INTERFACE_NUM=02\nID_SERIAL_SHORT=183004F712B4A7FE\n' ;;
        "$RAK_DEBUG")
            printf 'ID_VENDOR_ID=1209\nID_MODEL_ID=0002\nID_USB_INTERFACE_NUM=00\nID_SERIAL_SHORT=D57C9A1104E2B6F3\n' ;;
        "$TBEAM")
            printf 'ID_VENDOR_ID=1a86\nID_MODEL_ID=55d4\nID_USB_INTERFACE_NUM=00\nID_SERIAL_SHORT=54F1\n' ;;
        "$HELTEC")
            printf 'ID_VENDOR_ID=303a\nID_MODEL_ID=1001\nID_USB_INTERFACE_NUM=00\nID_SERIAL_SHORT=A4\n' ;;
        "$UNKNOWN_LNODE")
            printf 'ID_VENDOR_ID=1209\nID_MODEL_ID=0003\nID_USB_INTERFACE_NUM=00\nID_SERIAL_SHORT=0000000000000001\n' ;;
        *) printf '' ;;
    esac
}

DISCOVERED="$(witness_lnode_debug_ports)"

check_eq "exactly the two LNode debug ports are discovered" \
    "1209:0001	183004F712B4A7FE	$T114_DEBUG
1209:0002	D57C9A1104E2B6F3	$RAK_DEBUG" \
    "$DISCOVERED"
check_absent "an LNode's if02 data port never gets a witness" "$T114_DATA" "$DISCOVERED"
check_absent "a T-Beam RNode never gets a witness" "$TBEAM" "$DISCOVERED"
check_absent "a Heltec RNode never gets a witness" "$HELTEC" "$DISCOVERED"
check_absent "an unknown 1209 product id never gets a witness" "$UNKNOWN_LNODE" "$DISCOVERED"
check_absent "a device udev knows nothing about never gets a witness" "$NO_UDEV" "$DISCOVERED"

# The invocation itself, for each discovered board. Asserted as the exact argv
# rather than a rendered string, because that is what gets executed.
WDIR="$WORK/nightly-hw-20260827-041200-4242-witness"
T114_OUT="$(witness_log_path "$WDIR" 1209:0001 183004F712B4A7FE)"
check_eq "the witness file leads with the vid_pid the vanish watchdog reports" \
    "$WDIR/1209_0001-183004F712B4A7FE.log" "$T114_OUT"

mapfile -t ARGV < <(witness_reader_argv "$READER" "$T114_OUT" 1209:0001 183004F712B4A7FE "$T114_DEBUG")
mapfile -t WANT_ARGV <<EOF
python3
$READER
--port
$T114_DEBUG
--out
$T114_OUT
--label
1209:0001/183004F712B4A7FE
EOF
check_eq "the reader invocation is built argument by argument" \
    "$(printf '%s\n' "${WANT_ARGV[@]}")" "$(printf '%s\n' "${ARGV[@]}")"
check_eq "the reader watches the debug port, never the data port" \
    "$T114_DEBUG" "${ARGV[3]}"

# The banner lookup: the watchdog knows a vid:pid and nothing else, so that has
# to be enough to reach the file.
mkdir -p "$WDIR"
: >"$WDIR/1209_0001-183004F712B4A7FE.log"
: >"$WDIR/1209_0002-D57C9A1104E2B6F3.log"
check_eq "a vid:pid alone resolves to that board's witness file" \
    "$WDIR/1209_0001-183004F712B4A7FE.log" "$(witness_files_for "$WDIR" 1209:0001)"
if witness_files_for "$WDIR" 1a86:55d4 >/dev/null; then
    bad "a board with no witness file must not resolve to somebody else's"
else
    ok "a board with no witness file must not resolve to somebody else's"
fi

# --- 2. A reader that loses its port reconnects rather than exiting ----------
#
# This is the behaviour the batch exists for. A one-shot open dies on the
# disconnect and misses precisely the boot that carries the post-mortem, so it
# is tested against a port that is taken away and given back — twice, because
# surviving one disconnect and then dying on the next would pass a single-cycle
# test and fail on the rig.

mkdir -p "$WORK/bin"
cat >"$WORK/bin/lsof" <<'EOF'
#!/bin/sh
# Stubbed for the pty fixture: no competing holders. See the header.
exit 1
EOF
chmod +x "$WORK/bin/lsof"

cat >"$WORK/fixture.py" <<'PY'
"""A serial port that can be taken away and given back.

Commands on stdin, one per line:
  newpty  fresh pty, symlink <link> at its slave  (the board enumerating)
  say X   the board says X
  drop    close the pty and remove the symlink    (the board vanishing)
  rx      append whatever the reader wrote to us to <rxlog>
  quit
"""
import os
import pty
import select
import sys

link, rxlog = sys.argv[1], sys.argv[2]
master = slave = None


def drop():
    global master, slave
    try:
        os.unlink(link)
    except OSError:
        pass
    for fd in (master, slave):
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass
    master = slave = None


while True:
    line = sys.stdin.readline()
    if not line:
        break
    cmd = line.strip()
    if not cmd:
        continue
    if cmd == "newpty":
        drop()
        master, slave = pty.openpty()
        tmp = link + ".tmp"
        try:
            os.unlink(tmp)
        except OSError:
            pass
        os.symlink(os.ttyname(slave), tmp)
        os.rename(tmp, link)
        print("ok newpty", flush=True)
    elif cmd.startswith("say "):
        os.write(master, cmd[4:].encode() + b"\r\n")
        print("ok say", flush=True)
    elif cmd == "drop":
        drop()
        print("ok drop", flush=True)
    elif cmd == "rx":
        data = b""
        while True:
            ready, _, _ = select.select([master], [], [], 0.05)
            if not ready:
                break
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break
            if not chunk:
                break
            data += chunk
        with open(rxlog, "ab") as fh:
            fh.write(data)
        print("ok rx", flush=True)
    elif cmd == "quit":
        drop()
        print("ok quit", flush=True)
        break
PY

PORT="$WORK/port"
WLOG="$WORK/witness.log"
RXLOG="$WORK/rx.log"
: >"$RXLOG"
mkfifo "$WORK/cmd"
python3 "$WORK/fixture.py" "$PORT" "$RXLOG" <"$WORK/cmd" >"$WORK/fixture.out" 2>&1 &
FIXTURE_PID=$!
exec 9>"$WORK/cmd"
send() { echo "$1" >&9; }

# Wait until <file> contains <substring>, or give up after <timeout> tenths.
# Bounded polling rather than a sleep: the assertions stay about content, not
# about how fast this machine happens to be.
await() {
    local file="$1" needle="$2" tries="${3:-100}" i
    for ((i = 0; i < tries; i++)); do
        [ -f "$file" ] && grep -qF -- "$needle" "$file" && return 0
        sleep 0.1
    done
    return 1
}
awaited() {
    if await "$2" "$3" "${4:-100}"; then
        ok "$1"
    else
        bad "$1"
        printf '        never saw %q in %s\n' "$3" "$2"
    fi
}

# Wait for <substring> to appear <n> times. The second attach is not marked by
# any line the first one does not also write, so "it came back a second time"
# has to be counted rather than matched.
awaited_count() {
    local label="$1" file="$2" needle="$3" want="$4" tries="${5:-100}" i seen
    for ((i = 0; i < tries; i++)); do
        seen=$(grep -cF -- "$needle" "$file" 2>/dev/null || true)
        [ "${seen:-0}" -ge "$want" ] && { ok "$label"; return 0; }
        sleep 0.1
    done
    bad "$label"
    printf '        saw %q %s times, wanted %s\n' "$needle" "${seen:-0}" "$want"
}

send newpty
await "$WORK/fixture.out" "ok newpty" 50 || bad "the pty fixture never came up"

PATH="$WORK/bin:$PATH" python3 "$READER" \
    --port "$PORT" --out "$WLOG" --label "1209:0001/FIXTURE" \
    --poll 0.1 --capture 20 --acquire 2 &
READER_PID=$!

awaited "the reader arms itself on a port that is present" "$WLOG" "witness armed"

# Cycle 1: the board is there and talking, and the witness is NOT listening.
# That is the design, not a miss: periculum owns this port during a scenario.
send "say [INFO!] chatter-before-any-reset"
send "say [INFO!] chatter-before-any-reset"
sleep 0.5
check_absent "the witness does not hold the port before a vanish" \
    "chatter-before-any-reset" "$(cat "$WLOG")"

send drop
awaited "the reader notices the port going away" "$WLOG" "# port gone"

send newpty
awaited "the reader notices the port coming back" "$WLOG" "port came back"
awaited "...and attaches to it" "$WLOG" "# attached"
awaited "...and asks the firmware for its post-mortem" "$WLOG" "sent post-mortem query"

send "say [INFO!] [PM_QUERY] begin"
send "say [INFO!] [PANIC_COUNT] total=7"
send "say [INFO!] [HARDFAULT_PMRT] pc=0002a1f4 lr=0002a0c9"
send "say [INFO!] [PM_QUERY] done hardfault=1 panic=0"
awaited "the post-mortem block lands in the witness file" "$WLOG" "[PANIC_COUNT] total=7"
awaited "...including the fault registers" "$WLOG" "[HARDFAULT_PMRT] pc=0002a1f4"

send rx
await "$WORK/fixture.out" "ok rx" 50 || bad "the fixture never drained its master"
check_contains "the reader really wrote the query byte to the board" "p" "$(cat "$RXLOG")"

# Cycle 2: the same board resets again. A reader that survived once and then
# exited would have passed everything above.
send drop
awaited "the reader survives a SECOND disconnect" "$WLOG" "port lost while attached"
send newpty
awaited_count "...and attaches a second time rather than having exited" \
    "$WLOG" "port came back" 2 150
awaited_count "...opening the port again, not reusing a dead handle" \
    "$WLOG" "# attached" 2 150
send "say [INFO!] second-reset-evidence"
awaited "...and captures what the board says after the second reset" \
    "$WLOG" "second-reset-evidence" 150

if kill -0 "$READER_PID" 2>/dev/null; then
    ok "the reader is still running after two disconnects"
else
    bad "the reader is still running after two disconnects"
fi

kill -TERM "$READER_PID" 2>/dev/null
wait "$READER_PID"
READER_RC=$?
check_eq "SIGTERM stops the reader cleanly" "0" "$READER_RC"
check_contains "...and it says so in the file" "witness stopped" "$(cat "$WLOG")"

send quit
exec 9>&-
wait "$FIXTURE_PID" 2>/dev/null

printf '\n%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
