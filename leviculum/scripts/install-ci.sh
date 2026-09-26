#!/bin/bash
# Idempotent CI installer for the Leviculum 4-tier self-hosted pipeline.
set -e

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

# --vm-mode skips the developer-machine bits (git hooks + their
# accompanying chmods).  The VM never commits or pushes, so nothing
# needs the hooks to be live.  A worktree-scoped marker file is
# written so the tier-runners know to perform a `git fetch + checkout
# --force origin/master` at the head of every scheduled run.
VM_MODE=0
for arg in "$@"; do
    case "$arg" in
        --vm-mode) VM_MODE=1 ;;
        *)
            echo "ERROR: unknown flag '$arg'" >&2
            echo "Usage: $0 [--vm-mode]" >&2
            exit 1
            ;;
    esac
done

echo "[install-ci] Installing CI pipeline in $REPO_DIR (vm-mode=$VM_MODE)"

# 1. Dependency check
MISSING=()
# A hard dependency, not an optional one: `just fast` runs `just
# nrf-shellcheck` over the flash-runner scripts (Codeberg #345), and a gate
# that silently does not run is worse than no gate. (Written this way round
# because a comment opening with the tool's own name parses as a directive.)
for cmd in just docker notify-send cargo python3 flock socat shellcheck; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        MISSING+=("$cmd")
    fi
