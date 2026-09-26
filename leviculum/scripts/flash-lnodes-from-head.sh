#!/bin/bash
# flash-lnodes-from-head.sh — bring the attached LNodes to the tested commit,
# then prove they are actually running it.
#
# Extracted verbatim (behaviour-preserving) from the pre-periculum
# run-tier3-hw.sh, which is now a periculum driver. Flashing and firmware
# identity are leviculum's own concern: periculum tests whatever firmware it
# finds and deliberately leaves board preparation outside its scope
# (periculum CONCEPT.md, "prepare hook"). Without this step a hardware run
# tests current host code against whatever stale firmware happens to be on the
# boards, which makes every LNode result meaningless.
#
# Usage:
#   bash scripts/flash-lnodes-from-head.sh
#
# Protocol: every board whose firmware could NOT be confirmed to be HEAD is
# printed to STDOUT as one line
#
#   FW_UNVERIFIED <vid:pid>
#
# and nothing else goes to stdout. All narration goes to stderr. The exit code
# is ALWAYS 0: a board that is not touch-flashable (stuck in stock firmware
# with no touch handler, USB timeout, no bootloader) must not abort the run —
# the caller turns the FW_UNVERIFIED lines into a RED verdict that names the
# board, while the remaining boards still produce results.
#
# An ABSENT board is never flashed and never reported here: absence is a clean
# device-count skip, not a stale-firmware failure.
#
# env:
#   LEVICULUM_SKIP_FLASH=1   skip entirely (report nothing, exit 0)

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

log() { echo "$@" >&2; }

# The debug-serial reader, shared with the flash runner (see
# read_fw_build_banner below).
if [ ! -f "$REPO_DIR/leviculum-nrf/tools/fw-readback.sh" ]; then
    # The one exception to "exit code is ALWAYS 0": that rule is about boards
    # misbehaving, and a board misbehaving is a result. A missing in-repo file
    # is a broken checkout, and exiting 0 would let the run proceed with every
    # board's firmware unverified and nothing said about it.
    log "[CI_HW] ERROR: leviculum-nrf/tools/fw-readback.sh is missing;"
    log "        no board's firmware can be read back. Refusing to run."
    exit 1
fi
# shellcheck source=leviculum-nrf/tools/fw-readback.sh
. "$REPO_DIR/leviculum-nrf/tools/fw-readback.sh"

# True if a single USB ID is currently enumerated.
lnode_present() {
    lsusb -d "$1" >/dev/null 2>&1
}

# True only when every USB ID passed as an argument is currently enumerated.
ids_enumerated() {
    local id
    for id in "$@"; do
        lsusb -d "$id" >/dev/null 2>&1 || return 1
    done
    return 0
}

# Desktop notification for a flash failure that needs a manual RESET
# double-tap. Routed through hamster because the rig's display lives
# there (same channel run-tier3.sh notifies on). Best-effort.
notify_flash_failed() {
    local board="$1"
    # shellcheck disable=SC2029  # $board is meant to expand here: hamster has
    # no idea which board failed, the message is composed on this side.
    ssh hamster "notify-send -u critical 'Leviculum CI' 'LNode flash FAILED ($board): physical RESET double-tap needed'" \
        2>/dev/null || log "[CI_HW] WARN: notify-send for $board flash failure did not reach hamster"
}

# Resolve an LNode's debug ttyACM via udev properties. Each LNode exposes
# two CDC-ACM interfaces; interface 00 is the ASCII text debug console
# (interface 02 is the binary HDLC data link). Match ID_VENDOR_ID=1209, the
# board's product id and ID_USB_INTERFACE_NUM=00. The /dev/leviculum-*-debug by-serial
# symlinks an earlier version assumed do NOT exist on the rig, and serials
# are volatile, so we resolve dynamically rather than hardcode either.
# Echoes the matching /dev/ttyACM* on success; returns 1 if none found.
resolve_lnode_debug_port() {
    local pid="$1"   # 0001 (t114) | 0002 (rak4631 / Pocket-V2)
    local dev props
    set +f
    for dev in /dev/ttyACM*; do
        [[ -e "$dev" ]] || continue
        props=$(udevadm info -q property -n "$dev" 2>/dev/null) || continue
        grep -q '^ID_VENDOR_ID=1209$'       <<<"$props" || continue
        grep -q "^ID_MODEL_ID=${pid}\$"     <<<"$props" || continue
        grep -q '^ID_USB_INTERFACE_NUM=00$' <<<"$props" || continue
        echo "$dev"
        return 0
    done
    return 1
}

