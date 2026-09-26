# shellcheck shell=bash
# device-watchdog.sh — the tier-3 USB device-vanish watchdog.
#
# `run-tier3-hw.sh` sources this and runs the watchdog for the length of a
# tier-3 run. Split out of that script (as debug-witness.sh was, and for the
# same reason) so scripts/test-device-watchdog.sh can drive every decision
# against a fixture `lsusb` and a fixture sysfs tree instead of against the
# rig. Defines functions and constants only; sourcing it starts nothing.
#
# WHAT A VANISH MEANS, AND WHAT IT DOES NOT
#
# The rig boards are passed through to this VM via VFIO controller
# passthrough, so the host cannot inject a phantom VM-side disconnect: a board
# that really leaves the bus really left it. That much of the old design holds.
#
# What the old design got wrong was the step after it — treating "the board
# left the bus" as "the board failed". Two other causes were never enumerated,
# and BOTH have now been demonstrated on this rig:
#
#   1. THE POLL LIED. `lsusb -d <vid:pid> 2>/dev/null | wc -l` cannot fail
#      visibly: a libusb that cannot open /dev/bus/usb prints to the discarded
#      stderr and exits 1, and an empty listing is indistinguishable from an
#      absent device. `lsusb -d` exits 1 for "no such device" too, so the exit
#      code alone does not separate the two. One such poll latched a board as
#      vanished forever (repro: scripts/test-device-watchdog.sh, case "a
#      failing lsusb poll").
#
#   2. WE COMMANDED THE RESET. periculum reboots every board a scenario binds,
#      by design, before the daemons start (periculum/src/runner.rs,
#      `reset_bound_boards`): a clean duty-cycle histogram, radio at firmware
#      defaults, empty queues. An LNode takes that reboot as a full
#      `SCB::sys_reset()` (leviculum-nrf/src/usb.rs, `[RESET] host-requested
#      reboot`), so it leaves the USB bus for ~0.3 s and is back 1.7-3.2 s
#      later. That is a real disconnect with an entirely innocent cause, and
#      the 1-second poll cannot miss it. It is what turned the 2026-08-27 and
#      2026-08-30 nightlies RED (Codeberg 65).
#
# So this file makes three separate claims where the old one made one:
#
#   present-or-absent   — decided by lsusb, cross-checked against sysfs, and
#                         never decided by a poll that failed;
#   accounted-or-not    — an observed vanish is matched against the resets
#                         periculum says it commanded (its own BOARD_RESET
#                         event lines);
#   why                 — read off the board's witness file, or admitted to be
#                         unknown. Never asserted.
#
# Only an UNACCOUNTED vanish is a rig-honesty failure. A vanish we ordered is
# not absorbed infrastructure noise — it is our own command, and counting it as
# a device failure is the bug, not the honesty.

# The four distinct USB IDs of the five rig boards. The two T-Beams share
# 1a86:55d4, so that ID's baseline count is 2 and a single T-Beam vanish drops
# it to 1. Every board, including ones the active scenario silenced, counts: a
# silenced board that vanishes and returns un-silenced can still interfere with
# the running scenario.
RIG_USB_IDS=( "1a86:55d4" "1209:0001" "1209:0002" "303a:1001" )

# Where the sysfs cross-check looks. A variable so the fixture test can point
# it at a directory it built.
WATCHDOG_SYSFS_ROOT="${WATCHDOG_SYSFS_ROOT:-/sys/bus/usb/devices}"

# --- The three seams that touch the machine ---

# Devices matching one vid:pid, one per line. stdout only; the exit code is
# deliberately NOT propagated, because real lsusb exits 1 both for "no such
# device" and for "could not talk to libusb" and the caller must not confuse
# them. Trustworthiness is decided by watchdog_lsusb_all + the sysfs
# cross-check, not by this exit code.
watchdog_lsusb_id() { lsusb -d "$1" 2>/dev/null; }

