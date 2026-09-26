# Debugging with the Debug Probe (SWD)

A reliable, bootloader-free workflow for debugging the Leviculum nRF52840 LNode
firmware (RAK4631 / T114) over SWD, using a Raspberry Pi Debug Probe and probe-rs.
It replaces the UF2-bootloader / 1200-baud-touch / pinhole flashing dance and adds
a USB-independent log (RTT) plus full register and memory access.

## What it is for

Use SWD when you need to look inside the firmware, not just talk to it:

- Firmware crashes, hard faults and SoftDevice asserts.
- Reboots under load (the USB-CDC log port drops exactly when the board
  re-enumerates or reboots, so USB logging loses the interesting moment).
- Register and memory inspection (RESETREAS, heap, the reset-cause markers).
- Reliable flashing every time, with no bootloader and no double-tap.

RTT streams the firmware log straight through a reboot, because SWD is a separate
physical bus from USB.

## How: the commands

Everything runs through `scripts/probe-debug.sh <cmd> [board]`, wrapped by the
`just probe` recipe. Default board is `rak4631`; pass `t114` for the other.

| Command | What it does |
|---------|--------------|
| `just probe info` | chip and debug-port info; confirms wiring and APPROTECT open |
| `just probe reset` | reset the target over SWD |
| `just probe flash rak4631` | build and flash via SWD (no bootloader) then reset |
| `just probe rtt rak4631` | stream the live RTT log (firmware built with `rtt`) |
| `just probe gdb rak4631` | start a probe-rs GDB server on :1337 |
| `just probe read <hex-addr> <n>` | read n bytes of target memory |

For the reboot-cause repro under LoRa load, use `scripts/catch-reboot.sh
[board]`, described in the worked example below.

The probe binary and its behaviour are configurable by env var: `LEVICULUM_PROBE`
(default `2e8a:000c`, the RPi Debug Probe CMSIS-DAP), `PROBE_RS` (default
`~/.cargo/bin/probe-rs`), and `LEVICULUM_RTT=1` to build the RTT debug firmware.

## One-time setup

1. Install probe-rs: `cargo install probe-rs-tools` (needs >= 0.31). Optionally
   `gdb-multiarch`, `binutils-arm-none-eabi`, `picotool`, `tio` for GDB and probe
   maintenance.
2. Install a udev rule so the probe is reachable without root, for example
   `/etc/udev/rules.d/69-probe-rs.rules` from the probe-rs docs. sudo works as a
   fallback if you skip this.
3. The probe's OWN firmware must be >= 2.2.0 (CMSIS-DAP v2). Update it if probe-rs
   complains: see `scripts/probe-debug.sh fw-update`.

### Wiring

Probe `D` connector (Debug / SWD) to the board SWD pads:

| Probe `D` cable | Board pad |
|-----------------|-----------|
| orange          | SWCLK     |
| yellow          | SWDIO     |
| black           | GND       |

Do NOT connect 3V3/VTref or RST (the board is self-powered; RST is not needed).

- RAK4631 (inside the WisMesh Pocket V2): the RAK4631 module's 5-pin SWD port,
  pads labelled `SWDIO SWCLK RST 3V3 GND`. Pinch the V2 case open to reach it.
- T114: header `P1`, SWCLK = `P1.13`, SWDIO = `P1.15`, GND = any GND pin.

### Our lab rig (schneckenschreck via VFIO)

On our rig the probe plugs into a USB controller that is VFIO-passed-through to the
VM `schneckenschreck`, so it appears there as `2e8a:000c Raspberry Pi Debug Probe
(CMSIS-DAP)`. This is one example topology, not a requirement: a probe on a plain
host USB port works the same way. The VFIO path has its own failure modes, noted
under Known issues below.

## RTT (live log over SWD)

Build the firmware with the `rtt` feature, then it mirrors every log line to an RTT
channel that probe-rs streams, unaffected by a USB reboot:

    cd leviculum-nrf
    cargo build --release --bin rak4631 --features bsp-rak4631,rak-baseboard,rtt
    just probe flash rak4631      # flashes the rtt build over SWD
    just probe rtt   rak4631      # live log, including straight through a reboot

Production builds omit `rtt` and are byte-identical to before.

## GDB (for faults and live inspection)

    just probe gdb rak4631            # starts the server on :1337
    gdb-multiarch leviculum-nrf/target/thumbv7em-none-eabihf/release/rak4631 \
        -ex 'target extended-remote :1337'

CAUTION on the RAK: it runs the SoftDevice (BLE). Halting the core (a GDB
breakpoint) for more than a few ms can make the SoftDevice assert and reset the
chip. RTT (non-halting background memory access) is the safe default; use GDB
breakpoints only briefly and expect the SoftDevice may not tolerate a long halt.

## Worked example: catching the #50 reboot

`scripts/catch-reboot.sh rak4631` drives the airtime-max repro (SF10) while
continuously capturing the board debug port (USB if00, reopen-on-EOF so it survives
the reboot). After the first reboot it reports the cause from the boot banner:

- `[RESET_SITE] name=<touch|panic|hardfault|none(external)>`: which sys_reset
  fired. `none(external)` means none of our code, so a dependency or SoftDevice
  reset.
- `[RESETREAS]`, `[PANIC_COUNT]`, `[SD_FAULT]` from the boot banner.
- the ~25 log lines before the reboot (the context).

It reads the marker from USB (the boot banner), NOT RTT: `probe-rs attach` halts
the core, which perturbs the SoftDevice, whereas USB if00 capture is non-invasive.
Tune the load with the `RUNS`, `LORA_SF`, `LORA_CR`, `LORA_BANDWIDTH` env vars.

## Known issues and lessons (VFIO-passed-through probe)

These are lessons from our lab rig where the probe is VFIO-passed-through. On a
plain host USB port the wedge modes below are unlikely, but the RTT/SoftDevice
caveat still applies.

- **`probe-rs attach` (RTT) HALTS the core** to set up RTT. On the RAK (SoftDevice)
  this freezes the firmware while attached and can leave it halted on exit. For
  reading the reboot CAUSE use `catch-reboot.sh` (USB if00, non-invasive); use rtt
  only for short live inspection, and `just probe reset` afterwards.
- **A long-running `probe-rs gdb` server can WEDGE** in D-state (uninterruptible) on
  the VFIO USB and destabilise the whole rig USB (probe-rs ops and even USB serial
  reads start to hang; `pkill -9` and `/proc` reads block). Keep gdb sessions SHORT
  and bounded. To recover: `scripts/probe-debug.sh recover` (re-enumerates the probe
  USB); if that is not enough, physically replug the Debug Probe (and the RAK if its
  USB is wedged), or reboot the VM. Then `just probe reset`.
- For catching a reset CAUSE without gdb, the robust method is the reset-site marker
  plus `catch-reboot.sh` (USB), not a live gdb breakpoint.

## Troubleshooting

- `probe-rs` lists two probes (CMSIS-DAP + ESP JTAG): the scripts always select
  `--probe 2e8a:000c`, so this is handled.
- "Failed to open the debug probe" or udev warnings: the udev rule is missing or the
  user lacks access; re-run the one-time setup. (sudo works as a fallback.)
- "firmware ... outdated ... minimum 2.2.0": update the probe firmware (fw-update).
- "could not select JTAG": harmless; the scripts force `--protocol swd`.
- Probe present but no SWD: check the three wires (orange=SWCLK, yellow=SWDIO,
  black=GND) and that the board is powered.
- probe-rs or USB serial reads hang for minutes: a wedged `probe-rs gdb` (see above);
  run `recover` or replug the probe.