# Read the periodic firmware [FW_BUILD] banner from a CDC-ACM debug port,
# returning the last such line seen within <secs> (empty if none).
#
# The reader itself lives in leviculum-nrf/tools/fw-readback.sh, because the
# flash runner has to do exactly this to know which board it wrote (Codeberg
# #343) and two copies of a serial reader drift in exactly the detail that
# matters: DTR+RTS have to be asserted on open, since the debug CDC transmits
# only with them raised and a port opened without them is silent. One copy,
# and tools/test-fw-readback.sh exercises it against a pty.
read_fw_build_banner() { fw_read_banner "$1" "$2"; }

# Read the firmware [FW_BUILD] banner back over the debug serial and check
# its git_sha against the expected HEAD sha. A silent touch-flash that did not
# actually take (board re-enumerated but old firmware still resident) is caught
# here. This is a hardware-honesty check (same class as a board vanish): return
# 0 ONLY on a CONFIRMED git_sha match, NON-ZERO whenever the firmware cannot be
# confirmed == HEAD (definitive mismatch, no banner, or no debug serial). The
# caller turns non-zero into a firmware-unverified RED.
#
# Zero tolerance for false positives: the read is ROBUST FIRST. The debug serial
# can lag udev after a fresh re-enumeration, so resolving it is retried; a board
# running current firmware re-emits [FW_BUILD] every ~5 s, so the banner read is
# retried across ~3 windows (~24 s total) before concluding "no banner". A
# genuinely-current board catches its banner on the first window and returns 0
# immediately, so the happy path pays no extra latency.
verify_lnode_banner() {
    local board="$1" expect_sha="$2"
    local pid
    case "$board" in
        t114)    pid=0001 ;;
        rak4631) pid=0002 ;;
        *)       return 0 ;;
    esac
    # Robust debug-serial resolve: retry a few times before concluding absent.
    local port="" tries=0
    while (( tries < 3 )); do
        if port=$(resolve_lnode_debug_port "$pid"); then
            break
        fi
        port=""
        tries=$(( tries + 1 ))
        (( tries < 3 )) && sleep 2
    done
    if [[ -z "$port" ]]; then
        log "[CI_HW] WARN: $board debug serial (VID 1209 PID $pid intf 00) not found after retries; firmware sha UNVERIFIED"
        return 1
    fi
    log "[CI_HW] $board debug serial resolved to $port"
    # The firmware re-emits the banner every ~5 s. Read robustly over up to 3
    # windows (~24 s total) so a current board reliably yields at least one
    # banner before we ever conclude "none".
    local banner="" attempt=0
    while (( attempt < 3 )); do
        banner=$(read_fw_build_banner "$port" 8)
        [[ -n "$banner" ]] && break
        attempt=$(( attempt + 1 ))
    done
    if [[ -z "$banner" ]]; then
        log "[CI_HW] WARN: $board no [FW_BUILD] banner seen on $port after robust retries; firmware sha UNVERIFIED"
        return 1
    fi
    log "[CI_HW] $board banner: $banner"
    if [[ "$banner" == *"git_sha=$expect_sha"* ]]; then
        log "[CI_HW] $board firmware sha matches HEAD ($expect_sha)"
        return 0
    fi
    log "[CI_HW] WARN: LNode firmware sha mismatch ($board): expected $expect_sha, banner='$banner'; firmware UNVERIFIED"
    return 1
}

if [ -n "${LEVICULUM_SKIP_FLASH:-}" ]; then
    log "[CI_HW] LEVICULUM_SKIP_FLASH set; not flashing"
    exit 0
fi

head_sha=$(cd "$REPO_DIR" && git rev-parse --short HEAD)
log "[CI_HW] flashing LNodes from HEAD $head_sha"

