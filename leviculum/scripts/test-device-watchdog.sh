#!/usr/bin/env bash
# Fixture test for the tier-3 device-vanish watchdog (Codeberg #65, #353).
#
# Every case here is a failure INJECTED into the watchdog, not a mock of the
# watchdog's own logic: a fake `lsusb` on PATH that lies in a specific way, and
# a fixture sysfs tree the cross-check reads. The assertion is on what ends up
# in the journal and what the accounting concludes from it. That is deliberate
# — a guardrail nobody has watched fire is not a guardrail, and the first two
# cases are the positive controls for the two defects this file was written
# after:
#
#   1. A FAILING lsusb POLL LATCHED A VANISH. Reproduced against the old
#      watchdog on 2026-08-30: one poll where lsusb exits non-zero was enough
#      to mark a board that never moved as gone for the rest of the run, RED.
#      Cases 1a and 1b inject exactly that — once into the whole-bus poll, once
#      into the per-board one — and assert NO vanish is recorded. Case 2 is
#      their counterweight: a board that really leaves is still caught.
#
#   2. A COMMANDED RESET COUNTED AS A DEVICE FAILURE. periculum reboots every
#      bound board per scenario by design; an LNode reboot is a real USB
#      disconnect. Case 3 gives the accounting one observed vanish and one
#      BOARD_RESET line that ordered it, and asserts `accounted`; case 4 gives
#      it two vanishes against one commanded reset and asserts `unexplained`,
#      so the detector keeps its teeth.
#
# Usage: bash scripts/test-device-watchdog.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Fixture sysfs: one directory per device, idVendor/idProduct as sysfs writes
# them (lowercase hex, no 0x). Args: <dir> <vid:pid>...
make_sysfs() {
    local root="$1"; shift
    local i=0 id
    rm -rf "$root"; mkdir -p "$root"
    for id in "$@"; do
        i=$((i + 1))
        mkdir -p "$root/dev$i"
        printf '%s\n' "${id%%:*}" > "$root/dev$i/idVendor"
        printf '%s\n' "${id##*:}" > "$root/dev$i/idProduct"
    done
}

# Fixture lsusb. FAKE_PRESENT lists the rig vid:pids it reports; FAKE_FAIL_AT,
# when set, is the 1-based call number on which it fails the way a real lsusb
# fails with no access to /dev/bus/usb: a message on stderr and a non-zero exit.
# FAKE_ABSENT_AFTER makes the rig boards leave for good after that call.
#
# A root hub is always in the bare listing, present in every FAKE_ config,
# because a real host always has one: `lsusb` reporting an EMPTY bus means the
# tool is broken, never that the machine has no USB, and that is precisely the
# global sanity check the watchdog makes. A fixture whose whole bus disappears
# when one board does would test the wrong thing — it did, until the positive
# control for the sysfs cross-check pointed it out.
make_lsusb() {
    local bin="$1"
    mkdir -p "$bin"
    cat > "$bin/lsusb" <<'EOF'
#!/usr/bin/env bash
n=$(cat "$FAKE_STATE" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$FAKE_STATE"
if [ -n "${FAKE_FAIL_AT:-}" ] && [ "$n" = "$FAKE_FAIL_AT" ]; then
    echo "unable to initialize libusb: -99" >&2
    exit 1
fi
want=""
if [ "${1:-}" = "-d" ]; then want="$2"; fi
present="$FAKE_PRESENT"
if [ -n "${FAKE_ABSENT_AFTER:-}" ] && [ "$n" -gt "$FAKE_ABSENT_AFTER" ]; then
    present=""
fi
rc=1
if [ -z "$want" ]; then
    echo "Bus 001 Device 001: ID 1d6b:0002 fixture root hub"
    rc=0
fi
for id in $present; do
    if [ -z "$want" ] || [ "$id" = "$want" ]; then
        echo "Bus 003 Device 066: ID $id fixture board"
        rc=0
    fi
done
exit $rc
EOF
    chmod +x "$bin/lsusb"
}

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); printf 'ok    %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL  %s\n' "$1"; }
check_eq() {
    if [ "$2" = "$3" ]; then ok "$1"; else
        bad "$1"; printf '        want %q\n        got  %q\n' "$2" "$3"
    fi
}
check_contains() {
    if [[ "$3" == *"$2"* ]]; then ok "$1"; else
        bad "$1"; printf '        wanted substring %q\n        in %q\n' "$2" "$3"
    fi
}
check_absent() {
    if [[ "$3" != *"$2"* ]]; then ok "$1"; else
        bad "$1"; printf '        unwanted substring %q\n        in %q\n' "$2" "$3"
    fi
}

