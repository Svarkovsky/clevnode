#!/usr/bin/env bash
#
# Board pin-map gate for the nRF firmware: our pins against the reference.
#
# The two greps that guarded the pin maps before this were both
# internal-consistency checks — no pin aliased twice inside a board file, no
# board's alias used from another board. Both were true of `e5d62b95`, and
# its T114 QSPI map was still wrong: IO2 and IO3 named P1.00/P1.01 where the
# part has WP#/HOLD# on P0.07/P0.05. A map that is consistently wrong is
# consistent, so no check that only reads our own tree could have seen it,
# and only this gate reads the reference.
#
# It was NOT the `id=00:00:00` on the T114's boot line. That reading was
# blamed on the wrong IO3 leaving the part held, and the blame was wrong:
# `blocking_custom_instruction` runs single-line, so a JEDEC read uses SCK,
# CS, IO0 and IO1 only — all four correct in the old map. The pins had to be
# fixed for the quad path regardless, and the flash's silence is a separate
# question that `qspi.rs` answers with a deep-power-down release.
#
# Three claims, in the order they can fail:
#
#   1. Each board file's pin aliases equal the recorded reference numbers.
#   2. The bin's `qspi::identify_at_boot` / `lora::init` call sites pass the
#      same pins as those aliases. This is not redundant: the call sites pass
#      `p.P0_07` peripherals directly, not the aliases, so the alias can be
#      right while the hardware sees something else. Fixing only `boards/`
#      would have left the T114 exactly as red as it was.
#   3. The recorded numbers still match the upstream variant header, when a
#      Meshtastic tree is at hand. Without one, 1 and 2 still run and this
#      one says so rather than passing quietly.
#
# The reference numbers and the scope — QSPI and LoRa, and why not the rest —
# live in `leviculum-nrf/reference-pins.toml`.
#
# Usage:
#   check-nrf-board-pins.sh              # gate the firmware sources
#   check-nrf-board-pins.sh --self-test  # positive control, no sources
#
# The Meshtastic checkout is found via $MESHTASTIC_TREE, else
# `../meshtastic` next to the repo, else `~/coding/meshtastic`.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

python3 - "$ROOT" "${1:-}" <<'PY'
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

TAG = "[board-pins]"

# Argument order of the two initialisers, after the peripheral. Written out
# rather than taken from the table's key order, because a table reordered by
# accident must not silently redefine what the call site is compared against.
CALL_ORDER = {
    "qspi": ["sck", "cs", "io0", "io1", "io2", "io3"],  # qspi::identify_at_boot
    "lora": ["sck", "mosi", "miso", "cs", "reset", "busy", "dio1"],  # lora::init
}
CALLEE = {"qspi": "qspi::identify_at_boot", "lora": "lora::init"}

ALIAS_RE = re.compile(r"pub type (\w+)\s*=\s*peripherals::(P[01]_\d\d)\s*;")
PIN_ARG_RE = re.compile(r"\bp\.(P[01]_\d\d)\b")


def pin_name(n):
    """nRF52840 GPIO ordinal as embassy names it: 0..31 P0, 32..63 P1."""
    return f"P{n // 32}_{n % 32:02}"


def aliases(text):
    return dict(ALIAS_RE.findall(text))


def call_args(text, callee):
    """The `p.Pn_mm` arguments of the first `callee(...)` call, in order."""
    m = re.search(re.escape(callee) + r"\s*\(", text)
    if not m:
        return None
    depth, i = 1, m.end()
    while i < len(text) and depth:
        if text[i] == "(":
            depth += 1
        elif text[i] == ")":
            depth -= 1
        i += 1
    return PIN_ARG_RE.findall(text[m.end() : i - 1])


def define_value(text, name):
    """`#define NAME (32 + 14)` → 46. None if absent or commented out."""
    for line in text.splitlines():
        line = line.split("//")[0].strip()
        if not line.startswith("#define"):
            continue
        m = re.match(r"#define\s+" + re.escape(name) + r"\b(.*)$", line)
        if not m:
            continue
        v = m.group(1).strip()
        parts = re.fullmatch(r"\(?\s*(\d+)\s*(?:\+\s*(\d+)\s*)?\)?", v)
        if not parts:
            return None
        return int(parts.group(1)) + int(parts.group(2) or 0)
    return None


