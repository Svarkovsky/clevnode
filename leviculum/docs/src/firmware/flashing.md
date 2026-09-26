# LNode Firmware: Building and Flashing

There are two ways to put our firmware on a board, and they exist for
different people.

| | `lnflash` | `just flash*` |
| --- | --- | --- |
| for | anyone with a board | developers and CI |
| needs | the bundle, and root | this checkout and the embedded toolchain |
| builds firmware | no, it carries it | yes, from the working tree |
| identifies the board | from its bootloader | from the USB id you configure |
| boards today | T114, RAK4631 | T114, RAK4631 |

If you just want our firmware on a board, use `lnflash`. If you are
changing the firmware and want your build on a board, use `just flash`.

> **Physical-device steps.** The author of this page cannot flash a
> board, so any step that writes to or resets real hardware is marked
> **derived from source — requires the physical device**. The commands
> themselves are quoted verbatim from the `Justfile` and
> `leviculum-nrf/README.md`; only the *outcome* on hardware is
> un-verified here.

## `lnflash`, the distributable flasher

`lnflash` is a single static binary with the firmware beside it. It
needs no toolchain, no Python, no network, and nothing installed: the
point of the bundle is that a stranger can unpack it and run it.

```sh
tar xzf lnflash-<version>.tar.gz
cd lnflash-<version>
sudo ./lnflash
```

(`Justfile:50-51`)

**It works out what the board is, rather than being told.** That matters
because a board arrives carrying whatever its last owner put on it:
stock firmware, Meshtastic, MeshCore, RNode firmware, ours, or a build
that crashes before it reaches USB. Each of those picks its own USB
identity, so the running firmware cannot be trusted to say what the
hardware is. `lnflash` therefore finds candidates on the USB bus, brings
each into its bootloader, and only there asks what the board actually
is, from the bootloader's own `INFO_UF2.TXT`. The identity that a write
rests on can only come from that reading, which is enforced in the type
system rather than by convention (`lnflash/src/lib.rs:15-21`). Then it
checks the SoftDevice precondition, installs a matching SoftDevice first
if needed, writes the firmware, and reads the board's debug port back to
confirm what is now running.

Nothing is written before all of that has been shown and confirmed.

**Root is required.** The bootloader's drive is a `root:disk` block
device, and `lnflash` mounts it itself rather than assuming a desktop
automounter that a headless host does not have. Without root it will
identify the attached boards and then stop.
(`lnflash/src/main.rs:31-32`)

**One key press is sometimes unavoidable.** Getting into the bootloader
by software has to be implemented by whatever firmware is currently
running. Ours implements it, so every re-flash is touch-free. Stock
Meshtastic does not, so a first flash away from it needs a physical
double-tap of RESET, the second press within about half a second of the
first. `lnflash` detects that case and asks for it in plain words.
There is no universal software trigger, and a tool that claimed
otherwise would be lying.

### Options

`--dry-run` reports what is attached and what would happen, changing
nothing at all, not even rebooting a board into its bootloader.
`--check-bundle` verifies the bundle's own checksums and exits.
`--board NAME` refuses to write if what is attached is a different
board. `--yes` skips confirmation for automation and fails rather than
waits when a board needs the manual double-tap. Radio settings can be
given at flash time with `--radio-preset` (`eu868`, `us915`, `au915`) or
the individual `--radio-freq`, `--radio-bw`, `--radio-sf`, `--radio-cr`
and `--radio-txpower` flags; `--no-radio` leaves the board's stored
configuration alone. (`lnflash/src/main.rs:36-95`. The board keeps what
it is given across resets and across the next flash, so this is part of
the flash rather than a later configuration step.)

The bundle is looked for in this order: `--bundle PATH`, then
`$LNFLASH_BUNDLE`, then the directory holding the binary, then
`/usr/share/lnflash`. (`lnflash/src/main.rs:36-39`)

