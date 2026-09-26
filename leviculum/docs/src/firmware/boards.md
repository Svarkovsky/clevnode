# LNode Firmware: Supported Boards

The LNode firmware turns an nRF52840-based board into a standalone
Reticulum transport node. It runs the same `leviculum-core` transport
engine that powers the Linux daemon, cross-compiled for Cortex-M4F, and
routes packets between three interfaces: USB serial (HDLC framing to a
host), the SX1262 LoRa radio, and BLE. There is no PC in the data path;
the device is a router in its own right.

> The transport engine is the same `leviculum-core` library that powers
> the Linux daemon, compiled for Cortex-M4F.
> (`leviculum-nrf/README.md:4`)

On the wire the firmware speaks the RNode LoRa framing protocol, so an
LNode and an RNode interoperate on the same LoRa network. On the host
side it connects to `lnsd` or `rnsd` over USB serial with HDLC framing.
On the BLE side it implements the Columba v2.2 protocol for the Columba
Android app. (`leviculum-nrf/README.md:6`,
`leviculum-nrf/src/bin/t114.rs:3-8`)

## What the firmware does

Each firmware binary registers exactly three Reticulum interfaces and
runs an event-driven main loop that dispatches packets between them:

| Interface | ID | Medium | HW MTU |
|-----------|----|--------|--------|
| `serial_usb` | 0 | USB CDC-ACM, HDLC framing to host | 564 |
| `lora_sx1262` | 1 | SX1262 LoRa radio | 255 |
| `ble` | 2 | BLE peripheral, Columba v2.2 | 564 |

(Interface registration and MTUs:
`set_interface_name` (`leviculum-nrf/src/bin/t114.rs:189-197`) and
`leviculum-nrf/src/bin/rak4631.rs:230-238`. The main loop selecting over
the three RX sources plus a timer deadline begins at
`leviculum-nrf/src/bin/t114.rs:426`.)

Transport routing is enabled in the node builder, so an LNode forwards
packets and serves paths for other peers, exactly like a
transport-enabled `lnsd`.
(`enable_transport` (`leviculum-nrf/src/bin/t114.rs:166`),
`leviculum-nrf/src/bin/rak4631.rs:201`)

## Hardware coverage

We build one firmware per **pinout family**, not per product. A family is
a set of boards whose SX1262 wiring is identical, which happens whenever
the radio ships together with the MCU as one module: every carrier board
built around that module then inherits the same wiring. One build
therefore covers many products, and we only add a build when a board's
radio wiring genuinely differs.

The same principle applies inside a family. Peripherals that a carrier
board adds are detected at run time or degrade to nothing, so a single
image serves the bare module and the fully populated product alike.

The policy behind this page, including when a specialised build is
justified, is
[How far one firmware build reaches](../concepts/board-support-scope.md);
how a board is identified before anything is written is
[Flashing an LNode](../concepts/lnode-flashing.md).

### How to read the tables

| Level | Means |
|---|---|
| **Verified** | We own this board and run it. Failures here are bugs we must fix. |
| **Expected** | Radio wiring checked against the vendor reference and identical to a verified board of the same family. Never run by us. Report results. |
| **Not covered** | Different radio wiring. Our image will not drive the radio; do not flash it. |

**Expected is not a support promise.** It means the one thing that
decides whether the radio comes up at all, the SX1262 wiring, matches.
Everything a carrier adds beyond that, displays, GNSS, Ethernet,
accelerometers, e-paper, is not driven by our firmware on these boards
even where the vendor firmware drives it. The node routes packets; the
extra hardware stays dark.

### Family A: RAK4630 module

`rak4631` binary. Radio pins are internal to the RAK4630 module and
therefore identical across every carrier: NSS `P1.10`, SCK `P1.11`,
MOSI `P1.12`, MISO `P1.13`, BUSY `P1.14`, DIO1 `P1.15`, NRESET `P1.06`,
power enable `P1.05`. DIO2 drives the antenna switch; there is no
external TX or RX enable line on any of them.

