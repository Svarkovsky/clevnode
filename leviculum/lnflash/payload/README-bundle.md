# lnflash

Puts Leviculum firmware on a Heltec Mesh Node T114 or a RAK4631 (the
WisMesh Pocket V2 and the other carriers built around that module).

    sudo ./lnflash

That is the whole thing. Everything it writes is in this directory —
nothing is downloaded, nothing needs installing, and it never touches
the network.

## What it does

1. Looks at the USB bus for a board it recognises.
2. Puts that board into its bootloader.
3. **Asks the bootloader what the board actually is** and stops if the
   answer is not a board this bundle carries firmware for.
4. Checks that the board's Nordic SoftDevice is a version our firmware
   can run on, and installs the right one first if it is not.
5. Writes the firmware.
6. Reads the board's debug port back to confirm the firmware it is now
   running is the firmware in this bundle.

It shows you all of that and asks before writing anything.

## Why sudo

The bootloader's drive is a `root:disk` block device, and `lnflash`
mounts it itself rather than relying on a desktop automounter that a
headless machine does not have. Without root it will identify your
boards and then stop.

## If your board is not already running our firmware

Most boards can be put into their bootloader by software, and `lnflash`
does that for you. Boards running other firmware — stock Meshtastic, for
instance — usually cannot, because that trick has to be implemented by
whatever is currently running.

When that happens, `lnflash` asks you to do it by hand:

> press RESET twice, quickly — the second press within about half a
> second of the first.

A drive appears when it worked, and `lnflash` carries on.

**On a WisMesh Pocket V2 there is no RESET button to press.** That case
has none on the outside; the contact sits behind a small pinhole beside
the USB socket and is double-tapped with a needle. `lnflash` says so in
those words when it asks, so you do not have to know this in advance.

## Options

    --dry-run        Say what is attached and what would happen. Changes
                     nothing at all — it will not even reboot a board
                     into its bootloader, since that is already a change
                     to your device.
    --check-bundle   Verify this bundle's own checksums and exit.
    --yes            Skip the confirmation prompt. Fails rather than
                     waits if a board needs the manual double-tap.
    --board NAME     Only flash this board, and refuse if what is
                     attached is a different one.
    --bundle PATH    Use a bundle somewhere else. Otherwise: the
                     `LNFLASH_BUNDLE` environment variable, then next to
                     the binary, then `/usr/share/lnflash`.

## If something goes wrong

**"the drive went away mid-flush"** is not an error. The bootloader
restarts the instant the last block lands, while the filesystem still
wants to write metadata, so the kernel reports a device that is no
longer there. That message means the transfer finished.

**An interrupted flash is recoverable.** `lnflash` never writes the
bootloader itself, so a board that was interrupted comes back into its
bootloader on a double-tap and can simply be flashed again.

**A board that seems completely dead** — no serial ports, no drive,
nothing on the bus — usually is not. Double-tap RESET and it will
reappear as a drive.

## What is in here

    lnflash                                  the program
    LICENSE                                  AGPL-3.0-or-later, ours
    firmware/manifest.toml                   what is here, and its checksums
    firmware/t114/leviculum-t114-*.uf2       our firmware, T114
    firmware/t114/s140_nrf52_7.3.0_*.hex     Nordic's SoftDevice
    firmware/t114/s140_nrf52_7.3.0_license-agreement.txt
                                             its licence, Nordic's own
    firmware/rak4631/leviculum-rak4631-*.uf2 our firmware, RAK4631

One image per board, and `lnflash` picks by asking your board's
bootloader what it is. There is no RAK4631 SoftDevice in here: every RAK
we have seen already runs a version our firmware can use, and rather than
ship a repair we have never had to perform, `lnflash` refuses — telling
you what it found — if it meets one that does not.

The SoftDevice is Nordic Semiconductor's, not ours, and is redistributed
under the terms in `s140_nrf52_7.3.0_license-agreement.txt`, the file
Nordic ships beside the blob — which is why it is a separate file
here rather than built into the program. Everything else is
AGPL-3.0-or-later: <https://codeberg.org/Lew_Palm/leviculum>.