The full user-facing text ships inside the bundle as its `README`
(`lnflash/payload/README-bundle.md`), including what the alarming but
harmless "the drive went away mid-flush" message means.

### Building a bundle

```sh
just lnflash-bundle
```

Cross-compiles the firmware, converts it to UF2, builds the musl-static
binary, stages Nordic's SoftDevice next to Nordic's own licence file,
generates a manifest with checksums, and verifies the result. Output
lands under `target/lnflash/`. The first run takes minutes because of
the firmware build; `SKIP_FIRMWARE=1` reuses an existing ELF while
iterating on the bundle itself. (`Justfile:52-60`)

Everything in the bundle comes from this checkout. A bundle built out of
a foreign tree would be exactly the hidden dependency our
clone-and-deploy policy forbids. (`Justfile:54-56`)

### Which boards the bundle carries

**Today: the T114 and the RAK4631** (WisMesh Pocket V2 and every other
carrier built around the RAK4630 module). Boards are data rather than
code, so a new board is a catalogue entry plus a firmware build, not a
new binary — and an entry without a firmware build is an empty promise,
so the shipped bundle carries what we actually build.

The RAK4631 image is the `bsp-rak4631,rak-baseboard` build, the same one
`just flash-rak4631-pocket` produces. Not because it is the richer build,
but because [How far one firmware build
reaches](../concepts/board-support-scope.md) already decided it: one
build serves a pinout family, and everything the Pocket V2 baseboard adds
degrades harmlessly on a bare module — the display is found by an I2C
probe and its task exits when nothing answers, the button is `Pull::Up`
so an absent one reads as not pressed, the GNSS task parks on a silent
UART, and the battery task publishes to a subscriber that is not running.
The bundle therefore does not ask which RAK you have, and the manifest
has no way to express two images for one `Board-ID`.

`scripts/lnflash-bundle.sh` walks a board list rather than naming boards
in its steps, so a third board is one more line in that list: the
firmware build, the UF2 conversion, the staging, the manifest sections
and the licence assertions against the finished tarball all derive from
it.

**The SoftDevice carve-out.** The T114 entry ships Nordic's S140 7.3.0
beside its licence, so a factory board carrying 6.1.1 is repaired and
then flashed. The RAK4631 entry ships no SoftDevice. It states the same
`>=7.0.1, <8.0.0` constraint, but whether a factory Pocket V2 carries
something that constraint refuses is **unmeasured** — our only RAK has
run 7.3.0 since we first flashed it. A board that violates the constraint
with no remedy in the bundle is refused with `Nothing was written` rather
than written blind. The full reasoning, and what one reading of a stock
board would take to close it, is under "The SoftDevice carve-out" in
[Flashing an LNode](../concepts/lnode-flashing.md).

**First flash on a Pocket V2 needs the pinhole.** That board has no
externally accessible RESET, so when the 1200-baud touch does not take —
which is every board still running stock Meshtastic — `lnflash` asks for
a needle double-tap in the hidden pinhole beside the USB socket by name,
and points at [Recovery](recovery.md). The bundle does not depend on the
`meshtastic` CLI for this; `just dfu-rak4631` below stays available in
this checkout, but a stranger with the tarball needs only a needle.

The design behind all of this, including why the bootloader rather than
the application is the board's identity, is in
[Flashing an LNode](../concepts/lnode-flashing.md).

## The developer path: building from this checkout

The rest of this page covers building the firmware here and flashing it
with the `just flash*` recipes.

### Prerequisites

Install the Rust embedded toolchain, the ARM cross-compiler (needed by
`nrf-sdc` for C-header bindgen), flip-link, and add your user to the
`dialout` group for serial-port access. Log out and back in after the
`usermod` so the new group membership takes effect.

```sh
rustup target add thumbv7em-none-eabihf
rustup component add llvm-tools
cargo install flip-link
sudo apt install gcc-arm-none-eabi
sudo usermod -aG dialout $USER
```

