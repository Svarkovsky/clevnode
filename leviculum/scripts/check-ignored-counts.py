#!/usr/bin/env python3
"""Census gate for #[ignore]d tests (Codeberg #191c).

An #[ignore]d test is run by nothing: it compiles, it is registered, and it
is never executed. Codeberg #189 found a suite that had been in exactly that
state for a month while it was broken, and no gate anywhere said so. This
script counts every ignored test in the workspace and compares the counts
against the pinned numbers in scripts/ignored-counts.txt, so growing the
bucket is a diff instead of a side effect.

Enumeration is exhaustive, not a hand-picked list of units: every test
executable `cargo test --workspace --no-run` produces is queried with
`--ignored --list`, and so is every package's doc-test set. A unit missing
from the pin file is expected to have zero ignored tests, so a brand new
test binary is covered without anyone remembering to add it.

Usage:
    python3 scripts/check-ignored-counts.py            # the gate
    python3 scripts/check-ignored-counts.py --print    # current census,
                                                       # in pin-file format
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PIN_FILE = ROOT / "scripts" / "ignored-counts.txt"
PIN_FILE_REL = PIN_FILE.relative_to(ROOT)

# `<name>: test` for both libtest binaries and rustdoc's doc-test lister.
LIST_LINE = re.compile(r"^(?P<name>.+): test$")


def cargo(args: list[str]) -> str:
    """Run a cargo command from the repo root, returning stdout."""
    proc = subprocess.run(
        ["cargo", *args],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        sys.exit(f"[ignored-census] `cargo {' '.join(args)}` failed")
    return proc.stdout


def read_pins() -> dict[str, int]:
    pins: dict[str, int] = {}
    for lineno, raw in enumerate(PIN_FILE.read_text().splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        count, _, selector = line.partition(" ")
        selector = " ".join(selector.split())
        if not selector or not count.isdigit():
            sys.exit(f"[ignored-census] {PIN_FILE_REL}:{lineno}: malformed line: {raw!r}")
        pins[selector] = int(count)
    return pins


def selector_for(pkg: str, target: dict) -> str:
    """The cargo selector that runs one test unit, used as its census key."""
    kinds = target["kind"]
    name = target["name"]
    for kind, flag in (("lib", "--lib"), ("proc-macro", "--lib")):
        if kind in kinds:
            return f"-p {pkg} {flag}"
    for kind in ("test", "bin", "example", "bench"):
        if kind in kinds:
            return f"-p {pkg} --{kind} {name}"
    return f"-p {pkg} --{kinds[0]} {name}"


def binary_units() -> dict[str, list[str]]:
    """Ignored test names per libtest executable in the workspace."""
    meta = json.loads(cargo(["metadata", "--no-deps", "--format-version", "1"]))
    workspace_pkg = {p["id"]: p["name"] for p in meta["packages"]}

    units: dict[str, list[str]] = {}
    for line in cargo(
        ["test", "--workspace", "--no-run", "--message-format=json"]
    ).splitlines():
        if not line.startswith("{"):
            continue
        msg = json.loads(line)
        if msg.get("reason") != "compiler-artifact" or not msg.get("executable"):
            continue
        if not msg.get("profile", {}).get("test"):
            continue
        pkg = workspace_pkg.get(msg["package_id"])
        if pkg is None:  # a dependency's test target, not ours to pin
            continue
        listing = subprocess.run(
            [msg["executable"], "--ignored", "--list"],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        if listing.returncode != 0:
            sys.exit(f"[ignored-census] {msg['executable']} --ignored --list failed")
        units[selector_for(pkg, msg["target"])] = [
            m.group("name")
            for m in (LIST_LINE.match(ln) for ln in listing.stdout.splitlines())
            if m
        ]
    return units


def doc_units() -> dict[str, list[str]]:
    """Ignored doc-tests per package (```ignore blocks).

    One cargo invocation per package on purpose: in a single
    `--workspace --doc` run cargo prints the `Doc-tests <crate>` headers on
    stderr while rustdoc prints the test names on stdout, so attributing a
    name to a package would depend on how two streams interleave.
    """
    meta = json.loads(cargo(["metadata", "--no-deps", "--format-version", "1"]))
    packages = sorted(
        p["name"]
        for p in meta["packages"]
        if any(
            ("lib" in t["kind"] or "proc-macro" in t["kind"]) and t.get("doctest", True)
            for t in p["targets"]
        )
    )

    units: dict[str, list[str]] = {}
    for pkg in packages:
        listing = cargo(["test", "-p", pkg, "--doc", "--", "--ignored", "--list"])
        units[f"-p {pkg} --doc"] = [
            m.group("name")
            for m in (LIST_LINE.match(ln) for ln in listing.splitlines())
            if m
        ]
    return units


def guidance(selector: str, pinned: int, found: int, names: list[str]) -> str:
    listing = "\n".join(f"        {n}" for n in names) or "        (none)"
    return f"""
  {selector}
      pinned {pinned}, found {found}
      the unit's ignored tests are now:
{listing}

An #[ignore]d test is run by nothing -- not by `just fast`, not by
`just standard`, not by any tier. Codeberg #189 is one that stayed that
way for a month while it was broken. So adding one is a decision, and
this gate asks you to make it, in one of two ways:

  1. ROUTE IT. Drop the #[ignore] so the test rides its suite in
     `just standard`; or, if it must run apart from the parallel suite,
     add a line to a Justfile tier that runs it by name, e.g.

         cargo test {selector} <test-name> -- --ignored

     (scripts/run-status-parity.sh is the worked example). Routing needs
     no edit here -- but note the test still counts as ignored, so say in
     the pin file's comment where it runs.

  2. RAISE THE PIN. Edit {PIN_FILE_REL}:

         -{pinned:<4} {selector}
         +{found:<4} {selector}

     and say in the COMMIT MESSAGE why the test cannot be routed and
     where it does get run instead (by hand, on the rig, before a
     release). The number is the easy part; the sentence is the point.

Raising the pin is deliberately a one-line edit: adding a test that
genuinely belongs in the bucket must not be painful, or this gate gets
deleted the first time it is inconvenient. What it must not be is
silent, and a diff on {PIN_FILE_REL} is not silent.
"""


def shrink_guidance(selector: str, pinned: int, found: int) -> str:
    return f"""
  {selector}
      pinned {pinned}, found {found} -- the bucket SHRANK.

Somebody routed or deleted an ignored test, which is the good direction.
Lower the pin in {PIN_FILE_REL} to {found} in the same commit:

         -{pinned:<4} {selector}
         +{found:<4} {selector}

A pin left above the real count leaves that many free slots for the next
ignored test to appear in unnoticed, which is exactly what this gate
exists to prevent.
"""


def main() -> int:
    census = binary_units()
    census.update(doc_units())
    pins = read_pins()

    if "--print" in sys.argv[1:]:
        for selector, names in sorted(census.items()):
            if names:
                print(f"{len(names):<4} {selector}")
        return 0

    failures: list[str] = []
    for selector in sorted(set(census) | set(pins)):
        pinned = pins.get(selector, 0)
        if selector not in census:
            failures.append(
                f"""
  {selector}
      pinned {pinned}, but this test unit no longer exists (renamed or
      removed). Delete its line from {PIN_FILE_REL}.
"""
            )
            continue
        names = census[selector]
        if len(names) > pinned:
            failures.append(guidance(selector, pinned, len(names), names))
        elif len(names) < pinned:
            failures.append(shrink_guidance(selector, pinned, len(names)))

    if failures:
        print("[ignored-census] FAIL: the set of #[ignore]d tests no longer matches")
        print(f"[ignored-census] the pinned census in {PIN_FILE_REL}.")
        for f in failures:
            print(f)
        return 1

    total = sum(len(n) for n in census.values())
    pinned_units = sum(1 for n in census.values() if n)
    print(
        f"[ignored-census] OK: {total} ignored tests across {pinned_units} units, "
        f"{len(census)} units checked, all matching {PIN_FILE_REL}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