# Run the watchdog for a few seconds against one fixture, in a child bash so
# the fake PATH and the fixture env stay out of this shell.
# Args: $1 = case dir, $2 = seconds, rest = env assignments
drive_watchdog() {
    local dir="$1" secs="$2"; shift 2
    mkdir -p "$dir"
    make_lsusb "$dir/bin"
    # The body is single-quoted on purpose: $1/$2/$3 are the CHILD shell's
    # positional parameters, passed after the `_`. Expanding them here would
    # bake this shell's values in and defeat the point.
    # shellcheck disable=SC2016
    env PATH="$dir/bin:$PATH" FAKE_STATE="$dir/calls" "$@" \
        bash -c '
            set -uo pipefail
            . "$1/device-watchdog.sh"
            RIG_USB_IDS=( "1209:0001" )
            start_device_watchdog "$2/journal" "$2/stop"
            sleep "$3"
            stop_device_watchdog "$2/stop"
        ' _ "$SCRIPT_DIR" "$dir" "$secs"
}

echo "== Case 1a: a failing whole-bus poll is a FAILED POLL, never a vanish =="
# The board is present throughout and sysfs says so; only the tool breaks. Call
# 4 is a bare `lsusb` (call 1 is the baseline, then two calls per tick), so
# this is the global sanity check's positive control.
C1="$WORK/c1"
mkdir -p "$C1"
make_sysfs "$C1/sysfs" "1209:0001"
drive_watchdog "$C1" 5 \
    FAKE_PRESENT="1209:0001" \
    FAKE_FAIL_AT=4 \
    WATCHDOG_SYSFS_ROOT="$C1/sysfs"
J1="$(cat "$C1/journal")"
check_absent "a board that never moved is not recorded as vanished" "vanish " "$J1"
check_contains "the failed poll is recorded as such" "poll_failed " "$J1"
check_contains "and names the whole bus, not the board" "scope=global" "$J1"
check_contains "and the stop line counts it" "failed_polls=" "$J1"

echo "== Case 1b: a failing PER-BOARD poll is a FAILED POLL too =="
# Call 5 is a `lsusb -d <vid:pid>`. It fails with no output, which is exactly
# what an absent board looks like — `lsusb -d` exits 1 for both. Only the
# independent source can tell them apart, and it must.
C1B="$WORK/c1b"
mkdir -p "$C1B"
make_sysfs "$C1B/sysfs" "1209:0001"
drive_watchdog "$C1B" 5 \
    FAKE_PRESENT="1209:0001" \
    FAKE_FAIL_AT=5 \
    WATCHDOG_SYSFS_ROOT="$C1B/sysfs"
J1B="$(cat "$C1B/journal")"
check_absent "a board sysfs can still see is not recorded as vanished" "vanish " "$J1B"
check_contains "the cross-check is what caught it" "sysfs_disagrees" "$J1B"

echo "== Case 2: a real absence IS recorded (the detector still has teeth) =="
# The board answers the baseline poll and the first few ticks, then leaves for
# good; sysfs agrees it is gone. Nothing is broken here; the board is. Driven
# by a call counter rather than by editing the fixture from another process, so
# the case is deterministic and not a race.
C2="$WORK/c2"
mkdir -p "$C2"
make_sysfs "$C2/sysfs"   # empty tree: the independent source confirms absence
drive_watchdog "$C2" 6 \
    FAKE_PRESENT="1209:0001" \
    FAKE_ABSENT_AFTER=4 \
    WATCHDOG_SYSFS_ROOT="$C2/sysfs"
