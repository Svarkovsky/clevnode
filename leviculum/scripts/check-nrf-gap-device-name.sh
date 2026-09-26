#!/usr/bin/env bash
#
# GAP device-name pointer gate for the nRF firmware.
#
# `ble_gap_cfg_device_name_t` is handed to `sd_ble_cfg_set` inside
# `Softdevice::enable`, and its contract on `p_value` depends on `vloc`
# (nrf-softdevice-s140 bindings, `ble_gap_cfg_device_name_t`):
#
#   If vloc is BLE_GATTS_VLOC_STACK:
#     - p_value must point to non-volatile memory (flash) or be NULL.
#     - If p_value is NULL, the device name will initially be empty.
#   If vloc is BLE_GATTS_VLOC_USER:
#     - p_value cannot be NULL.
#
# Breaking the STACK rule is not a cosmetic mistake. `sd_ble_cfg_set`
# answers a bad pointer with NRF_ERROR_INVALID_ADDR, nrf-softdevice's
# `cfg_set` panics on every error but NoMem, and that panic lands inside
# `Softdevice::enable` — which our binaries call from `main` ahead of its
# first await, so the spawned USB task is never polled. The board dies
# before enumeration and boot-loops, with no serial port to say why.
# `e52dba1` moved the name from a `b"leviculum"` flash literal to a
# runtime-built `StaticCell` buffer in `.bss` and did exactly that.
#
# Nothing else in the suite catches it: the name-building function is pure
# and its host tests stay green, both BSPs build and link, clippy is happy,
# and the only observable is a board that never enumerates. Hence a source
# gate. It is a text check by necessity — the offending value is a runtime
# address, so no ELF inspection can decide it — and therefore it asserts
# the shape the contract allows rather than the shape it forbids.
#
# Usage:
#   check-nrf-gap-device-name.sh              # gate the firmware sources
#   check-nrf-gap-device-name.sh --self-test  # positive control, no sources

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

python3 - "$ROOT" "${1:-}" <<'PY'
import re
import sys
from pathlib import Path

TAG = "[gap-device-name]"

# `p_value` forms the SoftDevice accepts under BLE_GATTS_VLOC_STACK: a null
# pointer, or a pointer derived from a byte-string literal, which rustc puts
# in .rodata and the linker puts in flash.
NULL_PTR = re.compile(r"^(?:core::)?ptr::null_mut(?:::<[^>]*>)?\(\)$")
FLASH_LITERAL = re.compile(r'^b"[^"]*"\s+as\s')


def struct_bodies(text, name):
    """Yield (line_number, body) for each `name { .. }` literal in `text`."""
    for m in re.finditer(re.escape(name) + r"\s*\{", text):
        depth, i = 1, m.end()
        while i < len(text) and depth:
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
            i += 1
        yield text.count("\n", 0, m.start()) + 1, text[m.end() : i - 1]


def field(body, name):
    """The initialiser expression of `name` in a struct-literal body."""
    m = re.search(re.escape(name) + r"\s*:\s*", body)
    if not m:
        return None
    depth, i = 0, m.end()
    while i < len(body):
        c = body[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            break
        i += 1
    return body[m.end() : i].strip()


def violations(text, where):
    out = []
    for line, body in struct_bodies(text, "ble_gap_cfg_device_name_t"):
        p_value = field(body, "p_value")
        if p_value is None:
            out.append(f"{where}:{line}: no p_value field found in the literal")
            continue
        p_value = " ".join(p_value.split())
        stack = "BLE_GATTS_VLOC_STACK" in body
        user = "BLE_GATTS_VLOC_USER" in body
        if stack and not (NULL_PTR.match(p_value) or FLASH_LITERAL.match(p_value)):
            out.append(
                f"{where}:{line}: vloc is BLE_GATTS_VLOC_STACK but p_value is "
                f"`{p_value}`, which is neither NULL nor a flash literal. The "
                f"SoftDevice requires flash-or-NULL there; a RAM pointer earns "
                f"NRF_ERROR_INVALID_ADDR and nrf-softdevice panics on it, "
                f"pre-USB, in a boot loop. Pass ptr::null_mut() and write the "
                f"name after enable with sd_ble_gap_device_name_set."
            )
        elif user and NULL_PTR.match(p_value):
            out.append(
                f"{where}:{line}: vloc is BLE_GATTS_VLOC_USER but p_value is "
                f"NULL, which the SoftDevice forbids."
            )
    return out


# Positive control. The gate is only worth its runtime if a known-bad input
# makes it fire, so the bad shape is kept here verbatim from `e52dba1`.
BAD = """
        gap_device_name: Some(raw::ble_gap_cfg_device_name_t {
            p_value: gap_name.as_mut_ptr(),
            current_len: DEVICE_NAME_LEN as u16,
            max_len: DEVICE_NAME_LEN as u16,
            write_perm: unsafe { mem::zeroed() },
            _bitfield_1: raw::ble_gap_cfg_device_name_t::new_bitfield_1(
                raw::BLE_GATTS_VLOC_STACK as u8,
            ),
        }),
"""

GOOD = """
        gap_device_name: Some(raw::ble_gap_cfg_device_name_t {
            p_value: ptr::null_mut(),
            current_len: 0,
            max_len: DEVICE_NAME_LEN as u16,
            write_perm: unsafe { mem::zeroed() },
            _bitfield_1: raw::ble_gap_cfg_device_name_t::new_bitfield_1(
                raw::BLE_GATTS_VLOC_STACK as u8,
            ),
        }),
"""

LEGACY = """
        gap_device_name: Some(raw::ble_gap_cfg_device_name_t {
            p_value: b"leviculum" as *const u8 as _,
            current_len: 9,
            max_len: 9,
            write_perm: unsafe { mem::zeroed() },
            _bitfield_1: raw::ble_gap_cfg_device_name_t::new_bitfield_1(
                raw::BLE_GATTS_VLOC_STACK as u8,
            ),
        }),
"""


def self_test():
    rc = 0
    for label, text, want_fail in (
        ("e52dba1 RAM pointer", BAD, True),
        ("null pointer", GOOD, False),
        ("flash literal", LEGACY, False),
    ):
        found = violations(text, "<fixture>")
        if bool(found) != want_fail:
            verb = "did not fire on" if want_fail else "fired on"
            print(f"{TAG} FAIL self-test: the gate {verb} the {label} fixture")
            rc = 1
        else:
            print(f"{TAG} ok   self-test: {label} fixture")
    return rc


root, arg = Path(sys.argv[1]), sys.argv[2]

if arg == "--self-test":
    sys.exit(self_test())

if arg:
    print(f"{TAG} unknown argument: {arg}")
    sys.exit(2)

# The self-test runs on every invocation: a gate that has silently stopped
# being able to fail is worse than no gate.
rc = self_test()

sources = sorted((root / "leviculum-nrf" / "src").rglob("*.rs"))
seen = 0
for path in sources:
    text = path.read_text(encoding="utf-8")
    if "ble_gap_cfg_device_name_t" not in text:
        continue
    where = path.relative_to(root)
    for v in violations(text, where):
        print(f"{TAG} FAIL {v}")
        rc = 1
    seen += 1

if seen == 0:
    print(f"{TAG} FAIL no ble_gap_cfg_device_name_t literal found under")
    print(f"{TAG} leviculum-nrf/src — the gate is checking nothing. If the")
    print(f"{TAG} config genuinely moved, point this script at its new home.")
    rc = 1
elif rc == 0:
    print(f"{TAG} ok   p_value honours the vloc contract in {seen} file(s)")

sys.exit(rc)
PY
