# LNode Firmware: Bootloader Entry and Recovery

The nRF52840 boards use the Adafruit UF2 bootloader: it appears as a
mass-storage drive, and writing a `.uf2` file to that drive flashes the
device. This page covers how to enter that bootloader (touch-free and
manual), the board-specific caveats, what survives a re-flash, and what
to do when USB stays dark.

> **Physical-device steps.** The author of this page cannot operate a
> board. Every step that presses a button, taps a pinhole, or observes a
> drive appearing is **derived from source — requires the physical
> device**. The commands and mechanisms are quoted from
> `leviculum-nrf/README.md` and the `Justfile`; only the hardware
> outcome is un-verified here.

## Entering the UF2 bootloader

### Touch-free (1200-baud), the common case for T114

When the LNode firmware is already running, the host can drop it into the
bootloader without any physical interaction: it opens the board's
transport CDC port at 1200 baud, the firmware intercepts the line-coding
change, writes a retained-register magic value, and soft-resets into the
Adafruit UF2 bootloader.

> The host opens each T114's transport CDC port at 1200 baud, the
> firmware intercepts the line-coding change, writes a retained-register
> magic, and soft-resets into the Adafruit UF2 bootloader. No physical
> button press required.
> (`leviculum-nrf/README.md:27`)

All `just flash*` recipes use this path automatically when the device is
running our firmware. **(derived from source — requires the physical
device.)**

### Manual RESET double-tap, the fallback

A physical double-tap of the RESET button forces the UF2 bootloader
regardless of firmware state. You need it when the firmware on a specific
board has crashed or never reached USB init — a panic before the
1200-baud handler is installed, a stack overflow, or a hardware fault. In
a `just flash` batch the runner detects this per device via the
UF2-drive-polling timeout and prompts for that specific board only; the
rest of the batch keeps flashing touch-free.

> the firmware on a specific T114 has crashed or never reached USB init
> (panic before the handler is installed, stack overflow, hardware
> fault). The runner detects this per device via the UF2-drive-polling
> timeout and prompts for that specific T114 only.
> (`leviculum-nrf/README.md:38`)

**(derived from source — requires the physical device.)**

## WisMesh Pocket V2 (RAK4631): the hidden-pinhole caveat

The RAK WisMesh Pocket V2 has **no externally accessible RESET pin**, so
the ordinary double-tap-the-button trick does not apply. On this board:

- **First flash from stock Meshtastic.** Stock Meshtastic has no
  1200-baud-touch handler, so the touch-free path does not work yet. Use
  the software DFU command instead:

  ```sh
  just dfu-rak4631 /dev/ttyACM0
  ```

  which runs `meshtastic --port /dev/ttyACM0 --enter-dfu`. This
  firmware-side admin command is the only software-only DFU entry on a
  board with no accessible RESET pin. Requires the `meshtastic` CLI
  (`pip install meshtastic`). (`Justfile:306-314`)

- **Manual fallback.** Where the software command is unavailable, the
  bootloader is reached by a **needle double-tap in the hidden pinhole**
  — there is no visible reset button; the reset contact is reachable only
  through a small pinhole, double-tapped with a needle. *(This pinhole
  detail comes from project field notes, not from the firmware source;
  the source confirms only that the device "has no externally accessible
  RESET pin", `Justfile:307-308`.)*

- **After our firmware lands**, subsequent flashes use the touch handler
  in `src/usb.rs` and the DFU recipe is no longer needed.
  (`Justfile:309-310`)

> **Do not flash foreign nRF52 firmware onto the Pocket V2 without a
> recovery plan.** Project field experience is that prebuilt
> third-party nRF52 firmware may not boot on this RAK board (USB stays
> dark). Because the only software DFU entry is *firmware-side*, a board
> that boots into a non-responsive image and exposes no RESET pin can be
> hard to recover. *(This caveat is project knowledge; it is not stated
> in the firmware source, which documents only the missing RESET pin and
> the firmware-side DFU command.)*

All steps in this section are **derived from source / project notes —
requires the physical device.**

## Identity persistence across updates

A re-flash does **not** change the node's Reticulum address. The device
stores its Reticulum identity in internal flash and preserves it across
firmware updates.

> The device stores its Reticulum identity in internal flash and
> preserves it across firmware updates.
> (`leviculum-nrf/README.md:42`)

Mechanically, the firmware loads the identity from a dedicated flash page
at boot and only generates (and saves) a new one when none is present:

```text
if id_store.load() => Some(identity)   -> "Identity loaded from flash"
else                                   -> generate new, then save
```

(`leviculum-nrf/src/bin/t114.rs:128-164`,
`leviculum-nrf/src/bin/rak4631.rs:164-201`. The identity lives on the
board's `identity_flash_page`, e.g. `0xEC000` on the T114,
`leviculum-nrf/src/boards/t114.rs:144`.) Flashing new firmware rewrites
the program region but leaves that page intact, so the node keeps its
address. You can confirm the loaded identity on the debug port: the boot
log prints `Identity loaded from flash` and an `[IDENTITY]` line with the
full hash (`leviculum-nrf/src/bin/t114.rs:180-184`).

## When USB stays dark

If the board enumerates nothing on USB after a flash or a bad image:

1. **Force the bootloader manually.** On a T114, double-tap RESET to get
   the UF2 drive regardless of the running image
   (`leviculum-nrf/README.md:38`). On a Pocket V2, use the hidden-pinhole
   needle double-tap (see above) — the board has no accessible RESET pin
   (`Justfile:307-308`).
2. **Re-flash the known-good LNode firmware** once the UF2 drive appears:
   `just flash` (T114) or `just flash-rak4631` /
   `just flash-rak4631-pocket` (RAK4631). See [Flashing](flashing.md).
3. **Read the debug port** at 115200 baud to see why it crashed — the
   firmware replays the previous boot's HardFault/panic post-mortem and
   the persistent log on the next boot:

   ```sh
   picocom /dev/leviculum-debug -b 115200
   ```

   Look for `[HARDFAULT_PMRT]`, `[PANIC_PMRT]`, and `[PERSISTENT_LOG]`
   lines (`leviculum-nrf/src/bin/t114.rs:87-120`;
   `leviculum-nrf/README.md:59-60`).

**(All hardware steps: derived from source / project notes — requires the
physical device.)**

> **ESP32 RNodes vs. nRF52 LNodes.** The bricking risk above is specific
> to the nRF52 LNodes. The ESP32-based RNodes (LilyGO T-Beam) have a
> mask-ROM download bootloader and cannot be bricked: a failed flash is
> always recoverable by re-running the flash recipe. The nRF52 LNodes
> (T114, RAK4631) are different — a bad external image can leave the
> device USB-dark, which is why a recovery plan matters here.
> (`Justfile:316-320`)
