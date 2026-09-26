#!/usr/bin/env bash
# Leviculum firmware debugging via the Raspberry Pi Debug Probe (SWD / probe-rs).
#
# Bypasses the UF2 bootloader entirely: reliable SWD flashing, RTT logging,
# register/memory access, and reboot-cause catching for the nRF52840 LNodes
# (RAK4631 / T114). Fool-proof: validates the probe, firmware version and chip
# before acting, and always selects our CMSIS-DAP probe (not the ESP-JTAG).
set -euo pipefail

PROBE="${LEVICULUM_PROBE:-2e8a:000c}"   # RPi Debug Probe (CMSIS-DAP), explicit
CHIP="nRF52840_xxAA"
NRF_TARGET="thumbv7em-none-eabihf"
PROBE_RS="${PROBE_RS:-$HOME/.cargo/bin/probe-rs}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ELFDIR="$REPO/leviculum-nrf/target/$NRF_TARGET/release"

die() { echo "probe-debug: ERROR: $*" >&2; exit 1; }

board_features() {
  local rtt=""
  [ -n "${LEVICULUM_RTT:-}" ] && rtt=",rtt"   # LEVICULUM_RTT=1 -> build the RTT debug firmware
  case "$1" in
    rak4631) echo "bsp-rak4631,rak-baseboard${rtt}" ;;
    t114)    echo "bsp-t114${rtt}" ;;
    *) die "unknown board '$1' (use rak4631 | t114)" ;;
  esac
}

require_probe() {
  command -v "$PROBE_RS" >/dev/null 2>&1 || die "probe-rs not found at $PROBE_RS"
  lsusb 2>/dev/null | grep -qi "2e8a:000c" || die "Debug Probe (2e8a:000c) not on USB. Plugged in + passed to the VM?"
  local ver
  ver=$(lsusb -d 2e8a:000c -v 2>/dev/null | awk '/bcdDevice/{print $2; exit}')
  case "$ver" in
    2.[2-9]*|2.[1-9][0-9]*|[3-9].*) : ;;
    *) echo "probe-debug: WARN: probe firmware '$ver' may be < 2.2.0; see 'probe-debug.sh fw-update'." >&2 ;;
  esac
}

build() {
  local b="$1"
  ( cd "$REPO/leviculum-nrf" && cargo build --release --bin "$b" --features "$(board_features "$b")" )
}

p() { "$PROBE_RS" "$@" --chip "$CHIP" --probe "$PROBE" --protocol swd; }

usage() {
  cat <<'EOF'
probe-debug.sh - Leviculum firmware debugging via the RPi Debug Probe (SWD)

  info                 chip / debug-port info (confirms wiring + APPROTECT open)
  reset                reset the target over SWD
  build  <board>       build the firmware ELF
  flash  <board>       build + flash via SWD (no bootloader) + reset
  rtt    <board>       stream the RTT log live (HALTS the core; needs the rtt feature)
  gdb    <board>       start a probe-rs GDB server (keep SHORT; can wedge on VFIO)
  read   <addr> <n>    read n bytes of target memory at hex <addr>
  recover              re-enumerate the probe USB to clear a wedged probe-rs hang
  fw-update            how to update the probe's own firmware

  board = rak4631 | t114   (default: rak4631)

Env: LEVICULUM_PROBE (default 2e8a:000c), PROBE_RS (default ~/.cargo/bin/probe-rs)
EOF
}

cmd="${1:-help}"; shift || true
case "$cmd" in
  info)   require_probe; p info ;;
  reset)  require_probe; p reset ;;
  build)  build "${1:-rak4631}" ;;
  flash)
    b="${1:-rak4631}"; require_probe; build "$b"
    [ -f "$ELFDIR/$b" ] || die "ELF not found: $ELFDIR/$b"
    echo "probe-debug: flashing $b via SWD (SoftDevice region preserved)..."
    p download "$ELFDIR/$b"
    p reset
    echo "probe-debug: flashed + reset OK: $b" ;;
  rtt)
    b="${1:-rak4631}"; require_probe
    [ -f "$ELFDIR/$b" ] || die "ELF not found: $ELFDIR/$b (build first)"
    # NOTE: `probe-rs attach` HALTS the core to set up RTT. On the SoftDevice (RAK)
    # this freezes the firmware while attached and may leave it halted on exit
    # (recover with `probe-debug.sh reset`). For reading the reboot CAUSE, prefer
    # catch-reboot.sh (USB if00, non-invasive). Use rtt only for live inspection.
    exec "$PROBE_RS" attach "$ELFDIR/$b" --chip "$CHIP" --probe "$PROBE" --protocol swd ;;
  gdb)
    b="${1:-rak4631}"; require_probe
    # WARNING: a long-running probe-rs gdb server on the VFIO-passed-through probe
    # has wedged in D-state (uninterruptible) and destabilised the rig USB. Keep
    # gdb sessions SHORT and bounded; if it wedges, run `probe-debug.sh recover`
    # (and physically replug the probe if that is not enough).
    echo "probe-debug: GDB server on :1337. Connect (keep it SHORT):"
    echo "  gdb-multiarch $ELFDIR/$b -ex 'target extended-remote :1337'"
    exec "$PROBE_RS" gdb --chip "$CHIP" --probe "$PROBE" --protocol swd ;;
  recover)
    # Re-enumerate the Debug Probe's USB to clear a wedged probe-rs / D-state hang
    # (kills the held probe handle without touching the target). May need a
    # physical replug if the hang is in the host VFIO path.
    port=$(for d in /sys/bus/usb/devices/*/idVendor; do
             [ "$(cat "$d" 2>/dev/null)" = "2e8a" ] && basename "$(dirname "$d")"; done | head -1)
    [ -n "$port" ] || die "probe USB port not found (already gone? physically replug it)"
    echo "probe-debug: re-enumerating probe at USB $port ..."
    echo "$port" | sudo tee /sys/bus/usb/drivers/usb/unbind >/dev/null 2>&1; sleep 2
    echo "$port" | sudo tee /sys/bus/usb/drivers/usb/bind   >/dev/null 2>&1; sleep 3
    echo "probe-debug: re-enumerated. Stuck 'probe-rs gdb' may remain (D-state); a VM"
    echo "  reboot or a physical probe replug clears it fully." ;;
  read)
    require_probe; p read b8 "${1:?addr}" "${2:-16}" ;;
  fw-update)
    cat <<'EOF'
Update the probe's own firmware (if probe-rs reports firmware < 2.2.0):
  1. Download the latest debugprobe.uf2 from the raspberrypi/debugprobe releases.
  2. Pinch the long sides of the probe case to pop the lid; hold BOOTSEL and
     replug the probe -> it mounts as an "RPI-RP2" volume.
  3. Copy debugprobe.uf2 onto RPI-RP2; it flashes and reboots automatically.
EOF
  ;;
  help|*) usage ;;
esac