| Product | Level | Note |
|---|---|---|
| RAK WisMesh Pocket V2 (RAK19026 carrier) | **Verified** | Display, GNSS and battery supported |
| RAK4631 bare module | **Verified** | Same image, peripherals absent |
| RAK WisMesh Pocket Mini (RAK19003) | Expected | |
| RAK WisMesh Repeater / Hub (RAK2560) | Expected | Solar, IP67 |
| RAK WisMesh Tap | Expected | TFT not driven |
| RAK WisMesh Tag | Expected | |
| WisBlock with RAK13800 Ethernet | Expected | Ethernet not driven |
| WisBlock with RAK14000 e-paper | Expected | E-paper not driven |
| NomadStar Meteor Pro | Expected | |
| MonteOps HW1 | Expected | |
| GAT562 Mesh Trial Tracker | Expected | |
| MeshTiny | Expected | |
| muzi R1 Neo | Expected | |

Verified against Meshtastic's own variant definitions under
`variants/nrf52840/`, where all twelve carriers repeat the same seven pin
numbers, and against the RAK4630 datasheet quoted in
`variants/nrf52840/rak4631/variant.h`.

The non-radio pins this build drives were checked the same way, and they
hold up: the two LEDs on `P1.03` and `P1.04` are the module's own, and
`P0.13` / `P0.14` carry I2C and `P0.15` / `P0.16` the first serial port on
every carrier that defines them at all. That is not luck. The RAK4630
brings these signals out on fixed module pins and the WisBlock carriers
follow that convention, so a module-defined family stays coherent beyond
the radio. No carrier was found driving an output into a pin this build
also drives.

The one carrier worth naming is the e-paper-on-RX/TX variant, which puts
the display's SPI where the others put I2C and the serial port. Nothing
there fights our outputs, but the pins carry traffic that means nothing
to that hardware.

> **Do not confuse RAK4631 with RAK3401.** Meshtastic's `rak3401_1watt`
> variant declares the same PlatformIO board name, but it is a different
> radio module with different pins, its own SPI bus and a 1 W power
> amplifier. Our image would drive the wrong pins on it. This is why
> identification uses the bootloader's `Board-ID`, never a board name
> that vendor trees reuse.

### Family B: Heltec T114

`t114` binary. NSS `P0.24`, SCK `P0.19`, MOSI `P0.22`, MISO `P0.23`,
BUSY `P0.17`, DIO1 `P0.20`, NRESET `P0.25`, TCXO at 1.8 V via DIO3, DIO2
as antenna switch.

| Product | Level | Note |
|---|---|---|
| Heltec Mesh Node T114 | **Verified** | Status display and GNSS (L76K) supported |
| Heltec MeshSolar | **Blocked** | Radio matches, but our status LED sits on the battery controller's emergency-shutdown pin |
| LILYGO T-Echo | **Do not flash** | Two pin conflicts, see below |
| LILYGO T-Echo Plus | **Do not flash** | Same as T-Echo |

> **Matching radio pins are not sufficient, and this family is where that
> becomes concrete.** The `bsp-t114` build drives an ST7789 panel blind,
> because the panel cannot be detected, plus an LED and a GPS UART. Those
> pins are as much part of the image as the radio pins, and on a related
> board they land on whatever that board put there.
>
> On **LILYGO T-Echo** the collisions are severe. Our TFT power-enable
> output `P0.03` meets `PIN_EINK_BUSY`, which is an *output* of the
> e-paper controller, and our TFT clock `P1.08` meets `GPS_TX_PIN`, an
> output of the GPS receiver. Both are two drivers on one line. Our TFT
> data line `P0.12` meets `PIN_POWER_EN`, so the display driver would
> switch the board's peripheral power on and off as a side effect of
> drawing. This is a hardware hazard, not a board that merely fails to
> transmit.
>
> On **Heltec MeshSolar** the radio wiring, the LoRa SPI bus and even the
> GPS UART line up exactly, and none of our TFT pins is occupied. One pin
> spoils it: our status LED `P1.03` is that board's
> `BQ4050_EMERGENCY_SHUTDOWN_PIN`. Blinking a heartbeat onto the battery
> controller's shutdown input is not acceptable, so this stays blocked
> until the LED becomes a board fact that can be left unset.

**Method note.** Membership in a pinout family is a necessary condition,
never a sufficient one. Before any board moves to *Expected*, every pin
the image drives has to be checked against that board's own definition,
not only the seven radio pins. The three entries above passed the radio
check and failed this one.

