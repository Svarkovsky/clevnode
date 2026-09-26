# Leviculum

Leviculum is an independent reimplementation based on the [Reticulum protocol specification](docs/src/appendix/reticulum-specification.md).
It is wire-compatible with Reticulum and runs on Linux, macOS, and embedded devices.

## What is Reticulum?

Reticulum is a networking stack for building resilient, encrypted mesh networks over any transport medium. It works over LoRa radios, TCP, UDP, serial links, or anything that can carry bytes. Every node gets a cryptographic identity. Every connection is end-to-end encrypted. No servers, no accounts, no infrastructure required.

## What does Leviculum do?

Leviculum provides the same functionality as Python Reticulum but compiled to native code. The `lnsd` daemon is a drop-in replacement for `rnsd`, `lncp` replaces `rncp`, `lnstatus` replaces `rnstatus`, and `lnprobe` replaces `rnprobe`. Python CLI tools like `rnstatus`, `rnpath`, and `rnprobe` also work against a running `lnsd` without modification.

The protocol core (`leviculum-core`) compiles as `no_std` with only `alloc`, so it runs on microcontrollers. The same code powers the Linux daemon, a future Android app, and embedded firmware.

## Status

Leviculum is in active development. The protocol implementation is functionally complete: routing, path discovery, link establishment, encrypted channels, file transfer, forward secrecy ratchets, and LoRa radio support all work and are tested against Python Reticulum on real hardware. (One caveat: sending files larger than one megabyte as multiple segments is not implemented yet — Codeberg #27; receiving multi-segment transfers works.) It is not yet production-ready.

## Getting started

### Nightly Debian / Ubuntu package (recommended)

```sh
# amd64
wget https://codeberg.org/Lew_Palm/leviculum/releases/download/nightly/leviculum-nightly-amd64.deb
sudo apt install ./leviculum-nightly-amd64.deb

# arm64
wget https://codeberg.org/Lew_Palm/leviculum/releases/download/nightly/leviculum-nightly-arm64.deb
sudo apt install ./leviculum-nightly-arm64.deb
```

Sets up `lnsd` as a systemd service under a dedicated `leviculum` user, with its config directory at `/etc/reticulum`. The directory is mode 2775 (group-writable, setgid), so any user in the `leviculum` group shares it as a single source of truth — no per-user config or extra flags. To opt in:

```sh
sudo usermod -aG leviculum "$USER"
# log out and back in for the group to apply
```

That covers everything: the native Rust clients (`lnstatus`, `lnstest`, `lncp`, `lnprobe`) talk to lnsd via the shared-instance socket, and Python Reticulum tools (`rnstatus`, `rncp`, `rnpath`, `rnprobe`, Sideband, Nomadnet, …) auto-detect `/etc/reticulum` (per `RNS.Reticulum.__init__`'s standard lookup) and connect through the same socket. If you ever swap lnsd out for the Python `rnsd` daemon, the same configdir keeps working — `lnsd` and `rnsd` are wire- and config-compatible.

The binaries are statically linked against musl, so the package installs on Debian ≥ 9 and Ubuntu ≥ 16.04 (amd64 + arm64) regardless of host glibc. SHA-256 checksums are published alongside every `.deb` with the suffix `.sha256`. The exact build is embedded in the binaries — `lnsd --version` prints e.g. `0.7.1-nightly.20260419-5a5df20`.

### Applications

Two programs ship as their own nightly packages, so neither drags in the auto-started `lnsd` service that `leviculum` sets up. Both are versioned independently of the stack — their version numbers track the programs, not the protocol core.

```sh
# lnomad — Nomadnet terminal browser
sudo apt install ./lnomad-nightly-amd64.deb

# lblogd — dev-blog server (NomadNet page node + clearnet web)
sudo apt install ./lblogd-nightly-amd64.deb
```

Download them from the same [nightly release](https://codeberg.org/Lew_Palm/leviculum/releases/tag/nightly), as `.deb` or as a userspace `.tar.gz`, for amd64 and arm64. `lnomad` is a plain terminal program. `lblogd` installs and starts a systemd service that serves on `http://127.0.0.1:8180/` until you point it at a domain in `/etc/lblogd/config.toml`. Both need a running Reticulum instance for their mesh side — `lnsd` from the package above, or the Python `rnsd`.

### Build from source

Tested on current Debian (trixie). Install the toolchain once. Rust comes
from rustup because the Debian cargo is usually too old; everything else is
apt packages. No file outside these packages and the cloned repo is needed.

```sh
# Rust:
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"

# Build, test, and flash tooling:
sudo apt install just docker.io docker-compose build-essential pkg-config \
    python3 python3-venv python3-pip python3-serial python-is-python3 esptool

# Let the interop and integration tests run containers without sudo:
sudo usermod -aG docker "$USER"   # then log out and back in
```

Then clone and build:

```sh
git clone https://codeberg.org/Lew_Palm/leviculum.git
cd leviculum
git submodule update --init reference/Reticulum
cargo build --release --bin lnsd --bin lnstatus --bin lncp --bin lnstest --bin lnprobe
./target/x86_64-unknown-linux-musl/release/lnsd --version
```

The workspace pins `x86_64-unknown-linux-musl` as its build target (see
`.cargo/config.toml` for why), so the binaries land under
`target/x86_64-unknown-linux-musl/release/`, not `target/release/`. No
system C libraries are linked into the daemon. Run the test tiers:

```sh
cargo test-core      # unit tests
cargo test-interop   # against Python Reticulum (needs Docker and the submodule)
```

See the [installation guide](https://codeberg.org/Lew_Palm/leviculum/src/branch/master/docs/src/guide/installation.md) for all cargo aliases and test levels.

#### Flashing LoRa hardware (optional)

For the embedded LNode firmware (Heltec T114, RAK4631), install the embedded
toolchain once — the target itself, `flip-link` (the firmware's linker), and
`llvm-tools` (provides the `llvm-objcopy` the UF2 flasher uses) — then flash
attached devices over USB:

```sh
rustup target add thumbv7em-none-eabihf
rustup component add llvm-tools
cargo install --locked flip-link
just flash            # every attached T114
just flash-rak4631    # every attached RAK4631
```

For the RNode radios (LilyGO T-Beam, ESP32), extract Mark Qvist's signed
firmware off a known-good RNode once, then flash any T-Beam. The ESP32 cannot
be bricked, a failed flash is always recoverable by re-running flash-rnode:

```sh
just flash-rnode-extract /dev/ttyACM6   # once, from a trusted RNode
just flash-rnode /dev/ttyACM6
```

#### Cross-built .deb packages (optional)

Only for producing the static musl `.deb` artifacts via `just build-deb`:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo install cargo-zigbuild cargo-deb
pip install --user ziglang
```

## Bugs and contributions

Report bugs and ask questions at
<https://codeberg.org/Lew_Palm/leviculum/issues>. When the daemon is
involved, attach the output of `lnstest diag` (see the
[quickstart](docs/src/lnsd-quickstart.md) for the exact command). For
patches, see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE) for the full text.