done
if [ ${#MISSING[@]} -gt 0 ]; then
    echo "[install-ci] Missing dependencies: ${MISSING[*]}"
    echo "[install-ci] Hint: sudo apt install ${MISSING[*]}"
    exit 1
fi

# Optional test dependency: i2pd provides the SAM bridge (127.0.0.1:7656) the
# I2PInterface live tests need. The default suite covers I2PInterface with an
# in-process mock SAM bridge, so i2pd is not required to go green; it only gates
# the `#[ignore]`d live tests in leviculum-std (interfaces::i2p::i2pd_live). Warn
# rather than fail when it is absent.
for cmd in i2pd; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "[install-ci] Note: optional test dependency '$cmd' not found"
        echo "[install-ci] Hint: sudo apt install $cmd (needed only for the ignored I2P live tests)"
    fi
done

# Optional test dependency: nomadnet drives the on-demand lnomad acceptance
# (scripts/lnomad_nomadnet_acceptance.sh). Not part of any tier, so warn rather
# than fail when it is absent.
for cmd in nomadnet; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "[install-ci] Note: optional test dependency '$cmd' not found"
        echo "[install-ci] Hint: pip install nomadnet (needed only for the lnomad acceptance)"
    fi
done

# Optional rig dependency: uhubctl cuts and restores power on a single USB
# hub port, which is how a board that stopped enumerating gets recovered
# without touching it (scripts/install-usbhub-helper.sh wires the
# passwordless sudo the runner needs; scripts/run-tier3-hw.sh uses it between
# profiles). Only the hardware host has a hub to drive, so warn rather than
# fail — a VM runner has nothing to power-cycle.
if ! command -v uhubctl >/dev/null 2>&1; then
    echo "[install-ci] Note: optional rig dependency 'uhubctl' not found"
    echo "[install-ci] Hint: sudo apt install uhubctl"
    echo "[install-ci]       (needed only to power-cycle a hung board's hub port)"
fi

# Optional test dependency: cargo-fuzz (+ nightly) drives the wire-format
# parser fuzz harness under leviculum-core/fuzz (Codeberg #23). Not part of
# `just standard` — the regression tests for any crash it finds live in the
# normal unit suite — so warn rather than fail when it is absent.
if ! command -v cargo-fuzz >/dev/null 2>&1; then
    echo "[install-ci] Note: optional test dependency 'cargo-fuzz' not found"
    echo "[install-ci] Hint: cargo install cargo-fuzz && rustup toolchain install nightly"
    echo "[install-ci]       (needed only for the leviculum-core/fuzz targets; see docs/src/development-testing.md)"
fi

# Optional test dependency: btvirt hosts the periculum `ble_room_*` cells
# (periculum #49): N virtual LE controllers on one emulated air, so N lnsd
# daemons can prove BLE mesh formation with no boards. Debian does not
# package it; it is built from the bluez source tree MATCHING the installed
# bluez (mismatched daemon/emulator versions are their own bug class):
#
#   apt-get source bluez            # needs a deb-src line; 5.82 as of 2026-09
#   sudo apt install libreadline-dev python3-docutils
#   cd bluez-*/ && ./configure --enable-testing --disable-systemd \
#       --disable-cups --disable-obex --disable-hid2hci --disable-mesh \
#       --disable-udev
#   make -j"$(nproc)" emulator/btvirt
#   sudo install -m 755 emulator/btvirt /usr/local/bin/btvirt
#
# Prerequisites the cells need beyond the binary (one-time provisioning):
#   - kernel hci_vhci module, loaded at boot and group-writable:
#       echo hci_vhci | sudo tee /etc/modules-load.d/hci_vhci.conf
#       echo 'KERNEL=="vhci", GROUP="bluetooth", MODE="0660"' | \
#         sudo tee /etc/udev/rules.d/60-vhci-bluetooth.rules
#     (the executing user must be in the bluetooth group)
#   - the SYSTEM bluetoothd running: it must own the vhci adapters the
#     moment btvirt creates them — lnsd reaches controllers only through
#     BlueZ, and the cells do not start their own daemon
#   - NO BLE MIDI GATT service on the bench, or agentless SMP pairing
#     kills every room connection before the Columba handshake
#     (root-caused 2026-09-09). Two registrars to silence:
#       * bluetoothd's midi plugin: systemd drop-in overriding ExecStart
#         with `/usr/libexec/bluetooth/bluetoothd --noplugin=midi`
#       * WirePlumber's bluez monitors, SYSTEM-WIDE (the GDM session runs
#         its own instance): /etc/wireplumber/wireplumber.conf.d/ snippet
#         with `monitor.bluez = disabled` and `monitor.bluez-midi =
#         disabled` in the main profile
#   - btmon usable by the executing user, for the room's optional HCI
#     capture (PERICULUM_BLE_ROOM_BTMON): the monitor channel needs
#     CAP_NET_RAW, granted once with
#       sudo setcap cap_net_raw+ep /usr/bin/btmon
# Warn-only: only the bench that runs the BLE room needs any of it.
if ! command -v btvirt >/dev/null 2>&1 && [ ! -x /usr/local/bin/btvirt ]; then
    echo "[install-ci] Note: optional test dependency 'btvirt' not found"
    echo "[install-ci] Hint: build from the matching bluez source tree (see comment above"
    echo "[install-ci]       this check); needed only for the periculum ble_room cells"
fi

# 2. Activate git hooks (developer-machine mode only)
if [[ "$VM_MODE" -eq 0 ]]; then
    git config core.hooksPath .githooks
    echo "[install-ci] git core.hooksPath -> .githooks"
else
    echo "[install-ci] --vm-mode: skipping git core.hooksPath config"
fi

# 3. chmod hook + runner scripts
if [[ "$VM_MODE" -eq 0 ]]; then
    # commit-msg is the machine-authorship trailer guard (Codeberg #205). It
    # is convenience only — the enforcement is .woodpecker/commit-trailers.yml,
    # because a fresh clone has none of these hooks.
    #
    # These two are the whole set. A post-commit hook detached
    # `scripts/run-tier1.sh` — a 15-40 min background docker run — after every
    # commit until 2026-08-07; the rule that removed it is in
    # docs/src/concepts/checks-and-citations.md ("What may live in a git hook").
    chmod +x .githooks/pre-push .githooks/commit-msg
fi
chmod +x scripts/run-tier1.sh scripts/run-tier2.sh scripts/run-tier3.sh scripts/run-tier3-hw.sh
chmod +x scripts/flash-lnodes-from-head.sh
chmod +x scripts/ci-status.sh
chmod +x scripts/check-submodule-pins.sh scripts/check-commit-trailers.sh
chmod +x scripts/install-ci.sh
echo "[install-ci] runner scripts made executable"

# 4. State directory
mkdir -p ~/.local/state/leviculum-ci
echo "[install-ci] state dir: ~/.local/state/leviculum-ci"

# 5. Separate cargo target dir
mkdir -p ~/.cache/leviculum-ci-target
echo "[install-ci] cargo target dir: ~/.cache/leviculum-ci-target"

# 6. Firmware build toolchain.  flip-link is the firmware linker
#    (stack-overflow protection, Codeberg #50); run-tier3-hw.sh builds
#    the firmware via `just flash*`.  Both lines are idempotent: the
#    rustup target is a no-op once added, and `cargo install` skips a
#    crate that is already present at the requested version.
rustup target add thumbv7em-none-eabihf
cargo install --locked flip-link
#    llvm-tools ships llvm-objdump, which `just nrf-stack-frames` uses to
#    read the frame-allocating `sub sp` immediates out of the linked ELF
#    when binutils-arm-none-eabi is absent.
rustup component add llvm-tools
echo "[install-ci] firmware toolchain: thumbv7em-none-eabihf + flip-link + llvm-tools"

# 6b. Third-party licence notices (Codeberg #288). `just notices-guard`
#     runs in Tier 0, so this is a hard dependency of the push path, not
#     an optional extra — hence an install rather than the warn-only
#     treatment the test-only tools above get.
#
#     Pinned: the generated file is checked in and diffed byte for byte,
#     so a cargo-about that formats or classifies anything differently
#     turns the gate red on every machine that has the other version.
#     Bumping the pin is a deliberate act with a `just notices` commit
#     next to it.
#
#     `--features cli` is not optional: without it the crate builds as a
#     library and cargo installs no binary at all, reporting only a
#     warning.
cargo install --locked cargo-about --version 0.9.2 --features cli
echo "[install-ci] licence tooling: cargo-about 0.9.2 (just notices / just notices-guard)"

# 7. Install systemd user units, patching the hardcoded
#    %h/coding/libreticulum literal to point at the worktree this
#    installer was actually run from.  Lets a `git worktree`-based
#    second checkout (e.g. ~/coding/libreticulum-ci) install its
#    own units that fire against itself, instead of silently
#    targeting the developer's primary checkout.
SYSTEMD_USER_DIR=~/.config/systemd/user
mkdir -p "$SYSTEMD_USER_DIR"
for unit in scripts/systemd/leviculum-ci-tier2.service \
            scripts/systemd/leviculum-ci-nightly.service \
            scripts/systemd/leviculum-ci-nightly.timer; do
    sed "s|%h/coding/libreticulum|$REPO_DIR|g" "$unit" \
      > "$SYSTEMD_USER_DIR/$(basename "$unit")"
done
echo "[install-ci] systemd user units installed in $SYSTEMD_USER_DIR (path: $REPO_DIR)"

# 8. Reload systemd
systemctl --user daemon-reload

# 9. Enable timers.  Tier 2 is ON-DEMAND (Lew, 2026-06-12): only the
#    nightly stays scheduled.  Start tier2 manually when needed:
#      systemctl --user start leviculum-ci-tier2.service
#    Upgrade path: drop a previously-installed tier2 timer.
systemctl --user disable --now leviculum-ci-tier2.timer 2>/dev/null || true
rm -f "$SYSTEMD_USER_DIR/leviculum-ci-tier2.timer"
systemctl --user enable --now leviculum-ci-nightly.timer
echo "[install-ci] nightly timer enabled; tier2 is on-demand"

# 10. LoRa hardware probe (warning only)
if ! ls /dev/ttyACM* >/dev/null 2>&1; then
    echo "[install-ci] WARNING: no /dev/ttyACM* devices found — LoRa tests will skip in nightly."
fi

# 11. Worktree-scoped vm-mode marker.  Tier-runners check for this
#     file inside their git-dir before running _repo-sync.sh.  Marker
#     is per-worktree (not per-user) so a manual `bash run-tier2.sh`
#     from the developer's primary checkout never triggers a
#     destructive `git checkout --force` against the wrong tree.
if [[ "$VM_MODE" -eq 1 ]]; then
    GIT_DIR=$(git rev-parse --git-dir)
    touch "$GIT_DIR/leviculum-ci-vm-mode-marker"
    echo "[install-ci] vm-mode marker: $GIT_DIR/leviculum-ci-vm-mode-marker"
fi

# Summary
echo ""
echo "[install-ci] Installation complete."
echo ""
echo "  Run manually:    just fast | just standard | just extensive | just nightly"
echo "  Show status:     just status"
echo "  Logs:            ~/.local/state/leviculum-ci/"
echo "  Timers:          systemctl --user list-timers"
echo ""
echo "  Nothing starts Tier 1 for you. The pre-push hook runs Tier 0 only;"
echo "  Tier 1 is 'just standard', typed once per batch. A post-commit hook"
echo "  used to detach it after every commit — removed 2026-08-07."
echo "  A cold Tier 1 compiles the whole workspace: plan for 20-40 min."
