# shellcheck shell=bash
# debug-witness.sh — put a witness on every LNode debug port for a run.
#
# Codeberg #353. `run-tier3-hw.sh` sources this, starts one
# scripts/debug-witness-reader.py per discovered LNode after the flash phase,
# and stops them when periculum is done. This file holds the parts that can be
# decided without a board — which ports are LNode debug ports, where each one's
# witness file goes, and what the reader is invoked with — so
# scripts/test-debug-witness.sh can assert on them against a fixture device
# list instead of against the rig.
#
# Defines functions and constants only; sourcing it starts nothing.
#
# WHERE THE READERS START, AND WHY THERE
#
# After the flash phase, never before it. `just flash` touch-flashes at 1200
# baud and the board re-enumerates as a UF2 volume; a process holding the debug
# port through that fights the flash, and did — twice, on 2026-08-26, which is
# why the reviewer's own capture caught nothing. `flash-lnodes-from-head.sh`
# also reads the `[FW_BUILD]` banner back off the same if00 port to confirm the
# board runs HEAD, and a second reader there would split those bytes.
#
# Discovery therefore runs after the flash too: the ports enumerated before it
# are not the ports that exist after it.
#
# WHICH PORT
#
# if00 — the ASCII debug console. if02 is the binary HDLC data link the
# scenario drives, and is never touched here.
#
# /dev/serial/by-id is preferred over /dev/ttyACM*, and not only for the reason
# fw-readback.sh prefers it (the name carries the board serial). The witness
# watches a path across a disconnect: the by-id name is derived from the USB
# serial and comes back identical, while a ttyACM number is a position and can
# change on re-enumeration. Watching a ttyACM path means watching a name the
# board may not return under. The ttyACM walk stays only as a fallback for a
# host without by-id links, and the reader's header says which kind it got.

WITNESS_LNODE_VID="1209"
# 0001 = T114, 0002 = RAK4631 / Pocket V2. The same two product ids
# flash-lnodes-from-head.sh flashes; an RNode (1a86:55d4, 303a:1001) has no
# such console and is not an LNode, so it is never given a witness.
WITNESS_LNODE_PIDS=("0001" "0002")