flip-link is the firmware linker. It relocates the stack to the bottom
of RAM so a stack overflow faults cleanly against the RAM floor instead
of silently corrupting memory. It is link-time only, with zero runtime
cost.

(`leviculum-nrf/README.md:12-19`)

### `--release` is mandatory

Always build and flash with `--release`. The debug profile does not fit
the nRF52840 flash — the image overflows FLASH by several hundred KB at
link time.

> The debug profile does not fit the nRF52840 flash (the image overflows
> FLASH by several hundred KB at link time) — always build and flash with
> `--release`; all `just flash-*` recipes already do.
> (`leviculum-nrf/README.md:65-67`)

Every `just flash*` recipe already passes `--release`, so following the
recipes below keeps you safe. The release profile is size-optimized
(`opt-level = "z"`, `lto = true`, `codegen-units = 1`); DWARF debug info
is kept in the `.elf` (`strip = "none"`, `debug = true`) for HardFault
post-mortem analysis, but the UF2 only carries loadable sections, so the
debug info does not bloat what lands on the device.
(`leviculum-nrf/Cargo.toml:146-156`)

### The build/flash workflow

The firmware crate `leviculum-nrf` is its own Cargo workspace, separate
from the repo-root workspace, and is cross-compiled. The flash recipes
therefore `cd leviculum-nrf` before invoking cargo. (`Justfile:534-535`)

A plain build (no flash) is:

```sh
cargo build --release
```

(`leviculum-nrf/README.md:23`)

Flashing wraps `cargo run`: the runner builds the release binary, then
copies the resulting UF2 onto each board's UF2 bootloader drive. The
UF2 conversion and copy happen inside the `cargo run` step — a bare
`cargo build` produces only the ELF.

> Build the firmware with `cargo build --release`. Flash with `just
> flash` (from the repo root), which wraps `cargo run --release --bin
> t114`.
> (`leviculum-nrf/README.md:23`)

### Touch-free vs. manual double-tap

For the **T114**, flashing is touch-free in the common case: the host
opens the board's transport CDC port at 1200 baud, the firmware
intercepts the line-coding change, writes a retained-register magic, and
soft-resets into the Adafruit UF2 bootloader. No button press.
(`leviculum-nrf/README.md:27`)

A **physical double-tap of RESET** is still needed when the firmware on a
specific T114 has crashed or never reached USB init (panic before the
handler is installed, stack overflow, hardware fault). The runner detects
this per device via a UF2-drive-polling timeout and prompts for that
specific board only; the rest of the batch keeps flashing touch-free.
(`leviculum-nrf/README.md:38`)

The **WisMesh Pocket V2 (RAK4631)** running stock Meshtastic has no
1200-baud-touch handler and no externally accessible RESET pin, so its
*first* flash needs either `just dfu-rak4631` (a Meshtastic admin
command, below) or the manual needle double-tap in the hidden pinhole.
Once our firmware is on the board, subsequent flashes use the touch path
automatically. (`Justfile:551-553`, `Justfile:570-578`. See
[Recovery](recovery.md) for the pinhole detail.)

## The flash recipes

Each recipe below is quoted from the `Justfile`. The cargo invocation is
**derived from source — requires the physical device** to actually write
firmware (it builds the same on any host, but only does something useful
with a board attached).

### `just flash` — every T114

Flashes **every attached T114** sequentially. Flashing all of them is
deliberate: if only one were flashed, a later multi-node test could run
against mixed firmware versions. Use this as your default for T114s.

```sh
cd leviculum-nrf && cargo run --release --bin t114 --features bsp-t114
```

(`Justfile:536-538`; rationale `leviculum-nrf/README.md:25`)

### `just flash-one PORT` — a single T114

Flashes one T114 by port path or udev symlink. Use it for A/B firmware
testing (one board on a new build, one on the old).

