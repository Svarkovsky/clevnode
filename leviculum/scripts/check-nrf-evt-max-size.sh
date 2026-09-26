#!/usr/bin/env bash
#
# BLE event-buffer gate for the nRF firmware.
#
# nrf-softdevice sizes the buffer it passes to `sd_ble_evt_get` at compile
# time from a feature (`evt-max-size-256` / `-512`), defaulting to 128 bytes
# when none is selected, and panics unconditionally when an event does not fit
# (nrf-softdevice/src/events.rs:92). Our GATT characteristics are 251 bytes
# wide, so a full-size write from a peer that negotiated a large ATT MTU is a
# 269-byte event: on the default 128 every LNode panicked and reset within
# seconds of a real Android client connecting (Codeberg #354).
#
# Nothing else in the suite catches losing that feature again. It is not
# visible to the firmware's own `cfg`s (a dependency's features are not
# `cfg(feature = ..)` in the dependent), the LNode-to-LNode bench negotiates a
# small MTU and stays under 128, and a firmware missing the feature builds and
# links cleanly. So the assertion is made against cargo's resolved feature
# graph — the same resolution the build uses, not the manifest text.
#
# Usage: check-nrf-evt-max-size.sh [feature]

set -euo pipefail

WANT="${1:-evt-max-size-512}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NRF="$ROOT/leviculum-nrf"

# Every feature set that produces a shipped image, per board family. The
# baseboard aggregate is listed separately from the bare RAK4631 build because
# they are two different images and the gate is about what gets flashed.
BUILDS=(
    "bsp-t114"
    "bsp-rak4631"
    "bsp-rak4631,rak-baseboard"
)

rc=0
for feats in "${BUILDS[@]}"; do
    tree=""
    if ! tree="$(cd "$NRF" && cargo tree --frozen --features "$feats" -e features \
        -i nrf-softdevice 2>&1)"; then
        echo "[evt-max-size] FAIL $feats: cargo tree failed"
        printf '%s\n' "$tree" | sed 's/^/  /'
        rc=1
        continue
    fi

    if printf '%s\n' "$tree" | grep -qF "nrf-softdevice feature \"$WANT\""; then
        echo "[evt-max-size] ok   $feats: nrf-softdevice/$WANT is enabled"
    else
        echo "[evt-max-size] FAIL $feats: nrf-softdevice/$WANT is NOT enabled."
        echo "[evt-max-size] The image would run on a 128-byte BLE event buffer"
        echo "[evt-max-size] and panic on the first full-size write from a phone"
        echo "[evt-max-size] (Codeberg #354). Restore the feature in"
        echo "[evt-max-size] leviculum-nrf/Cargo.toml, in the \`softdevice\` list."
        rc=1
    fi
done

exit "$rc"