def check_aliases(board, group, entries, text, where):
    out = []
    found = aliases(text)
    for key, e in entries.items():
        want = pin_name(e["pin"])
        got = found.get(e["alias"])
        if got is None:
            out.append(
                f"{where}: {board} {group}.{key}: no `pub type {e['alias']}` "
                f"— expected {want} from {e['define']}"
            )
        elif got != want:
            out.append(
                f"{where}: {board} {group}.{key}: {e['alias']} is {got}, "
                f"reference {e['define']} says {want}"
            )
    return out


def check_call_site(board, group, entries, text, where):
    args = call_args(text, CALLEE[group])
    order = CALL_ORDER[group]
    if args is None:
        return [
            f"{where}: {board}: no `{CALLEE[group]}(` call found — the gate "
            f"is checking nothing. If the call moved, point this script at it."
        ]
    if len(args) < len(order):
        return [
            f"{where}: {board}: `{CALLEE[group]}` is passed {len(args)} pins, "
            f"expected {len(order)} ({', '.join(order)})"
        ]
    out = []
    for key, got in zip(order, args):
        want = pin_name(entries[key]["pin"])
        if got != want:
            out.append(
                f"{where}: {board}: `{CALLEE[group]}` argument {key} is "
                f"p.{got}, reference {entries[key]['define']} says {want}"
            )
    return out


def check_variant(board, group, entries, text, where):
    out = []
    for key, e in entries.items():
        got = define_value(text, e["define"])
        if got is None:
            out.append(
                f"{where}: {board} {group}.{key}: {e['define']} is not defined "
                f"upstream any more — reference-pins.toml records {e['pin']} "
                f"for it and can no longer be believed"
            )
        elif got != e["pin"]:
            out.append(
                f"{where}: {board} {group}.{key}: {e['define']} is {got} "
                f"upstream, reference-pins.toml records {e['pin']}"
            )
    return out


# Positive control. Fixtures are the shapes the real files have, with the
# e5d62b95 fault re-injected into each of the three layers in turn.
GOOD_BOARD = """
pub type LoRaSck = peripherals::P0_19;
pub type QspiClk = peripherals::P1_14;
/// QSPI flash IO3 (HOLD#)
pub type QspiIo3 = peripherals::P0_05;
"""
BAD_BOARD = GOOD_BOARD.replace("QspiIo3 = peripherals::P0_05", "QspiIo3 = peripherals::P1_01")

GOOD_BIN = """
    if let Some(mut flash) = leviculum_nrf::qspi::identify_at_boot(
        p.QSPI,
        p.P1_14.into(), // SCK
        p.P1_15.into(), // CSN
        p.P1_12.into(), // IO0
        p.P1_13.into(), // IO1
        p.P0_07.into(), // IO2 / WP#
        p.P0_05.into(), // IO3 / HOLD#
        t114::CONFIG.qspi_part,
    ) {
"""
BAD_BIN = GOOD_BIN.replace("p.P0_05.into(), // IO3", "p.P1_01.into(), // IO3")

GOOD_VARIANT = """
// QSPI Pins
#define PIN_QSPI_SCK (32 + 14)
#define PIN_QSPI_IO3 (0 + 5)   // HOLD if using two bit interface
"""
BAD_VARIANT = GOOD_VARIANT.replace("#define PIN_QSPI_IO3 (0 + 5)", "#define PIN_QSPI_IO3 (32 + 1)")

FIX_QSPI = {
    "sck": {"pin": 46, "alias": "QspiClk", "define": "PIN_QSPI_SCK"},
    "cs": {"pin": 47, "alias": "QspiCs", "define": "PIN_QSPI_CS"},
    "io0": {"pin": 44, "alias": "QspiIo0", "define": "PIN_QSPI_IO0"},
    "io1": {"pin": 45, "alias": "QspiIo1", "define": "PIN_QSPI_IO1"},
    "io2": {"pin": 7, "alias": "QspiIo2", "define": "PIN_QSPI_IO2"},
    "io3": {"pin": 5, "alias": "QspiIo3", "define": "PIN_QSPI_IO3"},
}
FIX_IO3 = {"io3": FIX_QSPI["io3"]}
FIX_SCK_IO3 = {"sck": FIX_QSPI["sck"], "io3": FIX_QSPI["io3"]}