```sh
just flash-one /dev/leviculum-transport
just flash-one /dev/ttyACM3
```

Expands to:

```sh
cd leviculum-nrf && LEVICULUM_FLASH_ONLY=<PORT> cargo run --release --bin t114 --features bsp-t114
```

(`Justfile:544-549`; usage forms `leviculum-nrf/README.md:31-36`)

### `just flash-rak4631` — every RAK4631 (bare module)

Flashes every attached RAK4631 / WisMesh Pocket V2 with the bare-module
build (no baseboard peripherals).

```sh
cd leviculum-nrf && LEVICULUM_USB_PID=0002 LEVICULUM_BOARD_NAME=RAK4631 \
  LEVICULUM_UF2_BOARD_ID=WisBlock-RAK4631-Board \
  cargo run --release --bin rak4631 --features bsp-rak4631
```

(`Justfile:554-556`)

### `just flash-rak4631-one PORT` — a single RAK4631

Flashes one RAK4631 by port path or udev symlink.

```sh
just flash-rak4631-one /dev/ttyACM0
just flash-rak4631-one /dev/leviculum-rak-transport
```

Expands to:

```sh
cd leviculum-nrf && LEVICULUM_FLASH_ONLY=<PORT> LEVICULUM_USB_PID=0002 \
  LEVICULUM_BOARD_NAME=RAK4631 LEVICULUM_UF2_BOARD_ID=WisBlock-RAK4631-Board \
  cargo run --release --bin rak4631 --features bsp-rak4631
```

(`Justfile:558-562`)

### `just flash-rak4631-pocket` — WisMesh Pocket V2, full baseboard

Flashes with all RAK19026 baseboard peripherals enabled (display, GNSS,
battery). `--features rak-baseboard` aggregates the three baseboard
features. Use this for a complete WisMesh Pocket V2.

```sh
cd leviculum-nrf && LEVICULUM_USB_PID=0002 LEVICULUM_BOARD_NAME=RAK4631 \
  LEVICULUM_UF2_BOARD_ID=WisBlock-RAK4631-Board \
  cargo run --release --bin rak4631 --features bsp-rak4631,rak-baseboard
```

(`Justfile:564-568`; `rak-baseboard` aggregate
`leviculum-nrf/Cargo.toml:144`)

### `just dfu-rak4631 PORT` — DFU entry for stock Meshtastic

Triggers the Adafruit UF2 bootloader on a stock-Meshtastic WisMesh
Pocket V2 in software. Stock Meshtastic has no 1200-bps-touch handler and
the device has no externally accessible RESET pin, so this firmware-side
admin command is the only software-only DFU entry. Needed **only** for
the first flash from Meshtastic; after our firmware lands,
`just flash-rak4631` uses the touch path and this recipe is no longer
needed. Requires the `meshtastic` CLI on PATH (`pip install meshtastic`).

```sh
just dfu-rak4631 /dev/ttyACM0
```

Runs:

```sh
meshtastic --port /dev/ttyACM0 --enter-dfu
```

(`Justfile:570-578`)

## A note on disconnecting consumers

Flashing a board takes over its transport serial port. Any running
consumer of that port (for example an active `lnsd` pointed at it) loses
its connection when the board is flashed. The flash action is explicit
and active; no persistence is promised across it.
(`leviculum-nrf/README.md:40`)

The device keeps its Reticulum identity in internal flash and preserves
it across firmware updates, so re-flashing does not change the node's
address. (`leviculum-nrf/README.md:42`. More in [Recovery](recovery.md).)

## Verifying the build before you flash

`cargo build --release` (above) confirms the image links and fits flash.
If you want to lint the firmware as CI does:

```sh
just lint-nrf
```

(Builds both BSP feature sets under clippy with `-D warnings`:
`Justfile:68-70`.)

Next: [Serial ports](serial-ports.md) for wiring the flashed board into
`lnsd`.
