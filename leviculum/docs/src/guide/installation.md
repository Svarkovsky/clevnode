# Installation

## Requirements

- Rust stable toolchain
- Git

Optional, depending on what you want to test:

- Python 3 (for interop tests)
- Docker (for integration tests)
- 2-4 RNode modems via USB (for LoRa integration tests)

No system C libraries are required. All cryptography is compiled from Rust source.

### Debian/Ubuntu setup

```sh
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Interop tests
sudo apt install python3

# Integration tests
sudo apt install docker.io
sudo usermod -aG docker $USER

# LoRa tests and embedded firmware (USB serial access)
sudo usermod -aG dialout $USER
```

## Build from source

```sh
git clone https://codeberg.org/Lew_Palm/leviculum.git
cd leviculum
cargo build --release --bin lnsd --bin lnstest --bin lncp
```

The workspace pins `x86_64-unknown-linux-musl` as its build target (see the
comments in `.cargo/config.toml` for why), so the binaries are in
`target/x86_64-unknown-linux-musl/release/`, not `target/release/`.

Verify the build; the output carries the version and the build commit:

```sh
./target/x86_64-unknown-linux-musl/release/lnsd --version
```

## Running the daemon

```sh
./target/x86_64-unknown-linux-musl/release/lnsd
```

The daemon resolves its config directory in the same order as Python
Reticulum (`configdir`, `RNS/Reticulum.py:231`), implemented in
`default_config_dir` (`leviculum-std/src/config.rs:773`):

1. `/etc/reticulum` — if `/etc/reticulum/config` exists. This is what
   the `.deb` package sets up.
2. `~/.config/reticulum` — if that directory's `config` exists.
3. `~/.reticulum` — the fallback, and where a source build with no
   prior config ends up.

Add `-v` (debug) or `-vv` (trace) for more verbose logging.

## Development

### Cargo aliases

Common workflows are available as cargo aliases (defined in `.cargo/config.toml`):

| Command | What it does |
|---------|-------------|
| `cargo test-core` | Run all leviculum-core unit tests |
| `cargo test-std` | Run all leviculum-std unit tests |
| `cargo test-interop` | Run interop tests against Python Reticulum |
| `cargo lint` | Run clippy on all crates |
| `cargo fmt --all -- --check` | Check formatting |

### Test levels

Tests are organized by what they require:

**Unit tests** -- just Rust, no extra dependencies:

```sh
cargo test-core
cargo test-std
```

**Interop tests** -- require Python 3 and the vendored Reticulum:

```sh
git submodule update --init reference/Reticulum
cargo test-interop
```

**Scenario tests** -- multi-node scenarios live in the sibling
[periculum](https://codeberg.org/Lew_Palm/periculum) checkout, expected at
`../periculum`. They require Docker and pre-built release binaries:

```sh
cargo build --release --bin lnsd --bin lnstest --bin lncp --bin lora-proxy
periculum run ../periculum/conformance ../periculum/regression
```

**LoRa integration tests** -- require physical RNode modems connected via USB:

LoRa scenarios live in periculum's `hardware/` corpus. They exercise real
over-the-air transfers between RNode radios running Reticulum firmware, and
between those and LNodes running leviculum's own firmware. A scenario names
the set of boards it needs (`profile = "rnode_pair"`, `"rnode_quad"`,
`"rnode_lnode_pair"`, ...), which is resolved against the bench description
in periculum's `rig.toml`. A scenario the bench cannot serve reports
`SKIPPED_INFRA` naming what was missing — never a failure. The per-profile
scenario counts are in periculum's `hardware/README.md`.

Hardware setup:

- Connect RNodes via USB. They appear as `/dev/ttyACM0`, `/dev/ttyACM1`, etc.
- Your user must be in the `dialout` group: `sudo usermod -aG dialout $USER`
- Override device paths with environment variables if needed:
  `LEVICULUM_RNODE_0=/dev/ttyUSB0 LEVICULUM_RNODE_1=/dev/ttyUSB1`

Running LoRa tests:

```sh
# See what the corpus holds, and what this bench can serve
periculum list ../periculum/hardware
periculum devices --probe

# Single scenario
periculum run ../periculum/hardware/lora_link_rust.toml

# The whole hardware corpus
periculum run ../periculum/hardware

# Override radio parameters (bandwidth in Hz)
LORA_BANDWIDTH=125000 periculum run ../periculum/hardware/lora_lncp_push.toml
```

Each LoRa test must pass on all three bandwidth profiles (62.5 kHz, 125 kHz,
250 kHz). The TOML files define 62.5 kHz; use `LORA_BANDWIDTH` to switch.

Some tests use the `lora-proxy` binary for fault injection (dropping frames
to test retransmit recovery). Build it before running proxy tests:

```sh
cargo build --release --bin lora-proxy
```

### Embedded cross-compilation

Embedded targets are not downloaded automatically. Install them when needed:

```sh
rustup target add thumbv7em-none-eabihf   # nRF52840
rustup target add thumbv6m-none-eabi       # RP2040
cargo check-nrf52
cargo check-embedded
```

### Before submitting changes

```sh
cargo fmt --all -- --check
cargo lint
cargo test-core
cargo test-interop
```