Unlike family A, this family is not one module: these are separate boards
that happen to share a wiring convention, so a new Heltec or LILYGO model
is not covered by default. Heltec Mesh Pocket, Heltec T1, Heltec T096 and
LILYGO T-Echo Lite each wire the radio differently and are **not
covered** either.

**So today this build serves exactly one product, the T114.** Sharing a
radio pinout turned out to be the easy half.

> **The `Board-ID` does not separate this family from its neighbours, and
> that is a hazard rather than an inconvenience.** Meshtastic records the
> same bootloader product string `HT-n5262` for the T114, for MeshSolar
> and for the Heltec Mesh Pocket, whose radio is wired differently and
> which is not covered here. Both our tools match that string exactly
> (`board_for_id` (`lnflash/src/manifest.rs:394`),
> `leviculum-nrf/tools/uf2-runner.sh:79`), so if the `INFO_UF2.TXT`
> `Board-ID` is identical too, neither can tell a Mesh Pocket from a
> T114. We cannot check that without the hardware. Until someone does,
> treat a `HT-n5262` match as a family hint and confirm the model by
> other means before writing.
>
> This is the general rule behind both this warning and the XIAO case
> below: **a `Board-ID` is only a safe key when it is bound to the same
> unit as the radio wiring.** On the RAK4630 both belong to the module,
> so the key is exact. Heltec binds the identifier to a bootloader shared
> across models while the wiring belongs to the model, and Seeed binds it
> to the MCU module while the radio sits outside it. Both of those
> decouple, and a decoupled key cannot carry a write decision alone.