# The whole bus, one device per line. THIS exit code is meaningful: a
# non-zero exit or an empty listing means lsusb itself is not working, and no
# per-id answer taken in the same tick can be trusted.
watchdog_lsusb_all() { lsusb 2>/dev/null; }

# Is the independent source available at all?
watchdog_sysfs_available() { [ -d "$WATCHDOG_SYSFS_ROOT" ]; }

# How many devices with this vid:pid sysfs knows about. Independent of libusb:
# it is the kernel's own device list, needs no /dev/bus/usb access and no
# privilege, so it survives exactly the failures that make lsusb lie.
watchdog_sysfs_count() {
    local id="$1" d n=0
    local vid="${id%%:*}" pid="${id##*:}"
    for d in "$WATCHDOG_SYSFS_ROOT"/*/; do
        if [ ! -r "$d/idVendor" ] || [ ! -r "$d/idProduct" ]; then continue; fi
        [ "$(cat "$d/idVendor" 2>/dev/null)" = "$vid" ] || continue
        [ "$(cat "$d/idProduct" 2>/dev/null)" = "$pid" ] || continue
        n=$((n + 1))
    done
    printf '%s\n' "$n"
}

# --- One poll of one id ---
#
# Prints the observed count and returns:
#   0  trustworthy: at or above baseline, or below it and confirmed
#   1  FAILED POLL: prints a reason instead of a count, decide nothing
#   2  below baseline but unconfirmed (no independent source to ask)
#
# Args: $1 = vid:pid, $2 = baseline
watchdog_poll_id() {
    local id="$1" base="$2" out n s
    out="$(watchdog_lsusb_id "$id")"
    n=$(printf '%s' "$out" | awk 'END{print NR}')
    if (( n >= base )); then
        printf '%s\n' "$n"
        return 0
    fi
    # Below baseline. One sample decides nothing: the poll that produces this
    # number is the poll that has already been shown to fail silently. Ask an
    # independent source before believing a board left the bus.
    if watchdog_sysfs_available; then
        s="$(watchdog_sysfs_count "$id")"
        if (( s >= base )); then
            printf 'sysfs_disagrees lsusb=%s sysfs=%s\n' "$n" "$s"
            return 1
        fi
        printf '%s\n' "$n"
        return 0
    fi
    # No sysfs to ask. Do not go blind and do not latch from one sample: the
    # caller requires a second consecutive sub-baseline poll instead.
    printf '%s\n' "$n"
    return 2
}

# --- The watchdog process ---

WATCHDOG_PID=""

# Start the watchdog for the run's execution window.
#
# It snapshots a per-vid:pid baseline, then polls once a second and appends one
# line per EVENT to $journal — every vanish and every return, not one latched
# line per board. The count matters: an accounted run has exactly as many
# vanishes as periculum commanded resets, and a board that vanishes once more
# than we told it to is the failure this watchdog exists for.
#
# Args: $1 = journal path, $2 = stop-sentinel path
start_device_watchdog() {
    local journal="$1" stop="$2"
    rm -f "$stop"
    : > "$journal"
    (
        set +e
        declare -A base gone
        local id baseline_note="" polls=0 failed=0 global_failed=0
        local global_streak=0
        declare -A subbaseline vanishes
        for id in "${RIG_USB_IDS[@]}"; do
            base[$id]=$(watchdog_lsusb_id "$id" | awk 'END{print NR}')
            gone[$id]=""
            subbaseline[$id]=0
            vanishes[$id]=0
            baseline_note="$baseline_note${baseline_note:+,}$id:${base[$id]}"
        done
        echo "watchdog_start at=$(date -Iseconds) baseline=$baseline_note" >> "$journal"
        while [[ ! -e "$stop" ]]; do
            polls=$((polls + 1))
            # Is lsusb working at all this tick? If not, every per-id answer
            # from the same tick is worthless — skip the tick rather than read
            # a board out of a broken tool.
            local all rc_all n_all
            all="$(watchdog_lsusb_all)"
            rc_all=$?
            n_all=$(printf '%s' "$all" | awk 'END{print NR}')
            if (( rc_all != 0 )) || (( n_all == 0 )); then
                failed=$((failed + 1))
                global_failed=$((global_failed + 1))
                if (( global_streak == 0 )); then
                    echo "poll_failed at=$(date -Iseconds) scope=global reason=lsusb_unusable rc=$rc_all devices=$n_all" >> "$journal"
                fi
                global_streak=$((global_streak + 1))
                sleep 1
                continue
            fi
            global_streak=0
            for id in "${RIG_USB_IDS[@]}"; do
                local cur rc
                cur="$(watchdog_poll_id "$id" "${base[$id]}")"
                rc=$?
                if (( rc == 1 )); then
                    failed=$((failed + 1))
                    subbaseline[$id]=0
                    echo "poll_failed at=$(date -Iseconds) vid_pid=$id reason=$cur" >> "$journal"
                    continue
                fi
                if (( rc == 2 )); then
                    # Unconfirmed sub-baseline: believe it only if the next
                    # poll says the same. One second later is still well
                    # inside the shortest real absence measured on this rig
                    # (~1.5 s), so this costs no sensitivity.
                    subbaseline[$id]=$(( subbaseline[$id] + 1 ))
                    if (( subbaseline[$id] < 2 )); then
                        continue
                    fi
                fi
                if (( cur < ${base[$id]} )); then
                    subbaseline[$id]=0
                    if [[ -z "${gone[$id]}" ]]; then
                        gone[$id]=$(date +%s)
                        vanishes[$id]=$(( vanishes[$id] + 1 ))
                        echo "vanish at=$(date -Iseconds) vid_pid=$id baseline=${base[$id]} now=$cur" >> "$journal"
                    fi
                else
                    subbaseline[$id]=0
                    if [[ -n "${gone[$id]}" ]]; then
                        echo "return at=$(date -Iseconds) vid_pid=$id count=$cur gone_s=$(( $(date +%s) - gone[$id] ))" >> "$journal"
                        gone[$id]=""
                    fi
                fi
            done
            sleep 1
        done
        local tally=""
        for id in "${RIG_USB_IDS[@]}"; do
            tally="$tally${tally:+,}$id:${vanishes[$id]}"
        done
        echo "watchdog_stop at=$(date -Iseconds) polls=$polls failed_polls=$failed global_failures=$global_failed vanish_events=$tally" >> "$journal"
        exit 0
    ) &
    WATCHDOG_PID=$!
}

# Stop the watchdog (create the stop sentinel, reap the process).
stop_device_watchdog() {
    local stop="$1"
    : > "$stop"
    if [[ -n "$WATCHDOG_PID" ]]; then
        wait "$WATCHDOG_PID" 2>/dev/null || true
    fi
    WATCHDOG_PID=""
    rm -f "$stop"
}

# --- Accounting a vanish against the resets we ordered ---

# How many USB disconnects periculum says it commanded for one board serial.
#
# periculum prints one `BOARD_RESET kind=... serial=<S> ... gone_ms=<N> ...`
# line per board per scenario, and `gone_ms` is a number exactly when the board
# really left the bus — an RNode reset never re-enumerates and prints
# `gone_ms=-`, which is why the RNode ids have never latched while the LNode
# ids latch every run. Counting the numeric ones therefore counts precisely the
# disconnects we ordered.
# Args: $1 = run log, $2 = usb serial
watchdog_commanded_resets() {
    local log="$1" serial="$2" n
    if [ ! -r "$log" ] || [ -z "$serial" ]; then printf '0\n'; return; fi
    # grep -c exits 1 on a zero count and still prints the 0, so the fallback
    # is about the unreadable-file case only.
    n=$(grep -cE "BOARD_RESET .*serial=$serial .*gone_ms=[0-9]" "$log" 2>/dev/null) || true
    printf '%s\n' "${n:-0}"
}

# Observed vanish events for one vid:pid in the journal.
# Args: $1 = journal, $2 = vid:pid
watchdog_vanish_events() {
    local journal="$1" id="$2" n
    [ -r "$journal" ] || { printf '0\n'; return; }
    n=$(grep -cE "^vanish .*vid_pid=$id( |\$)" "$journal" 2>/dev/null) || true
    printf '%s\n' "${n:-0}"
}

# Adjudicate every board that vanished: was each disconnect one we ordered?
#
# Prints one line per vanished id:
#   <vid:pid> observed=<n> commanded=<n> verdict=accounted|unexplained
#
# `boards.tsv` (written by witness_start) maps vid:pid to USB serial, which is
# the only thing that joins the watchdog's view (vid:pid, no serial) to
# periculum's (serial, no vid:pid). A board with no mapping — an RNode, or any
# run with no witness — has commanded=0, so its vanish stays unexplained and
# still reads RED. Silence about a board is never taken as permission.
#
# Args: $1 = journal, $2 = boards.tsv, $3 = run log
watchdog_adjudicate() {
    local journal="$1" boards="$2" log="$3"
    local id observed commanded bid serial verdict
    while IFS= read -r id; do
        [ -n "$id" ] || continue
        observed="$(watchdog_vanish_events "$journal" "$id")"
        commanded=0
        if [ -r "$boards" ]; then
            while IFS=$'\t' read -r bid serial; do
                [ "$bid" = "$id" ] || continue
                commanded=$(( commanded + $(watchdog_commanded_resets "$log" "$serial") ))
            done < "$boards"
        fi
        if (( observed > 0 )) && (( observed <= commanded )); then
            verdict=accounted
        else
            verdict=unexplained
        fi
        printf '%s observed=%s commanded=%s verdict=%s\n' "$id" "$observed" "$commanded" "$verdict"
    done < <(grep -oE '^vanish .*vid_pid=[0-9a-fA-F]{4}:[0-9a-fA-F]{4}' "$journal" 2>/dev/null \
             | grep -oE '[0-9a-fA-F]{4}:[0-9a-fA-F]{4}$' | sort -u)
}

# --- What the board says about why it left ---

# The reset cause for one board, read off its witness file. Prints one short
# phrase. Says `unknown` when the file is missing, empty, or says nothing about
# a reset — the banner must be able to admit ignorance, because the claim it
# used to make unconditionally ("firmware self-reset suspected") was false on
# every run it was printed on.
# Args: $1 = witness file
watchdog_reset_cause() {
    local wf="$1"
    if [ ! -r "$wf" ] || [ ! -s "$wf" ]; then printf 'unknown (no witness file)\n'; return; fi
    if grep -q '\[RESET\] host-requested reboot' "$wf"; then
        printf 'host-requested reboot (the harness commanded it)\n'
        return
    fi
    if grep -qE '\[HARDFAULT_PMRT\]|\[PANIC_PMRT\]' "$wf" \
       || grep -qE '\[PANIC_COUNT\] total=[1-9]' "$wf"; then
        printf 'firmware panic/hardfault (see [PANIC_COUNT]/[*_PMRT] in the witness)\n'
        return
    fi
    if grep -q 'RESET_REASON.* dog=1' "$wf"; then
        printf 'hardware watchdog timeout ([RESET_REASON] dog=1)\n'
        return
    fi
    if grep -q 'RESET_REASON.* lockup=1' "$wf"; then
        printf 'CPU lockup ([RESET_REASON] lockup=1)\n'
        return
    fi
    if grep -q 'RESET_REASON.* resetpin=1' "$wf"; then
        printf 'reset pin asserted ([RESET_REASON] resetpin=1)\n'
        return
    fi
    if grep -q 'RESET_REASON' "$wf"; then
        printf 'unknown (the board booted but named no cause)\n'
        return
    fi
    printf 'unknown (the witness caught no boot banner)\n'
}