J2="$(cat "$C2/journal")"
check_contains "a board that really left is recorded as vanished" \
    "vanish " "$J2"
check_contains "and the stop line counts the event" "vanish_events=1209:0001:1" "$J2"

echo "== Case 2b: lsusb says gone, sysfs says present -> failed poll, not a vanish =="
C2C="$WORK/c2c"
mkdir -p "$C2C/bin"
make_sysfs "$C2C/sysfs" "1209:0001"
make_lsusb "$C2C/bin"
# shellcheck disable=SC2016  # child-shell positionals, see drive_watchdog
OUT2C=$(env PATH="$C2C/bin:$PATH" FAKE_STATE="$C2C/calls" FAKE_PRESENT="" \
    WATCHDOG_SYSFS_ROOT="$C2C/sysfs" \
    bash -c '. "$1/device-watchdog.sh"; watchdog_poll_id 1209:0001 1; echo "rc=$?"' _ "$SCRIPT_DIR")
check_contains "the independent source overrules the poll" "sysfs_disagrees" "$OUT2C"
check_contains "and the poll is failed, not believed" "rc=1" "$OUT2C"

echo "== Case 3: a vanish periculum commanded is ACCOUNTED, not RED =="
# One observed disconnect; one BOARD_RESET line that ordered it. This is the
# 2026-08-30 nightly in miniature.
C3="$WORK/c3"; mkdir -p "$C3"
cat > "$C3/journal" <<'EOF'
watchdog_start at=2026-08-30T03:13:00+02:00 baseline=1209:0001:1
vanish at=2026-08-30T03:13:47+02:00 vid_pid=1209:0001 baseline=1 now=0
return at=2026-08-30T03:13:50+02:00 vid_pid=1209:0001 count=1 gone_s=3
EOF
printf '1209:0001\t183004F712B4A7FE\n' > "$C3/boards.tsv"
cat > "$C3/run.log" <<'EOF'
[reset] BOARD_RESET kind=rnode port=/dev/ttyACM3 serial=- result=ready ack_ms=- gone_ms=- back_ms=- ready_ms=1793 radio_off=yes
[reset] BOARD_RESET kind=lnode port=/dev/ttyACM1 serial=183004F712B4A7FE result=ready ack_ms=1 gone_ms=297 back_ms=2765 ready_ms=2829 radio_off=-
EOF
OUT3=$(bash -c '. "$1/device-watchdog.sh"; watchdog_adjudicate "$2/journal" "$2/boards.tsv" "$2/run.log"' _ "$SCRIPT_DIR" "$C3")
check_eq "one observed vanish against one commanded reset is accounted" \
    "1209:0001 observed=1 commanded=1 verdict=accounted" "$OUT3"

echo "== Case 4: one disconnect MORE than we commanded is UNEXPLAINED =="
C4="$WORK/c4"; mkdir -p "$C4"
cat > "$C4/journal" <<'EOF'
vanish at=2026-08-30T03:13:47+02:00 vid_pid=1209:0001 baseline=1 now=0
return at=2026-08-30T03:13:50+02:00 vid_pid=1209:0001 count=1 gone_s=3
vanish at=2026-08-30T03:41:02+02:00 vid_pid=1209:0001 baseline=1 now=0
EOF
cp "$C3/boards.tsv" "$C4/boards.tsv"
cp "$C3/run.log" "$C4/run.log"
OUT4=$(bash -c '. "$1/device-watchdog.sh"; watchdog_adjudicate "$2/journal" "$2/boards.tsv" "$2/run.log"' _ "$SCRIPT_DIR" "$C4")
check_eq "a surplus disconnect is still a vanish" \
    "1209:0001 observed=2 commanded=1 verdict=unexplained" "$OUT4"

