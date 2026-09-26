# Flashing an LNode

Every board we flash is an nRF52840 carrying a factory UF2 bootloader.
That bootloader, not the firmware on top of it, is the part that decides
what a flashing tool can do. The rule that follows:

> **The application is not the board's identity. The bootloader is.
> Anything a tool needs to know before writing, it learns from the
> bootloader, never from the firmware currently running.**

A T114 may arrive carrying Meshtastic, Meshcore, microReticulum, RNode
firmware or our own. Each picks its own USB identity, and a crashed one
picks none. The bootloader underneath is the same in all five cases,
answers on a fixed USB ID, and publishes what it is in a text file.

## The three states a board can be in

**Application.** Our firmware enumerates `1209:0001` (T114) or
`1209:0002` (RAK4631), from `usb_vid`/`usb_pid` in
`leviculum-nrf/src/boards/t114.rs:137` and
`leviculum-nrf/src/boards/rak4631.rs:125`. Two CDC ports: interface 00
is the debug log, interface 02 the Reticulum transport. Both IDs are
squatted pid.codes test IDs, flagged as a TODO at
`leviculum-nrf/src/usb.rs:103`. Heltec stock firmware uses `239a:8071`.
Meshtastic on the SenseCAP Solar Node uses `2886:0059`, and calls itself
"XIAO-BOOT" while doing so. Nothing stops an application from naming
itself after a bootloader, which is the sharpest available argument for
the rule above: the product string is application data, not evidence.

**Bootloader (UF2/DFU).** A different USB ID entirely, which is why an
application-ID match can never be true while a board sits in DFU
(`leviculum-nrf/tools/uf2-runner.sh:332`). Measured on the rig:

| board | bootloader USB ID | mass-storage label |
| --- | --- | --- |
| T114 | `239a:0071` "Adafruit HT-n5262" | `HT-n5262` |
| RAK4631 | `239a:0029` "Adafruit WisBlock RAK4631" | `RAK4631` |
| XIAO nRF52840 | `2886:0044` "Seeed XIAO nRF52840" | `XIAO-BOOT` |

The last row is the Solar Node, measured 2026-08-17. Note that its
bootloader is the one row whose product string does **not** contain the
word BOOT, while its application's does.

**Dark.** Crashed firmware never enumerates at all. No touch reaches it;
only a physical double-tap does.

## Getting into the bootloader

Two mechanisms, and only one of them is ours to control.

**1200-baud touch.** The host opens a CDC port at exactly 1200 baud.
Our firmware answers the resulting `SET_LINE_CODING` in
`leviculum-nrf/src/usb.rs:168`, writes `DFU_MAGIC_UF2_RESET` (`0x57`) to
`GPREGRET` at `0x4000_051C` and resets. The bootloader reads that
retained register on the next boot and stays in mass-storage mode.
Measured latency from `stty ... 1200` to the bootloader appearing on
USB: 5 s on the T114, 3 s on the RAK.

**Double-tap RESET.** The bootloader's own mechanism, independent of any
firmware: it sets a RAM flag at startup and stays in DFU if a second
reset arrives while the flag is still live.

The touch only exists if the running firmware implements it. Ours does.
Stock Meshtastic does not, which is why a first flash away from
Meshtastic needs the manual double-tap (`Justfile:492`); for that case
Meshtastic offers its own admin command, wrapped as `just dfu-rak4631`.
For Meshcore, microReticulum and RNode firmware on nRF we have not
measured it.

**This is the load-bearing limit for any "fully automatic" tool.** There
is no universal software trigger. The double-tap is the only mechanism
that works regardless of what is running, and it needs a human. A tool
can be fully automatic for boards already carrying our firmware, which
is every re-flash, and must fall back to one clearly announced key press
otherwise.