LILYGO T-Echo and T-Echo Plus are the harmless side of the same coin:
they report a different `Board-ID` (`TTGO_eink` by Meshtastic's record),
so our tools decline them today. The firmware would run; the tooling
needs the identifier before it can.

> **Elecrow ThinkNode M1 is not covered**, although its seven radio pins
> match. It runs its TCXO at 3.3 V where this family uses 1.8 V, and our
> build compiles 1.8 V in. Supporting it needs that value to become a
> board fact rather than a family fact.

### Not covered today

Each of these is a separate pinout family, reachable by adding one board
file rather than by changing shared code:

| Family | Products |
|---|---|
| XIAO nRF52840 + Wio-SX1262 | Seeed SenseCAP Solar Node, Wio Tracker L1, XIAO kits |
| ThinkNode M6 | Elecrow ThinkNode M6, muzi BASE |
| ProMicro + E22 | nRF52 ProMicro DIY, DLS Minimesh Lite |
| Individual wirings | Heltec Mesh Pocket, B&Q Nano G2 Ultra, LILYGO T-Echo Lite, Canary One, MS24SF1, MeshLink, TWC Mesh v4 |

The XIAO family is the one case where the bootloader cannot answer which
board it is: the MCU module is a XIAO and the radio is a separate part,
so a SenseCAP Solar Node and a DIY XIAO with different radio wiring both
report `nRF52840-SeeedXiao-v1`. Boards like that need a second
discriminator before anything may be written.

### Known open question

Our RAK build sets the SX1262 TCXO to 3.3 V, following the RNode
firmware, which selects `MODE_TCXO_3_3V_6X` for this board
(`leviculum-nrf/src/boards/rak4631.rs:39-43`). Meshtastic and MeshCore
both run the same module at 1.8 V. The value lives in the module, so it
applies to every carrier in family A equally. Our Pocket V2 works with
3.3 V, but the divergence against two references is unresolved and should
be settled before the family is presented as broadly supported.

## Cargo features and binaries

Two firmware binaries are defined, one per board family:

```text
[[bin]]
name = "t114"
path = "src/bin/t114.rs"

[[bin]]
name = "rak4631"
path = "src/bin/rak4631.rs"
```

(`leviculum-nrf/Cargo.toml:159-165`)

The board-support-package (BSP) features select the runtime for a given
board. Exactly one BSP feature must be enabled per build; a
`compile_error!` in `lib.rs` enforces the mutual exclusion.
(`leviculum-nrf/Cargo.toml:131-139`)

| Feature | Effect | Cite |
|---------|--------|------|
| `bsp-t114` | T114 BSP (+ SoftDevice BLE + status display + GNSS) | `leviculum-nrf/Cargo.toml:224` |
| `bsp-rak4631` | RAK4631 BSP (+ SoftDevice BLE) | `leviculum-nrf/Cargo.toml:212` |
| `display` | SSD1306 OLED, probed at run time | `leviculum-nrf/Cargo.toml:226` |
| `gnss` | NMEA0183 GNSS (ZOE-M8Q on the V2 baseboard, L76K on the T114) | `leviculum-nrf/Cargo.toml:227` |
| `battery` | battery telemetry on baseboard | `leviculum-nrf/Cargo.toml:233` |
| `rak-baseboard` | aggregate of `display` + `gnss` + `battery` | `leviculum-nrf/Cargo.toml:234` |

> **Note on BLE:** Both firmware entry points register a BLE interface
> and call `leviculum_nrf::ble::init`
> (`leviculum-nrf/src/bin/t114.rs:245`,
> `leviculum-nrf/src/bin/rak4631.rs:272`). The Cargo `softdevice`
> feature, and therefore the BLE stack, is pulled in by *both* BSP
> features (`leviculum-nrf/Cargo.toml:133`,
> `leviculum-nrf/Cargo.toml:139`).

The baseboard peripherals are each gated behind their own Cargo feature
(`leviculum-nrf/Cargo.toml:141-144`) and spawned only when that feature
is on (`leviculum-nrf/src/bin/rak4631.rs:308-334`). Because each of them
either probes for its hardware or degrades to nothing when it is absent,
the aggregate build is what we ship for the whole family rather than a
Pocket-V2-only image.

The mapping from board to binary and features used by the flash recipes:

| Board | Binary | Features |
|-------|--------|----------|
| Heltec Mesh Node T114 | `t114` | `bsp-t114` |
| RAK4631 (bare module) | `rak4631` | `bsp-rak4631` |
| WisMesh Pocket V2 (full baseboard) | `rak4631` | `bsp-rak4631,rak-baseboard` |

(Feature sets as invoked in the `just flash`, `just flash-rak4631`, and
`just flash-rak4631-pocket` recipes: `Justfile:719`, `Justfile:746`,
`Justfile:759`.)

### What the `lnflash` bundle carries

The distributable bundle carries an image for both families: `bsp-t114`
for the T114 and `bsp-rak4631,rak-baseboard` for the RAK4630 module
(Codeberg #261). The RAK row it ships is the last one in the table above,
not the middle one — the bare module runs the baseboard image, and the
paragraph above is why. There is deliberately no way for a user to choose
between them: the manifest cannot express two images for one `Board-ID`,
because a question nobody can answer from looking at their board is not a
question worth asking.

Which boards the bundle knows at all is `lnflash/catalogue.toml`, and it
is a shorter list than the tables above on purpose. A row here says our
image would drive that board's radio; a catalogue entry says the
bootloader can be told apart from every other board's, which is the
stricter of the two claims and the only one a write may rest on. See
[Building and flashing](flashing.md), "Which boards the bundle carries".

## Build target

All firmware builds target the hard-float Cortex-M4 triple:

```sh
thumbv7em-none-eabihf
```

(`leviculum-nrf/README.md:15`. Add it with `rustup target add
thumbv7em-none-eabihf`.)

## Default radio profile

The radio parameters are compiled into the firmware and must match the
RNode configuration on the same LoRa network.

| Parameter | Value |
|-----------|-------|
| Frequency | 869.463 MHz (ReticulumNet consensus, EU ISM band) |
| Spreading factor | SF8 |
| Bandwidth | 125 kHz |
| Coding rate | CR4/5 |
| TX power | 22 dBm |

(`leviculum-nrf/README.md:8`. The `eu_medium` profile the firmware loads
at boot: `leviculum-nrf/src/lora.rs:136-161`, applied at
`leviculum-nrf/src/bin/t114.rs:235` and
`leviculum-nrf/src/bin/rak4631.rs:264`.)

See [Flashing](flashing.md) for how to build and write these binaries to
a board, and [Recovery](recovery.md) for the bootloader-entry details.