# Candidate device paths, one per line. Overridden wholesale by the fixture
# test — this is the only function here that looks at the machine's /dev.
witness_device_nodes() {
    local dev found=""
    for dev in /dev/serial/by-id/*-if00; do
        [ -e "$dev" ] || continue
        printf '%s\n' "$dev"
        found=1
    done
    [ -n "$found" ] && return 0
    for dev in /dev/ttyACM*; do
        [ -c "$dev" ] || continue
        printf '%s\n' "$dev"
    done
    return 0
}

# udev properties of one device, `KEY=value` per line. The test's second seam.
witness_device_props() {
    udevadm info -q property -n "$1" 2>/dev/null || true
}

# One property's value out of a property block.
witness_prop() {
    local key="$1" props="$2"
    printf '%s\n' "$props" | sed -n "s/^${key}=//p" | head -n 1
}

# Every LNode debug port currently present, as `<vid>:<pid>\t<serial>\t<path>`,
# sorted so the run log lists boards in a stable order.
#
# All three of vendor, product and interface number must match. Interface is
# what keeps the data port out: an LNode's if02 carries the same vid:pid and
# would otherwise be handed a witness that fights the scenario for it.
witness_lnode_debug_ports() {
    local dev props vid pid iface serial out=""
    while IFS= read -r dev; do
        [ -n "$dev" ] || continue
        props="$(witness_device_props "$dev")"
        vid="$(witness_prop ID_VENDOR_ID "$props")"
        [ "$vid" = "$WITNESS_LNODE_VID" ] || continue
        pid="$(witness_prop ID_MODEL_ID "$props")"
        case " ${WITNESS_LNODE_PIDS[*]} " in
            *" $pid "*) ;;
            *) continue ;;
        esac
        iface="$(witness_prop ID_USB_INTERFACE_NUM "$props")"
        [ "$iface" = "00" ] || continue
        serial="$(witness_prop ID_SERIAL_SHORT "$props")"
        [ -n "$serial" ] || serial="unknown"
        out="$out$vid:$pid	$serial	$dev"$'\n'
    done < <(witness_device_nodes)
    printf '%s' "$out" | sed '/^$/d' | sort
}

# Where one board's witness file lives.
#
# The vid:pid leads the name because that is the only thing the device-vanish
# watchdog knows about the board that vanished — it polls `lsusb -d <vid:pid>`
# and has no serial. The serial follows it so two boards sharing a vid:pid
# still get a file each. `:` becomes `_`: a colon in a filename is legal but a
# nuisance to paste into anything.
# Args: $1 = dir, $2 = vid:pid, $3 = serial
witness_log_path() {
    printf '%s/%s-%s.log' "$1" "${2/:/_}" "$3"
}

# Every witness file belonging to one vid:pid, so the RED banner can name the
# file rather than the directory.
# Args: $1 = dir, $2 = vid:pid
witness_files_for() {
    local dir="$1" vidpid="$2" f found=""
    for f in "$dir/${vidpid/:/_}"-*.log; do
        [ -f "$f" ] || continue
        printf '%s\n' "$f"
        found=1
    done
    [ -n "$found" ] || return 1
    return 0
}

# The reader invocation for one board, one argument per line. Split out from
# the spawn so the fixture test can assert on what WOULD be run without
# running it.
# Args: $1 = reader path, $2 = out file, $3 = vid:pid, $4 = serial, $5 = device
witness_reader_argv() {
    printf '%s\n' python3 "$1" --port "$5" --out "$2" --label "$3/$4"
}

# PIDs of the readers this shell started.
WITNESS_PIDS=()

# Start one reader per discovered LNode debug port. Prints one
# `[CI_HW] WITNESS: ...` line per board so the run log says who is watching
# what, and one line if nothing was found — a run with no witness must say so
# rather than look like a run that needed none.
# Args: $1 = witness dir, $2 = reader path
witness_start() {
    local dir="$1" reader="$2"
    local vidpid serial dev out argv started=0
    mkdir -p "$dir"
    # boards.tsv joins the two halves of the vanish accounting. The device
    # watchdog knows a vid:pid and no serial (it polls `lsusb -d`); periculum's
    # BOARD_RESET lines know a serial and no vid:pid. Nothing else on this rig
    # holds both, and this discovery already does, so it writes the map down
    # while it has it — after the flash, when the ports are the ports that will
    # exist for the rest of the run.
    : > "$dir/boards.tsv"
    while IFS=$'\t' read -r vidpid serial dev; do
        [ -n "$dev" ] || continue
        printf '%s\t%s\n' "$vidpid" "$serial" >> "$dir/boards.tsv"
        out="$(witness_log_path "$dir" "$vidpid" "$serial")"
        mapfile -t argv < <(witness_reader_argv "$reader" "$out" "$vidpid" "$serial" "$dev")
        "${argv[@]}" </dev/null >>"$dir/reader-stderr.log" 2>&1 &
        WITNESS_PIDS+=("$!")
        started=$((started + 1))
        echo "[CI_HW] WITNESS: $vidpid $serial on $dev -> $out"
    done < <(witness_lnode_debug_ports)
    if ((started == 0)); then
        echo "[CI_HW] WITNESS: no LNode debug port found; a board that resets this run will not be explained"
    fi
}

# Stop every reader. Called on the normal path AND from an EXIT trap: a reader
# left alive past the run would be found by the next run's flash phase, which
# is the bug this file was careful not to introduce in the first place.
witness_stop() {
    local pid
    for pid in "${WITNESS_PIDS[@]:-}"; do
        [ -n "$pid" ] || continue
        kill -TERM "$pid" 2>/dev/null || true
    done
    for pid in "${WITNESS_PIDS[@]:-}"; do
        [ -n "$pid" ] || continue
        wait "$pid" 2>/dev/null || true
    done
    WITNESS_PIDS=()
}