**And what that key press *is* differs per board.** The WisMesh Pocket V2
has no externally accessible RESET at all: the contact is reachable only
through a hidden pinhole beside the USB socket, double-tapped with a
needle ([Recovery](../firmware/recovery.md), "the hidden-pinhole
caveat"). Telling its owner to press RESET twice sends them looking for a
button that does not exist, which is a worse failure than saying nothing
— they conclude the board is dead. So the wording is a board fact like
any other and lives in `lnflash/catalogue.toml`
(`[board.<name>.double_tap]`), not in a branch around the prompt; a board
that says nothing there gets the ordinary wording. `just flash-rak4631`
carries the same line into the developer runner's prompt through
`LEVICULUM_DOUBLE_TAP_HINT` (Codeberg #261).

### RNode firmware on the T114 lands a board in exactly that state

Mark's official RNode build for the Heltec T114
(`rnode_firmware_heltec_t114.zip`, v1.85/1.86) is an app-only Nordic DFU
package. Its application vector table reads SP `0x20040000` and reset
vector `0x00051819`, so the image is linked for a flash base around
`0x51000`. This board's factory bootloader with S140 7.3.0 runs
applications at `0x27000`, which is the base our own image is linked for
as well (`leviculum-nrf/memory.x:15`). `rnodeconf` pushes the app-only
package through the factory bootloader (`adafruit-nrfutil dfu serial
--package … -t 1200`), so it lands at `0x27000`; the bootloader jumps
there, reads a reset vector pointing into unprogrammed flash, and
hard-faults before USB comes up. The same dark board as the SoftDevice
mismatch below, from a different cause. `rnodeconf` itself calls the
T114 target experimental and AS-IS.

The operational consequence is the one this page keeps arriving at:
nothing is running to answer the 1200-baud touch, and a board that never
enumerates is not even a candidate for a tool to find, so it takes a
double-tap. After that nothing further is special — `lnflash` writes our
application at `0x27000` over whatever was resident and it boots. Where
the previous firmware was linked does not affect where the next one is
written. Measured on 2026-08-10 on serial `183004F712B4A7FE`:
`rnodeconf` v1.86 reported "Device programmed" and then could not reopen
the port, the board did not re-enumerate, a double-tap brought the
`HT-n5262` bootloader back, and `CURRENT.UF2` showed the RNode vector
table with reset `0x51819` sitting at `0x27000`. `lnflash` then flashed
over it and the board came up as `1209:0001` "leviculum T114".

## What the bootloader tells you

Mounting the mass-storage drive gives three files: `INFO_UF2.TXT`,
`INDEX.HTM` and `CURRENT.UF2`. The first is the entire basis for
deciding whether a board can be flashed. Read from the rig, verbatim
(CRLF line endings):

```
UF2 Bootloader 0.9.0-2-g836c8dc-dirty lib/nrfx (v2.0.0) lib/tinyusb (0.12.0-145-g9775e7691) lib/uf2 (remotes/origin/configupdate-9-gadbb8c7)
Model: HT-n5262
Board-ID: HT-n5262
Date: Jul  9 2024
SoftDevice: S140 7.3.0
```

```
UF2 Bootloader 0.4.3
Model: WisBlock RAK4631 Board
Board-ID: WisBlock-RAK4631-Board
Date: May 20 2023
Ver: 0.4.3
SoftDevice: S140 7.3.0
```

The `SoftDevice:` line is generated at runtime by the bootloader from
the SoftDevice actually installed, so it answers the version question
directly rather than by inference. The T114's `Board-ID` is exactly
`HT-n5262`, not a substring of something longer, which retroactively
justifies the substring match at
`leviculum-nrf/tools/uf2-runner.sh:222`.

Note the T114 bootloader build date matches Heltec's published
`HT-n5262-bootloader-20240709.hex`, so that board still carries its
factory bootloader.

## What a UF2 is allowed to write

The bootloader validates every block's target address before writing.
From `src/usb/uf2/uf2cfg.h` upstream:

```c
#define USER_FLASH_START   MBR_SIZE   // skip MBR included in SD hex
#define USER_FLASH_END     (BOOTLOADER_REGION_START - DFU_APP_DATA_RESERVED)
```

`in_app_space()` accepts `USER_FLASH_START <= addr < USER_FLASH_END`;
blocks below the window are skipped silently while still reporting
success, and blocks at or above it are rejected outright.

Both bounds are measurable without reading a single constant, because
the bootloader generates `CURRENT.UF2` from exactly that window. On both
rig boards it covers `0x1000` to `0x000EA000`. So `USER_FLASH_START` is
`0x1000`, one MBR page, and `USER_FLASH_END` is `0xEA000`. Nordic's own
`nrf_mbr.h` agrees: `#define MBR_SIZE (0x1000)`, carried through into
the `nrf-softdevice-s140` bindings as 4096.

The consequence is the important part: **the writable window opens
directly above the MBR and includes the whole SoftDevice region.** An
image converted from a SoftDevice hex has its MBR blocks declined and
the rest installed, which is precisely what the `skip MBR included in SD
hex` comment describes. A SoftDevice can therefore be replaced through
the ordinary mass-storage path, without touching the bootloader.

`leviculum-nrf/memory.x` used to contradict this, computing safe
application space as `0xEC000 - 0x27000` (788 KiB) — 8 KiB the
bootloader would have refused to write. It now links the application
against the bootloader's own window, `0xEA000 - 0x27000` = `0xC3000`
(780 KiB); the image is 344 KiB, so the change costs nothing today.

Everything at or above `0xEA000` survives every UF2 flash, because the
bootloader declines those blocks. Both persistence pages live there:
identity at `0xEC000` and the radio configuration at `0xEB000`
(`leviculum-nrf/src/boards/t114.rs`, `radio_store.rs`). A user's chosen
frequency therefore survives a firmware update as well as a reset.

Family IDs seen in practice:

| family | meaning | writes |
| --- | --- | --- |
| `0xADA52840` | nRF52840 application | application region |
| `0x239A0071` / `0x239A0029` | board-specific application | application region |
| `0xD663823C` | bootloader self-update | MBR, `0xF4000` region, `0xFD800`, UICR `0x10001000` |

Only the last one touches the bootloader itself. **A flashing tool must
never emit it.** With the bootloader intact, an interrupted flash is
recoverable: the board comes back into DFU and can be rewritten. Replace
the bootloader and a failure needs SWD to undo. There is a precedent for
the general risk: a bad application build once left the RAK4631 in a
HardFault boot loop that required opening the case to reach the internal
reset button (commit `43d25830`).

## The SoftDevice

Our firmware links against S140 v7.x and places its application at
`0x27000` (`FLASH` `ORIGIN`, `leviculum-nrf/memory.x:30`). A board
carrying S140 6.1.1
puts the boundary at `0x26000` instead, so the version is not cosmetic.

The bindings we compile against are generated from S140 **7.0.1**
(`SD_VERSION = 7000001`, which decodes as major 7, minor 0, bugfix 1 —
not 7.0.0 as `docs/ble5-broadcast-protocol3-spike.md:39` states). The
boards run 7.3.0. That works because Nordic keeps the ABI stable within
a major version, which is what makes `>=7.0.1, <8.0.0` the honest
version constraint rather than a guess.

### Reading the version without trusting the bootloader

`INFO_UF2.TXT` is the convenient source, but it depends on the
bootloader choosing to emit the line. The SoftDevice states its own
version in flash, independently: `nrf_sdm.h` puts the info struct at
offset `0x2000` above the MBR, with the version word `0x14` into it, so
the absolute address is `0x3014`. The encoding is
`major * 1000000 + minor * 1000 + bugfix`.

That address sits inside the range `CURRENT.UF2` dumps, so it can be
read without writing anything. Verified 2026-08-09 on both rig boards:
the word reads `7003000`, decoding to S140 7.3.0 and agreeing exactly
with what each board's `INFO_UF2.TXT` claims. A tool that cross-checks
the two is immune to a bootloader too old to report the line at all.

**The image is not part of this repo's source.** The crate dependency
(`leviculum-nrf/Cargo.toml:76`) supplies Rust bindings, not the blob.
The authoritative copy is Nordic's own distribution, downloaded
2026-08-10 to `~/coding/s140_nrf52_730/`, containing
`s140_nrf52_7.3.0_softdevice.hex` (md5
`29013ba2d0507c25f62dffa96b6c67af`), the API headers, release notes and
`s140_nrf52_7.3.0_license-agreement.txt`. Parsed, the hex covers
`0x0`-`0xB00` (MBR) and `0x1000`-`0x26498` (the SoftDevice itself,
149 KiB), which lands exactly below our `0x27000` origin once
page-aligned.

For a while the only copy on our machines was inside a Meshtastic
checkout. That copy is authentic — byte-identical after stripping CR,
both files 9726 lines — but it had been converted to LF, whereas
Nordic ships CRLF. Two consequences: an Intel-HEX parser must handle
both, and the vendored image should be Nordic's original so that no
build path leads through somebody else's repository.

Nordic's headers also settle two constants this page derived by other
means. `nrf_mbr.h` defines `MBR_SIZE (0x1000)`, and `nrf_sdm.h` gives
`SD_MAJOR_VERSION 7`, `SD_MINOR_VERSION 3`, `SD_BUGFIX_VERSION 0` —
encoding to exactly the `7003000` both rig boards report from `0x3014`.

### The mismatch is a soft brick, not a dead board

Flashing our application onto a board still carrying 6.1.1 produces a
device that goes dark: the old SoftDevice forwards to `0x26000`, finds
no vector table there because our image starts a page higher, and
crashes before USB initialises. No CDC ports, no bootloader drive,
nothing on the bus. It looks hardware-dead and is not. The bootloader
region at `0xF4000` is never touched by an application flash, so a
physical double-tap always brings the UF2 drive back. The 1200-baud
touch is useless here, because the application never runs far enough to
answer it.

Confirmed on 2026-08-08 on the T114 with serial `183004F712B4A7FE`,
which had been written off as bricked for weeks. Its `INFO_UF2.TXT` read
`SoftDevice: S140 6.1.1`. That board was never part of the 2026-05
spike, which names only `DEC9947DAD9D2869`, so it is direct evidence for
the factory state: **factory T114 boards ship S140 6.1.1**, as
`leviculum-nrf/memory.x:5` claims.

### The SoftDevice carve-out: the RAK4631 states the constraint and ships no remedy

The version constraint is the same on both boards, `>=7.0.1, <8.0.0`, and
for the same reason: our image is based at `0x27000` and cannot start on
a 6.1.1 boundary. What differs is what the bundle can do about a
violation.

For the T114 the remedy is measured end to end — a genuine 6.1.1 board
was repaired unattended on 2026-08-10, recorded under "Verified on
hardware" below. For the RAK4631 (Codeberg #261) it is **unmeasured**.
Our only RAK, serial `DEC9947DAD9D2869`, has carried 7.3.0 since we first
flashed it, so we have never seen the failing state on that board and
cannot say what a factory Pocket V2 ships. `memory.x:5` claims both
boards leave the factory on 6.1.1, but that line predates the T114
measurement that confirmed it for the T114 alone.

So the bundle carries no SoftDevice for the RAK4631, deliberately. A
precondition without a remedy is allowed by construction — `Requires` and
`Remedy` are separate tables, and `flow::resolve` answers an unmet
constraint with no remedy by refusing:

    rak4631 needs SoftDevice >=7.0.1, <8.0.0 and the board has 6.1.1
    (bootloader and flash agree), but this bundle carries no remedy for
    that. Nothing was written.

That is the honest outcome for a case nobody has seen: "I cannot fix
this, here is why" beats writing anyway, and it beats shipping a repair
path whose only evidence is that the same blob works on a different
board. The opposite arrangement is what the loader refuses outright — a
remedy with no precondition to trigger it will not load.

**What would close this.** One factory or Meshtastic-stock Pocket V2 read
through `lnflash --dry-run` under sudo, which prints the `SoftDevice:`
line off `INFO_UF2.TXT` without writing anything. If it reads 6.1.1, the
same vendored S140 7.3.0 hex serves this board too — it is an nRF52840
blob, not a board-specific one — and the carve-out becomes one line in
`scripts/lnflash-bundle.sh`'s board list plus the payload pair beside it.
Until somebody reads one, the gap stays visible here rather than assumed
closed.

### Installing the SoftDevice

`adafruit-nrfutil`'s serial DFU protocol does not work against the
Heltec bootloader; the 2026-05 spike got "Timed out waiting for
acknowledgement" (`f517a172`). Do not retry it. The mass-storage path
works, and needs no SWD probe:

    python3 ~/coding/meshtastic/bin/uf2conv.py -f 0xADA52840 -c \
      -o s140_7.3.0.uf2 \
      ~/coding/meshtastic/bin/s140_nrf52_7.3.0_softdevice.hex

Copy the result onto the mounted bootloader drive and `sync`. The
application flashed earlier boots immediately afterwards; it was intact
all along, only the SoftDevice beneath it was wrong.

The conversion is deterministic, so its output can be checked before
anything is written. Reproduced 2026-08-09:

| property | value |
| --- | --- |
| output size | 311 296 bytes, 608 blocks |
| family | `0xADA52840` |
| ranges | `0x0`-`0xB00` and `0x1000`-`0x26500` |
| highest byte touched | `0x26500`, below the `0x27000` app base |

The last row is the one that matters: the update cannot reach the
application, which is why the app survives it.

**Eleven of those 608 blocks are never written.** They carry the MBR
below `0x1000` and the bootloader declines them, silently and with a
success return. The block counter still sees all 608 arrive and reboots
on the last one, so nothing about the transfer looks unusual. This is
harmless, since the MBR is already present and identical, but it means
an application-family UF2 can never replace an MBR, and a report that
counts copied blocks is not evidence that all of them landed.

### Why we cannot simply ship it

The SoftDevice is under Nordic's five-clause BSD variant, and two of
those clauses decide the architecture of any flashing tool we build.
This is a reading of the licence text, not legal advice.

Clause 2 permits redistribution in binary form, provided the copyright
notice, the conditions and the disclaimer travel with the distribution.
Clause 4 restricts use to Nordic silicon, which our case satisfies.
Clause 3 is trivially satisfiable. So handing the blob to a user is
allowed, as long as the licence goes with it.

The obstacle is the combination with our own licence. Clause 4 limits
what the software may be used *for*, and clause 5 forbids modification,
decompilation and disassembly outright. AGPL-3.0 grants every recipient
the right to use and modify the whole work for any purpose, and permits
no additional restrictions of that kind. A blob carrying clauses 4 and 5
therefore cannot become part of one combined work with AGPL code.

The practical consequence: **the SoftDevice must not be linked into an
`lnflash` binary via `include_bytes!`.** That would make it part of the
executable and put the two licences in direct conflict. Shipping it
alongside as a separate file, with Nordic's own licence file next to it,
is ordinary aggregation and does not have that problem.

**Decided (2026-08-09): we ship it, as a separate file with its licence
beside it**, sourced from Nordic's own distribution rather than a
third-party checkout. Our own firmware images travel the same way, even though
being ours they could be embedded. One payload layout beats a split
where some images live inside the binary and others outside, and it is
what makes the bundle below the extension point for new boards. The
binary stays a single static executable; it just is not the only file.

**Ship Nordic's own licence file, not a copy of the text.** The
distribution includes `s140_nrf52_7.3.0_license-agreement.txt`, and it
differs from the widely circulated `LICENSE-NORDIC` in exactly one line:
its notice reads `Copyright (c) 2007 - 2020, Nordic Semiconductor ASA`
where the other says only `Copyright (c) Nordic Semiconductor ASA`.
Clause 2 obliges us to reproduce *the above copyright notice*, so the
file that travels with the blob is the one Nordic shipped alongside it.
It also names its own product and version, which the circulated variant
does not. That is what `lnflash/payload/t114/` vendors, next to Nordic's
CRLF original of the hex.

Note also that Meshtastic vendors the blob under GPL-3 without any
accompanying Nordic notice, so their practice is not the precedent it
was taken for; it fails clause 2 on its face. The `nrf-softdevice`
project is the counter-example worth copying: it is MIT/Apache licensed
and places a `LICENSE-NORDIC` in every crate that carries Nordic
material. That project has no copyleft conflict to solve, so it
demonstrates correct attribution, not that the AGPL question goes away.

One further detail. Converting the hex to UF2 does not alter a byte,
only the container, so it is hard to read as the "modification" clause 5
prohibits; still, distributing the untouched hex and converting at
runtime avoids the question entirely.

## CURRENT.UF2 as a backup

The bootloader exposes the installed flash as `CURRENT.UF2`, 1.9 MB
covering `0x1000`-`0xEA000` under the board-specific family ID. Filtering
it to blocks at or above `0x27000` and renumbering `blockNo`/`numBlocks`
yields a restorable application image. Verified on both rig boards on
2026-08-09: read, filtered to 3120 of 3728 blocks, written back, and
each board returned with its original serial and firmware
(`[FW_BUILD] git_sha=bb7c4f64` on the T114), `PANIC_COUNT total=0`.

That makes a flash reversible for the user who wants their previous
firmware back, at the cost of one file copy before writing. The
SoftDevice portion of the dump is not needed for restore and is filtered
out; keeping it would only re-write identical bytes.

**It also identifies what is installed, without running it.** The dump is
the application region, so the strings in it are the application's. Read
off the Solar Node on 2026-08-17 it gave Meshtastic 2.7.15, build
`567b8ea`, build target `seeed_solar_node`, and an occupancy of 92.4 per
cent that distinguishes a programmed board from a blank one. Two uses
follow. A tool can name what it is about to overwrite instead of
reporting that it found "a board", and where the `Board-ID` is ambiguous
the foreign firmware's own build target often names the carrier that the
bootloader does not. The second use is inference from a third party's
build strings and belongs in a prompt to the user, never in a silent
decision to write.

## Practical details that bite

**The USB serial number may change between modes, and whether it does is
board-specific.** The T114 reports `183004F712B4A7FE` as an application
and `12B4A7FE183004F7` in the bootloader: the two 32-bit words are
swapped. The Solar Node reports `40E37463CA8A59DF` in both, unchanged
(measured 2026-08-17). Anything correlating a device across app to
bootloader to app must therefore accept both forms rather than assume
either. That is what `same_serial` (`lnflash/src/usb.rs:140`) does: it
tests equality first and the swap only as an alternative, so a board
that keeps its serial is matched as readily as one that swaps
it. The older runner is unaffected because it
only compares serials in application mode
(`leviculum-nrf/tools/uf2-runner.sh:252`).

**Writing needs root.** The mass-storage device appears as `/dev/sdX`
owned `root:disk`. Automounting assumes a desktop stack that a headless
host does not have. A single self-contained binary can read USB identity
from sysfs and issue the touch through termios without any external
tool, but it cannot write the drive unprivileged.

**A successful write ends in a kernel error.** The bootloader reboots
the moment the final UF2 block lands, while the filesystem still wants
to flush metadata, producing `device offline error ... lost async page
write`. This is the normal completion path, not a failure
(`leviculum-nrf/tools/uf2-runner.sh:199`).

**A copy returning 0 does not mean the flash took.** Verify that the
application re-enumerated and that the bootloader drive is gone
(`leviculum-nrf/tools/uf2-runner.sh:273`). Stronger still, read the
periodic `[FW_BUILD]` banner off the debug port and compare the git SHA,
as `scripts/flash-lnodes-from-head.sh:133` does.

**More than one board can be in its bootloader at once, and the wrong
one is usually first.** The volumes are anonymous mass storage; only
`Board-ID` distinguishes them. A tool that takes the first UF2 volume it
finds and stops looking will be handed the same wrong volume on every
retry, refuse it every time, and never examine the board it was asked to
flash. `find_uf2_drive` therefore enumerates every candidate and
`poll_matching_drive` selects across the whole set
(`leviculum-nrf/tools/uf2-volumes.sh`).

**A mount you make is a mount you owe back.** Mounting a volume to read
its `Board-ID` and then leaving it behind because it was the wrong board
is worse than not looking: the leaked mount shadows every later board in
the search path and survives until somebody unmounts by hand. Volumes
this tooling mounts go under `/run/leviculum-uf2/<device>` — one mount
point per device, never the single shared `/mnt`, so a foreign volume
cannot occupy the only slot — and are released on every exit path.

**A give-up message must report, not assert.** The runner used to end
every failure with "app never re-enumerated" even when no volume for the
board had ever been found, naming a symptom that had not been reached.
It now names the volumes it saw and their `Board-ID`s.

## Structuring a flashing tool

Everything above is mechanism. What follows is the shape a tool takes
if it has to survive more boards than the two we support today.

Start with how wide the field actually is. The Meshtastic tree carries
162 variants, and they collapse onto very few flashing mechanisms:

| chip family | variants | how it is flashed |
| --- | --- | --- |
| nRF52840 | 50 | UF2 mass storage |
| RP2040 / RP2350 | 12 | UF2 mass storage |
| ESP32 / S3 / C3 / C6 / S2 | 93 | ESP ROM bootloader over serial |
| STM32 | 5 | its own path |

**Two transports cover 155 of the 160 flashable variants**, and we
already own both: the UF2 path in `leviculum-nrf/tools/uf2-runner.sh`
and the ESP path behind `Justfile:575`, which drives `esptool`. The
work is not building 162 things. It is separating two mechanisms
cleanly and turning everything else into data.

### Four axes, not one "board"

Treating a board as one indivisible unit is the design mistake to
avoid. A board is four independent answers, and a new device rarely
changes all four:

**Identify** — what is attached? For UF2 boards the truth is the
`Board-ID` in `INFO_UF2.TXT`; for ESP32 it is the chip identity the ROM
bootloader reports. Never the USB ID of the running application, which
belongs to whatever firmware happens to be installed.

That truth is authoritative but not always sufficient, and the condition
under which it is sufficient can be stated exactly:

> **A `Board-ID` carries a write decision only where it is bound to the
> same physical unit as the radio wiring. Where the two are bound to
> different units, a match is a hint.**

Three real bindings, all measured in 2026-08:

- **Coupled.** On the RAK4630 the SX1262 wiring and the bootloader both
  belong to the module. Twelve different carriers report
  `WisBlock-RAK4631-Board` and share seven identical pin numbers. The key
  is exact, and one image serves all of them.
- **Decoupled by the vendor.** Heltec records the same bootloader product
  string `HT-n5262` for the Mesh Node T114, for MeshSolar and for the
  Mesh Pocket. The first two share our wiring; the Mesh Pocket puts CS on
  `P0.26` and BUSY on `P0.15`. The identifier belongs to a bootloader
  shared across models while the wiring belongs to the model.
- **Decoupled by construction.** The Seeed XIAO is an MCU module with the
  radio outside it, so a SenseCAP Solar Node and a DIY XIAO with
  different radio wiring both report `nRF52840-SeeedXiao-v1`.

In both decoupled cases a manifest entry keyed on `info_uf2_board_id`
alone is not a decision. Such a board needs a second discriminator or an
explicit question naming the model, and the honest failure is to stop and
ask rather than to write the more likely of two images. The cost of
getting this wrong is not a failed flash but a board driving the wrong
pins, which on hardware carrying a power amplifier is a repair rather
than a retry.

The cheaper answer, where it is available, is to make the ambiguity stop
mattering. The RAK4631 looks like the same problem, since the bare
module and the Pocket V2 share a `Board-ID` and have separate builds,
but the baseboard build degrades cleanly on a bare module: the display
is found by an I2C probe and its task exits when nothing answers
(`leviculum-nrf/src/display.rs:158-164`), the button pin is pulled up so
it never reads as pressed (`leviculum-nrf/src/button.rs:36`), the GNSS
task waits on a UART that stays silent, and the battery task publishes
into a watch channel whose only subscriber is the display that is not
running. One image therefore covers both, at 47.6 KiB of flash and
2.5 KiB of RAM that the bare module does not use, and the RAM cost is
already proven affordable because the same image runs on a Pocket V2
with the same chip and the same memory. Prefer that over asking a
question, and reserve the discriminator for boards whose peripherals
genuinely cannot be probed.

**Enter** — how does it reach a programmable state? 1200-baud touch,
physical double-tap, a DTR/RTS sequence on ESP32, BOOTSEL on RP2040.

**Transport** — how do the bytes get in? The two above.

**Verify** — did it take? Re-enumeration plus the `[FW_BUILD]` banner
with a matching git SHA.

Crossing all four sit **preconditions**. The SoftDevice version is the
only one today; a bootloader minimum version would be the next. A
precondition must be data that names its own remedy, never a special
case in code.

Separated this way, a new nRF or RP2040 board is data entry, and a new
chip family costs exactly one new transport.

### The bundle is the extension point

Since third-party blobs cannot be linked in anyway, the payload lives
beside the binary and the manifest describes it:

```
lnflash                     # board-agnostic binary
firmware/
  manifest.toml             # index, checksums, licences
  t114/
    leviculum-t114-0.8.0.uf2
    s140_nrf52_7.3.0_softdevice.hex
    s140_nrf52_7.3.0_license-agreement.txt
  rak4631/
    leviculum-rak4631-0.8.0.uf2
```

The second board arrived in Codeberg #261 and cost exactly what this
structure promised: a catalogue entry, an image, and a line in the board
list `scripts/lnflash-bundle.sh` walks. No Rust changed except the
per-board wording of the one prompt a human has to act on — see
"Getting into the bootloader" below.

Note what the RAK directory does *not* contain. The SoftDevice remedy is
per board and this bundle carries none for that one; see "The SoftDevice
carve-out" below for why, and what a board that needs it is told.

The data is split in two by what it describes. **Board facts** — USB
IDs, `Board-ID`, flash geometry, the SoftDevice constraint — are
properties of the hardware and do not change when a release is cut, so
they live in `lnflash/catalogue.toml`, compiled into the binary:

```toml
[board.t114]
family      = "nrf52840"
transport   = "uf2-msc"
entry       = ["touch-1200", "double-tap"]
identify    = { info_uf2_board_id = "HT-n5262" }
requires.softdevice = ">=7.0.1, <8.0.0"
```

**Release facts** — which images this tarball carries and what they
hash to — are the bundle's:

```toml
[bundle]
version = "0.8.0"

[board.t114.app]
file    = "t114/leviculum-t114-0.8.0.uf2"
sha256  = "..."

[board.t114.remedy.softdevice]
file    = "t114/s140_nrf52_7.3.0_softdevice.hex"
license = "t114/s140_nrf52_7.3.0_license-agreement.txt"
convert = "hex-to-uf2"
```

The split is Codeberg #342. Before it, both halves were in the bundle
manifest, and `run()` loaded it before dispatching — so `--set-time`
and `--set-telemetry`, which read nothing but the USB IDs, refused to
start without a firmware bundle on disk. Activation is configuration,
not firmware (#236/#238); somebody pointing a node they already own at
an LXMF address was being sent to hunt for an image they had no use
for. The configure-only sessions now take the catalogue and never
locate a bundle at all; the flashing paths locate one and still fail
with `no bundle found`, naming every place they looked.

A bundle built before the split still loads: its board-fact sections
are ignored, and the catalogue in the binary reading them is the more
trustworthy of the two copies anyway, since binary and bundle ship
together.

A new board still needs no new binary in the sense that matters — it is
data entry, in the catalogue plus one image. The `license` field is not
bureaucracy: it makes shipping a third-party blob without its licence
impossible by construction, which is exactly the mistake described
above. Board names stay identical to the firmware-side ones in
`leviculum-nrf/src/boards/mod.rs:11`, so that two namespaces never
diverge.

### Identify in two stages, write only after

Before entering the bootloader we know only "some USB device". The
reliable identity exists only afterwards. The order is therefore:
find candidates, enter, **confirm identity there**, check
preconditions, check the checksum, and only then write. No write may
rest on a guessed identity. Commit `362c1c2d` records why: a T114 image
once landed on a RAK4631 during bring-up. Several devices on the bus
must each be resolved individually rather than assuming "the one UF2
drive".

### Confirm by reading back, do not infer

The stage above resolves identity before the write. Afterwards there is
a second, separate question — *which board actually received it* — and
a UF2 mass-storage volume gives no help with it at all: the volume
carries no board serial, so a runner that finds one has no way from the
volume alone to say whose it is.

`uf2-runner.sh` used to answer by pairing the volume with a candidate
from its own USB enumeration, which yields a board of the right *type*
and not the board that owns the volume. With two T114s attached, one in
DFU and one running, it wrote to the one in DFU and reported the other.
Measured twice on the rig, 2026-08-23 and 2026-08-24, naming the
opposite board each night — the answer follows enumeration order, which
is neither stable nor related to which board was in the bootloader
(Codeberg #343). The same gap made `flash CONFIRMED` mean "a board of
this type re-enumerated" rather than "the named board runs the named
image".

The fix is a read-back, and it turns attribution into a measurement:

- The firmware carries `leviculum_nrf::FW_BUILD_STAMP`, one contiguous
  literal `git_sha=<sha> dirty=<bool>`, and prints it after
  `[FW_BUILD] ` at boot and every five seconds thereafter.
- The runner greps that same literal out of the **flat image it is
  about to write** (`tools/fw-readback.sh`, `fw_image_stamp`). Not out
  of `git rev-parse`: that describes the working tree at the moment of
  the question rather than the bytes going to the board, and it cannot
  express a dirty tree at all, so two different images built from one
  commit would both answer with that commit.
- After the copy it opens the candidate's debug port and requires the
  stamp to match. If the named board is carrying something else, the
  other attached candidates are asked, and the one that answers with
  the image is the one the summary names.

Three outcomes, kept apart on purpose, because they need different
things done to them:

| Outcome | Meaning | Reported as |
|---|---|---|
| match | the named board answers with this image | `flash CONFIRMED — serial=… reports …, read back from the board` |
| mismatch | it answers with a different image | `flash NOT CONFIRMED — serial=… reports <its stamp>, the image that was written is <ours>` |
| no answer | no debug port, or nothing on it | `flash UNCONFIRMED — serial=… did not answer on its debug port …; that is not the same as carrying the wrong image` |

A board can legitimately fail to answer — crashed firmware, a port that
never appears — and silence must never be reported as wrong firmware,
nor as right firmware. When the write cannot be bound to any board at
all, the runner says exactly that (`flash UNATTRIBUTED — … the runner
does not know which board it wrote`) and exits non-zero. A guess in that
position is what produced the ticket.

Two constraints the read-back has to respect, both long established on
the rig: the debug CDC transmits only with **DTR and RTS asserted**, so
a port opened without them is silent for reasons that have nothing to
do with its firmware; and the **`by-id` symlinks are the stable
handle** (`-if00` debug, `-if02` transport), because a `/dev/ttyACM*`
number is a position and moves between enumerations — trusting a
position for an identity is the defect itself.

`tools/test-fw-readback.sh` (`just nrf-fw-readback`) drives all of this
against stubbed boards, so it runs with no hardware.

#### The line has to be one the board said after the reset

Reading a window and keeping the last `[FW_BUILD]` in it is not the same
question as "what is running now". The port's input queue was filled
before the question was asked, and a line in it is an answer to a
question nobody asked — on 2026-09-09 that made `lnflash` report

```
1 of 2 board(s) confirmed running the firmware in this bundle.
  1-1 (rak4631): not confirmed — Some(WrongBuild { saw: "ead0bce", expected: "daa8b8e" })
```

about a board whose own debug port said `git_sha=daa8b8e` seconds
later. `ead0bce` was the build that had been running before the flash.
A completely successful flash exited non-zero, which stops any script
that chains on it (Codeberg #378).

The confirmation therefore establishes a boundary the tool itself
creates, and only accepts what comes after it
(`lnflash/src/verify.rs`, `fresh_banner`):

- **The board must have left the bus.** If the bootloader is still
  there when the wait times out, the board never rebooted into what was
  written, and nothing a port says next is about the new image. That is
  `Absent`, not a build claim.
- **The port's input queue is flushed on open** — one `tcflush`, the
  same one every control transaction does — so no line from the
  previous session can be read as an answer.
- **The debug port is resolved to its `by-id` path and the open is
  proved** against the board's bus identity before a byte is read
  (`entry::wait_for_interface_tty`, `flow::open_debug`). A bare
  `/dev/ttyACM` number is a position, and on a multi-board run the
  board it moves to is the one still carrying the firmware this flash
  replaced.
- **Only a complete line counts.** A half-read `git_sha=daa8b8e` parses
  as `daa8` and would be reported as a *different* build — a failure
  manufactured out of a partial read.
- **The budget is three banner periods, 15 s.** The firmware emits one
  every 5 s, so a healthy board answers inside the first; the margin
  covers a board whose banner task ticks just before the port opens and
  a line the flush cut in half. A board still silent after that is
  silent, and silence is `unknown`.

The exit code carries the same three-way split, because "not confirmed"
and "failed" need different things done about them:

| exit | meaning |
|---|---|
| 0 | every board was written and named the build in this bundle |
| 1 | the flash failed: a board did not come back, or named a different build, or nothing was written |
| 2 | every board took the write, none contradicted it, and at least one could not be read back |

Naming the old sha as if it were current is the defect. A confirmation
that cannot decide says `unknown` and never names a sha it did not
read.


`just nrf-shellcheck` (Codeberg #345) is the static half of the same
coverage: `shellcheck -x` over `leviculum-nrf/tools/*.sh` and
`scripts/flash-lnodes-from-head.sh`, which sources them. The scripts
carried `# shellcheck` directives long before anything ran them, and an
`SC2034` and an `SC2015` sat in the runner until #341 and #343 happened
to remove them. It runs from the repo root because the `source=`
directives name repo-relative paths.

### The radio configuration belongs to the flash

A board that has just been written runs the compiled `eu_medium`
profile, and until the firmware learned to remember a configuration
(`radio_store.rs`) there was nowhere else for one to live: every host
that bound the board had to send the frequency again, and a standalone
LNode with no host had no way to be on anything else.

With the flash page in place the honest moment to choose is the flash
itself, once. `lnflash` therefore ends its sequence with a fifth step:
after the `[FW_BUILD]` banner confirms the write, it asks "Flash
default radio settings? [Y/n]". Enter takes the eu868 preset; "n"
opens a preset menu — eu868, us915, au915, custom — where custom is
the five-number field-by-field path. The choice goes to the board's
transport CDC; since #238 `lnflash` opens with a capability probe and
sends the configuration inside the control envelope
(`docs/src/firmware/usb-control-envelope.md`), falling back to the
legacy magic-prefixed frame `lnsd` still uses
(`leviculum_core::rnode::build_radio_config_frame`, HDLC-framed,
answered by `RADIO_CONFIG_ACK`) when the probe goes unanswered. Non-interactively,
`--radio-preset <eu868|us915|au915>` names a preset outright; it
cannot be combined with the `--radio-*` value flags (two ways to state
one configuration).

### The presets are community profiles, not conformance claims

The names look like regulatory bands, which is why the menu spells out
what they actually are: the settings each regional Reticulum community
has converged on (the Reticulum wiki's "Popular RNode Settings"), with
the regulatory situation documented next to them rather than implied
by the name.

| preset | freq (Hz)  | BW (Hz) | SF | CR  | txpower |
|--------|------------|---------|----|-----|---------|
| eu868  | 869463000  | 125000  | 8  | 4/5 | 22 dBm  |
| us915  | 914875000  | 125000  | 8  | 4/5 | 22 dBm  |
| au915  | 925875000  | 250000  | 9  | 4/5 | 22 dBm  |

**eu868** is the ReticulumNet consensus channel and the compiled
firmware default. The channel sits in the band ERC 70-03 Annex 1
designates as h1.7 (869.4-869.65 MHz, 500 mW e.r.p.), so 22 dBm
conducted stays lawful up to roughly 7 dBi of antenna gain, and the
derived long-term airtime lock arms the ETSI 10 % duty cycle.

**us915** follows the US community, and the tool prints a note when it
is chosen because power conformance is not the whole story: FCC
15.247(a)(2) requires at least 500 kHz of occupied bandwidth for
non-hopping digital systems, which a fixed-frequency 125 kHz node does
not meet. The preset ships the profile the entire known US Reticulum
scene runs; whether that is lawful for a given deployment is the
operator's call, and the note says so at selection time, not in a
footnote.

**au915** matches the Western Sydney and Brisbane communities. Under
the ACMA LIPD class licence (915-928 MHz, digital modulation, 1 W
EIRP, no minimum bandwidth) 22 dBm plus a typical antenna sits far
under the limit — sourced from two agreeing secondary references, as
the primary ACMA text could not be retrieved.

**eu433** is decided but deferred: ERC 70-03 allows 10 mW e.r.p. at
433.05-434.79 MHz, i.e. 10 dBm, and the SX1262 driver's lowest PA
profile is 14 dBm. Offering the preset today would transmit 4 dB over
the limit, so asking for it is refused with that reason until the
driver can produce 10 dBm.

Three details are not obvious:

**The step cannot fail the flash.** It runs after the firmware is on
the board and confirmed. A board that does not answer is a board
running the compiled default, which is a warning and a re-run, not a
failed flash.

**Two of the seven wire fields are not the user's to state.** The
preamble is derived from the PHY the way the RNode firmware derives it
(`derive_preamble_symbols`); a preamble belonging to a different SF
mis-prices airtime on both sides. The long-term airtime lock is sent at
the value the firmware would have derived for the chosen frequency,
because the frame's own presence (`lt_alock_present`) switches that
derivation off — sending zero would persist "no duty-cycle limit" onto
a board whose operator only picked a frequency.

**Validation happens before the board is touched.** `--radio-sf 3` and
a bandwidth the SX1262 has no register code for are refused at the
command line. Left to the board, an unparseable frame is silently
dropped and looks exactly like a dead port.

An unavailable preset refuses the same way: `--radio-preset eu433`
stops the run with the 10 dBm reason before any board is enumerated,
rather than shipping a board 4 dB over the limit.

### The real bottleneck is not the tool

A manifest invites the belief that the whole palette is a matter of
configuration lines. It is not. **Our firmware supports exactly two
boards today**, `bsp-t114` and `bsp-rak4631`. A manifest entry without a
matching firmware build is an empty promise, and a LoRa board needs more
than an entry: pin mapping, TCXO voltage, SPI frequency and maximum
transmit power all live in `BoardConfig` and have to be right per device
and measured.

The structure should therefore follow the firmware side's growth rather
than anticipate it. The value appears immediately and independently of
it: a tool that identifies a board reliably, checks the SoftDevice
precondition, and says "I do not know this board" instead of writing to
it is what is missing today.

### Deliberately not

No plugin system with shared libraries; it contradicts the statically
linked binary. No scripting language in the manifest — such fields
become a programming language within a year; when declarative data is
not enough, the answer is a new transport in Rust. And no fetching
firmware from the network: it contradicts "no infrastructure" and adds
an attack surface to a tool that overwrites other people's devices with
root privileges.

The structure proves itself on the second board, not the first. Building
the UF2 transport for the T114 alone, but already split along the four
axes and driven by the manifest, is only a claim until the RAK4631 —
same transport, different board ID, different bootloader — runs through
without a code change.

## Verified on hardware

The factory path was exercised end to end on 2026-08-10, on the T114 with
serial `183004F712B4A7FE`, using the tarball rather than the repo build.

A genuine factory state was reconstructed rather than waited for: the
6.1.1 SoftDevice was extracted from Meshtastic's combined hex and
trimmed to the window `0x1000`-`0x27000`, which drops 11 MBR blocks and
**135 blocks that would have written bootloader, bootloader settings and
UICR**. Without that trim the bootloader rejects those blocks and the
whole copy fails; had it accepted them, it would have been the brick
path. The board then went dark exactly as predicted, and did not stay in
DFU — a physical double-tap was required, confirming the 2026-08-08
observation.

From there the tool ran unattended: it read `SoftDevice 6.1.1` with
bootloader and flash agreeing, found `>=7.0.1, <8.0.0` violated,
installed the SoftDevice (608 blocks, 11 declined), and then — the part
worth naming — the board rebooted into the *old* application, which now
booted because its base finally matched the installed SoftDevice. The
tool touched it back into the bootloader, re-read the version as 7.3.0,
and wrote the current application. That re-entry is the step a naive
implementation gets wrong by answering "wait for the bootloader" with the
pre-reboot sysfs entry.

That the application runs at all is the independent proof that the
SoftDevice is 7.3.0: an image based at `0x27000` cannot start on 6.1.1.
The board was confirmed afterwards over the debug port at
`git_sha=d82ccfc` with LoRa cycling normally.

Also confirmed in the same session: a board already sitting in its
bootloader is handled without a redundant touch, several devices on one
bus are resolved individually, and a RAK4631 on the same hub is neither
offered nor written to, because it had no catalogue entry.

That last clause is now history rather than behaviour. Codeberg #261 gave
the RAK4631 its entry, so a Pocket V2 on the same hub is found, hinted at
its own board, brought into its own bootloader and written its own image
— and a bundle that carries no image for it says so in those words rather
than falling silent. The property the 2026-08-10 session actually
demonstrated is the one that survives: **each device is resolved
individually, and no write rests on an identity the bootloader did not
publish.** The structure's own claim — "the second board proves it" — is
what #261 collected on: the RAK reached hardware readiness through a
catalogue entry, an image, and one board-list line, with no change to the
transport, the identify step or the write path.

## Open questions

- Does the touch handler exist in Meshcore, microReticulum or RNode
  firmware on nRF? Unmeasured; assume no, and fall back to the
  double-tap prompt.
- Nothing further on the SoftDevice. Provenance, version constraint and
  the two independent ways to read the installed version are settled
  above.
- Whether boards leaving the factory *today* still carry 6.1.1 is
  unknown; the measured board is one unit from one batch. A tool must
  read `INFO_UF2.TXT` and decide, never assume a version.