def self_test():
    rc = 0
    cases = (
        ("alias", check_aliases, FIX_IO3, GOOD_BOARD, BAD_BOARD),
        ("call site", check_call_site, FIX_QSPI, GOOD_BIN, BAD_BIN),
        ("variant", check_variant, FIX_SCK_IO3, GOOD_VARIANT, BAD_VARIANT),
    )
    for label, fn, entries, good, bad in cases:
        for text, want_fail in ((bad, True), (good, False)):
            found = fn("t114", "qspi", entries, text, "<fixture>")
            if bool(found) != want_fail:
                verb = "did not fire on" if want_fail else "fired on"
                shape = "wrong" if want_fail else "correct"
                print(f"{TAG} FAIL self-test: the {label} check {verb} the {shape} fixture")
                rc = 1
        if rc == 0:
            print(f"{TAG} ok   self-test: {label} check fires on P1_01-for-P0_05")
    return rc


def tree_revision(tree):
    """What the checkout calls itself, or None if it will not say.

    Refuses a revision belonging to an *enclosing* repository: git walks
    upwards, so a variant tree sitting inside some other checkout would
    otherwise be reported at that checkout's revision — a provenance line
    that lies is worse than one that says it does not know.
    """

    def git(*args):
        try:
            r = subprocess.run(
                ["git", "-C", str(tree), *args], capture_output=True, text=True, timeout=10
            )
        except (OSError, subprocess.SubprocessError):
            return None
        return r.stdout.strip() or None if r.returncode == 0 else None

    top = git("rev-parse", "--show-toplevel")
    if top is None or Path(top).resolve() != Path(tree).resolve():
        return None
    return git("describe", "--tags", "--always", "--dirty") or git("rev-parse", "--short", "HEAD")


def find_variants(root):
    """The Meshtastic checkout, or None. Never a hard-coded home directory."""
    candidates = []
    env = os.environ.get("MESHTASTIC_TREE")
    if env:
        candidates.append(Path(env))
    candidates += [root.parent / "meshtastic", Path.home() / "coding" / "meshtastic"]
    for c in candidates:
        if (c / "variants").is_dir():
            return c
    return None


root, arg = Path(sys.argv[1]), sys.argv[2]

if arg == "--self-test":
    sys.exit(self_test())

if arg:
    print(f"{TAG} unknown argument: {arg}")
    sys.exit(2)

# The self-test runs on every invocation: a gate that has silently stopped
# being able to fail is worse than no gate.
rc = self_test()

table_path = root / "leviculum-nrf" / "reference-pins.toml"
table = tomllib.loads(table_path.read_text(encoding="utf-8"))["boards"]

tree = find_variants(root)
if tree is None:
    print(f"{TAG} note no Meshtastic checkout found — set $MESHTASTIC_TREE to")
    print(f"{TAG} re-derive {table_path.relative_to(root)} from its variant")
    print(f"{TAG} headers. The recorded numbers are still gated against ours.")
else:
    # Which tree answered, on the run's own output. Two hosts here carry
    # different Meshtastic revisions, so claim 3 can pass on one and fail on
    # the other; without this line, neither run says which one it read.
    print(f"{TAG} note upstream {tree} at {tree_revision(tree) or 'an unknown revision'}")

checked = 0
for board, spec in table.items():
    for group in ("qspi", "lora"):
        entries = spec[group]
        unknown = set(CALL_ORDER[group]) - set(entries)
        if unknown:
            print(f"{TAG} FAIL {board} {group}: table is missing {sorted(unknown)}")
            rc = 1
            continue
        board_text = (root / spec["board"]).read_text(encoding="utf-8")
        bin_text = (root / spec["bin"]).read_text(encoding="utf-8")
        problems = check_aliases(board, group, entries, board_text, spec["board"])
        called = {k: v for k, v in entries.items() if v.get("call_site", True)}
        problems += check_call_site(board, group, called, bin_text, spec["bin"])
        if tree is not None:
            variant = tree / spec["variant"]
            if not variant.is_file():
                problems.append(
                    f"{spec['variant']}: not in {tree} — the upstream half of "
                    f"the gate cannot run for {board}"
                )
            else:
                problems += check_variant(
                    board, group, entries, variant.read_text(encoding="utf-8"), spec["variant"]
                )
        for p in problems:
            print(f"{TAG} FAIL {p}")
            rc = 1
        checked += len(entries)

if checked == 0:
    print(f"{TAG} FAIL the reference table is empty — the gate checked nothing")
    rc = 1
elif rc == 0:
    where = "sources, call sites and upstream" if tree else "sources and call sites"
    print(f"{TAG} ok   {checked} pins agree across {where}")

sys.exit(rc)
PY