echo "== Case 5: a board nothing knows about is unexplained, never accounted =="
# No boards.tsv entry (an RNode, or a run with no witness). Silence must not
# read as permission.
C5="$WORK/c5"; mkdir -p "$C5"
printf 'vanish at=2026-08-30T03:13:47+02:00 vid_pid=1a86:55d4 baseline=2 now=1\n' > "$C5/journal"
: > "$C5/boards.tsv"
: > "$C5/run.log"
OUT5=$(bash -c '. "$1/device-watchdog.sh"; watchdog_adjudicate "$2/journal" "$2/boards.tsv" "$2/run.log"' _ "$SCRIPT_DIR" "$C5")
check_eq "an unmapped board's vanish stays RED" \
    "1a86:55d4 observed=1 commanded=0 verdict=unexplained" "$OUT5"

echo "== Case 6: an RNode reset that never leaves the bus commands nothing =="
# periculum prints gone_ms=- when the board did not re-enumerate. Counting
# those would license a disconnect nobody ordered.
C6="$WORK/c6"; mkdir -p "$C6"
printf '[reset] BOARD_RESET kind=rnode port=/dev/ttyACM3 serial=DEADBEEF result=ready ack_ms=- gone_ms=- back_ms=- ready_ms=1793 radio_off=yes\n' > "$C6/run.log"
OUT6=$(bash -c '. "$1/device-watchdog.sh"; watchdog_commanded_resets "$2/run.log" DEADBEEF' _ "$SCRIPT_DIR" "$C6")
check_eq "gone_ms=- is not a commanded disconnect" "0" "$OUT6"

echo "== Case 7: the cause is read off the witness, never asserted =="
C7="$WORK/c7"; mkdir -p "$C7"
# The real 2026-08-30 T114 evidence, trimmed: a commanded reboot, zero panics.
cat > "$C7/commanded.log" <<'EOF'
2026-08-30T03:13:49+0200 [RESET_REASON] raw=0x00000004 resetpin=0 dog=0 sreq=1 lockup=0
2026-08-30T03:13:49+0200 [INFO!] [PANIC_COUNT] total=0 t=10
2026-08-30T03:13:49+0200 [INFO!] [PERSISTENT_LOG] [INFO!] [RESET] host-requested reboot t=8618857 t=14
EOF
cat > "$C7/panic.log" <<'EOF'
2026-08-30T03:13:49+0200 [RESET_REASON] raw=0x00000004 resetpin=0 dog=0 sreq=1 lockup=0
2026-08-30T03:13:49+0200 [INFO!] [PANIC_COUNT] total=3 t=10
EOF
cat > "$C7/silent.log" <<'EOF'
2026-08-30T03:13:49+0200 # port came back after 2.5s
EOF
OUT7A=$(bash -c '. "$1/device-watchdog.sh"; watchdog_reset_cause "$2/commanded.log"' _ "$SCRIPT_DIR" "$C7")
OUT7B=$(bash -c '. "$1/device-watchdog.sh"; watchdog_reset_cause "$2/panic.log"' _ "$SCRIPT_DIR" "$C7")
OUT7C=$(bash -c '. "$1/device-watchdog.sh"; watchdog_reset_cause "$2/silent.log"' _ "$SCRIPT_DIR" "$C7")
OUT7D=$(bash -c '. "$1/device-watchdog.sh"; watchdog_reset_cause "$2/does-not-exist.log"' _ "$SCRIPT_DIR" "$C7")
check_contains "a host-requested reboot is named as such" "host-requested reboot" "$OUT7A"
check_absent  "and is never called a firmware self-reset" "self-reset" "$OUT7A"
check_contains "a real panic is named as a firmware fault" "firmware panic" "$OUT7B"
check_contains "a witness that saw no boot admits it" "unknown" "$OUT7C"
check_contains "a missing witness admits that too" "no witness file" "$OUT7D"

echo
if (( FAIL == 0 )); then
    echo "test-device-watchdog: ALL PASS ($PASS checks)"
    exit 0
fi
echo "test-device-watchdog: $FAIL FAILED, $PASS passed"
exit 1