# Flash only the boards currently enumerated. A board physically removed
# from the rig (Pocket-V2 unplugged → 1209:0002 absent) must NOT stall
# the flash waiting for a UF2 drive that will never appear; skip it.
#
# UF2_TIMEOUT=120 (vs the uf2-runner.sh default of 30) on the VM flash
# path: the libvirt USB-attach chain (1200-baud touch → nRF52 UF2
# bootloader → udev → virsh attach → VM enumeration → /dev/sda) has
# highly variable latency (~6 s typical, >30 s on a stalling run). The
# desktop-flash default on hamster keeps 30.
#
# T114 fleet, then Pocket-V2 fleet. Each just-target builds the embedded
# firmware (cargo run) and touch-flashes every attached board of that
# kind. A failing fleet warns + notifies but does not abort; a hard flash
# failure marks the board UNVERIFIED (collected in flash_failed_ids) so the
# verdict goes RED rather than silently testing whatever firmware remains.
flashed_ids=()
flash_failed_ids=()

if lnode_present 1209:0001; then
    if ( cd "$REPO_DIR" && UF2_TIMEOUT=120 just flash >&2 ); then
        log "[CI_HW] T114 flash invocation completed"
    else
        log "[CI_HW] WARN: LNode flash failed (t114)"
        notify_flash_failed t114
        flash_failed_ids+=( "1209:0001" )
    fi
    flashed_ids+=( "1209:0001" )
else
    log "[CI_HW] t114 (1209:0001) not enumerated; skipping flash"
fi

if lnode_present 1209:0002; then
    if ( cd "$REPO_DIR" && UF2_TIMEOUT=120 just flash-rak4631 >&2 ); then
        log "[CI_HW] Pocket-V2 flash invocation completed"
    else
        log "[CI_HW] WARN: LNode flash failed (rak4631)"
        notify_flash_failed rak4631
        flash_failed_ids+=( "1209:0002" )
    fi
    flashed_ids+=( "1209:0002" )
else
    log "[CI_HW] rak4631/Pocket-V2 (1209:0002) not enumerated; skipping flash"
fi

if (( ${#flashed_ids[@]} == 0 )); then
    log "[CI_HW] WARN: no LNodes enumerated; nothing flashed"
    exit 0
fi

# Settle: LNodes re-enumerate as fresh ttyACM after the touch reset.
# Bounded poll until the boards we actually flashed are back (or 30 s
# timeout) so the first scenario does not open a half-enumerated port.
log "[CI_HW] waiting for LNodes to re-enumerate (USB IDs ${flashed_ids[*]})"
waited=0
while ! ids_enumerated "${flashed_ids[@]}"; do
    if (( waited >= 30 )); then
        log "[CI_HW] WARN: LNodes not fully re-enumerated after ${waited}s; proceeding (verify marks any unconfirmed board UNVERIFIED)"
        break
    fi
    sleep 2
    waited=$(( waited + 2 ))
done
if ids_enumerated "${flashed_ids[@]}"; then
    log "[CI_HW] LNodes re-enumerated after ${waited}s"
fi
# Give udev a beat to settle the fresh ttyACM nodes + properties before
# resolve_lnode_debug_port iterates them.
sleep 2

# Verify every flashed (enumerated-at-flash-time) board runs HEAD. A board
# is UNVERIFIED if EITHER its `just flash` invocation returned non-zero
# (hard flash failure) OR verify_lnode_banner could not confirm its git_sha
# == HEAD (mismatch / no banner / debug serial gone). An UNVERIFIED board's
# USB id goes to stdout as an FW_UNVERIFIED line so the caller's verdict goes
# RED and names it. An ABSENT board was never in flashed_ids and is correctly
# untouched — absence is a clean device-count skip, not a stale-firmware
# failure.
for id in "${flashed_ids[@]}"; do
    case "$id" in
        1209:0001) board=t114 ;;
        1209:0002) board=rak4631 ;;
        *)         continue ;;
    esac
    if printf '%s\n' "${flash_failed_ids[@]:-}" | grep -qxF "$id"; then
        log "[CI_HW] WARN: $board ($id) hard flash failure — firmware UNVERIFIED; run will be RED"
        echo "FW_UNVERIFIED $id"
        continue
    fi
    if ! verify_lnode_banner "$board" "$head_sha"; then
        log "[CI_HW] WARN: $board ($id) firmware UNVERIFIED — cannot confirm HEAD $head_sha; run will be RED"
        echo "FW_UNVERIFIED $id"
    fi
done
exit 0
