# leviculum-nrf

## LNode firmware
Firmware for the Heltec Mesh Node T114 (nRF52840 + SX1262) and the Wismesh Pocket V2. It turns these devices into a standalone Reticulum transport nodes (LNodes) that routes packets between USB serial, LoRa radio, and BLE. The transport engine is the same `leviculum-core` library that powers the Linux daemon, compiled for Cortex-M4F.

The firmware speaks the RNode LoRa framing protocol, so T114 and RNode devices interoperate on the same LoRa network. On the BLE side, it implements the Columba v2.2 protocol and can be used with the Columba Android app. On the host side, it connects to `lnsd` or `rnsd` over USB serial with HDLC framing.

The default radio profile uses 869.463 MHz (the ReticulumNet consensus channel in the EU ISM band), SF8, 125 kHz bandwidth, and 22 dBm TX power. These parameters are compiled into the firmware and must match the RNode configuration on the same network.

## Prerequisites

Install the Rust embedded toolchain, the ARM cross-compiler (needed by `nrf-sdc` for C header bindgen), and add your user to the `dialout` group for serial port access (log out and back in afterwards).

```
rustup target add thumbv7em-none-eabihf
rustup component add llvm-tools
sudo apt install gcc-arm-none-eabi
sudo usermod -aG dialout $USER
```

## Build and flash

Build the firmware with `cargo build --release`. Flash with `just flash` (from the repo root), which wraps `cargo run --release --bin t114`.

`just flash` flashes **every attached T114** sequentially. This is deliberate: if only one were flashed, developers would forget to update the other(s) and later run multi-node tests against mixed firmware versions, producing misleading results. The runner walks all matching devices in turn, copies the UF2, and prints a per-device summary at the end.

Since the touch-free flashing change (`git log --grep='Bug #13'`), flashing is touch-free in the common case: the host opens each T114's transport CDC port at 1200 baud, the firmware intercepts the line-coding change, writes a retained-register magic, and soft-resets into the Adafruit UF2 bootloader. No physical button press required.

**Selective flashing** (e.g. for A/B firmware testing, one T114 on a new build and another on the old):

```
just flash-one /dev/leviculum-transport
just flash-one /dev/ttyACM3
```

Both forms expand to `LEVICULUM_FLASH_ONLY=<port> cargo run --release --bin t114`.

**When you still need a physical double-tap:** the firmware on a specific T114 has crashed or never reached USB init (panic before the handler is installed, stack overflow, hardware fault). The runner detects this per device via the UF2-drive-polling timeout and prompts for that specific T114 only. Other T114s in the batch continue to flash touch-free.

**Caveat:** any running consumer of a T114's transport serial port (e.g. an active `lnsd`) loses its connection when that T114 is flashed. The flash action is active and explicit; no persistence is promised across it.

The device stores its Reticulum identity in internal flash and preserves it across firmware updates.

## USB serial ports

The firmware exposes two USB CDC-ACM serial ports. The lower-numbered port is the debug log output. The higher-numbered port is the Reticulum transport interface that carries HDLC frames. The actual `/dev/ttyACM*` numbers depend on other connected USB devices.

To get stable device paths, install the udev rules:

```
sudo cp udev/99-leviculum.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
```

After the next plug-in, `/dev/leviculum-debug` and `/dev/leviculum-transport` point to the correct ports regardless of enumeration order.

## Reading debug output

```
picocom /dev/leviculum-debug -b 115200
```

## Build profiles

The debug profile does not fit the nRF52840 flash (the image overflows
FLASH by several hundred KB at link time) — always build and flash with
`--release`; all `just flash-*` recipes already do.
