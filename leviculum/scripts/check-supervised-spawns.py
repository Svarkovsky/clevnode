#!/usr/bin/env python3
"""Census gate for process spawns that are not supervised.

`docs/src/concepts/checks-and-citations.md`:

    A harness that spawns a long-lived external process must ensure it dies
    with the harness, however the harness dies. Cleanup code is a convenience;
    the kernel is the guarantee.

`leviculum_std::process::spawn_supervised` is that guarantee — it sets
`PR_SET_PDEATHSIG` on the child from a thread that outlives every spawn. A bare
`Command::new(..).spawn()` has none of it, and the failure is invisible: the
test passes, the daemon is simply still running afterwards. Seven of them were
found alive on 2026-08-07, the oldest over four hours old and from several
different runs, and one had held `just standard` open for two hours.

So the bare spawns are counted, per file, against `scripts/supervised-spawn-counts.txt`.
A file that gains one and is not in the pin file fails this gate; the pin file
carries a reason per entry, so keeping a bare spawn is a decision that lands in
a diff a reviewer can ask about.

"No orphans found" is satisfied forever by a check that stopped looking, so the
classifier is exercised on two fixtures before it reports anything about the
tree: one holding four bare spawns in the four shapes the tree uses, all of
which must be reported, and one holding a supervised call, a runtime spawn, a
string and a comment, none of which may be.

Usage:
    python3 scripts/check-supervised-spawns.py            # the gate
    python3 scripts/check-supervised-spawns.py --print    # current census,
                                                          # in pin-file format
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PIN_FILE = ROOT / "scripts" / "supervised-spawn-counts.txt"
PIN_FILE_REL = PIN_FILE.relative_to(ROOT)

# Third-party and generated trees: not ours to fix, and `target/` holds copies
# of our own sources that would double every count.
SKIP_DIRS = {".git", "target", "vendor", "reference", ".rnode-tools", ".rnode-fw"}

# Call sites of the supervised spawn, in both its blocking and its tokio
# spelling. `(?<!fn )` keeps the two definitions in leviculum-std/src/process.rs
# out of the count, so the floor below counts users and not the helper itself.
SUPERVISED = re.compile(r"(?<!fn )\bspawn_supervised(?:_async)?\s*\(")
# A process spawn takes no arguments. Every runtime spawn in this tree
# (`tokio::spawn(fut)`, `thread::Builder::new().spawn(closure)`,
# `JoinSet::spawn(..)`) takes at least one, so the empty argument list is most
# of the classification; `receiver_words` below is the rest of it.
PROCESS_SPAWN = re.compile(r"\.spawn\(\s*\)")

IDENT = re.compile(r"[A-Za-z0-9_]")


def strip_noise(src: str) -> str:
    """Blank out comments and literal contents, preserving every offset.

    Offsets are preserved so a hit's position still maps to a real line number,
    and so the backward walk in `receiver_words` sees the same layout the source
    has. Replacement is by space, except newlines, which are kept so line
    numbering survives.
    """
    out = list(src)
    i, n = 0, len(src)

    def blank(start: int, end: int) -> None:
        for k in range(start, min(end, n)):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = src[i]
        if c == "/" and i + 1 < n and src[i + 1] == "/":
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif c == "/" and i + 1 < n and src[i + 1] == "*":
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth += 1
                    j += 2
                elif src.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif c == "r" and (m := re.match(r'r(#*)"', src[i:])):
            close = '"' + m.group(1)
            j = src.find(close, i + len(m.group(0)))
            j = n if j < 0 else j + len(close)
            blank(i, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            blank(i, min(j + 1, n))
            i = j + 1
        elif c == "'":
            # A char literal, or a lifetime. `'a` is not a literal and must not
            # swallow the rest of the line.
            m = re.match(r"'(\\.|[^\\'])'", src[i:])
            if m:
                blank(i, i + len(m.group(0)))
                i += len(m.group(0))
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


def receiver_words(code: str, dot: int) -> list[str]:
    """Every identifier in the receiver chain whose `.spawn()` sits at `dot`.

    Walks backwards over call groups (`(..)`, `[..]`), method names and path
    separators, collecting each identifier it crosses.
    `std::process::Command::new(x).args(y).spawn()` yields
    `["new", "Command", "process", "std"]` — the fully-qualified spelling is
    why this returns the whole chain and not just its head; a single-token head
    would read `std` there and miss the site.
    `cmd.spawn()` yields `["cmd"]`, `thread::Builder::new().spawn(..)` yields
    `["new", "Builder", "thread"]`.
    """
    words: list[str] = []
    i = dot - 1
    closers = {")": "(", "]": "["}
    while i >= 0:
        while i >= 0 and code[i] in " \t\n\r":
            i -= 1
        if i < 0:
            break
        if code[i] in closers:
            close, openc = code[i], closers[code[i]]
            depth, i = 1, i - 1
            while i >= 0 and depth:
                if code[i] == close:
                    depth += 1
                elif code[i] == openc:
                    depth -= 1
                i -= 1
            continue
        if IDENT.match(code[i]):
            end = i + 1
            while i >= 0 and IDENT.match(code[i]):
                i -= 1
            words.append(code[i + 1 : end])
            j = i
            while j >= 0 and code[j] in " \t\n\r":
                j -= 1
            if j >= 1 and code[j] == ":" and code[j - 1] == ":":
                i = j - 2
                continue
            if j >= 0 and code[j] == ".":
                i = j - 1
                continue
            break
        break
    return words


def is_command_binding(code: str, name: str) -> bool:
    """Does `name` denote a `Command` anywhere in this file?

    Three shapes, all of them present in the tree: a `let mut cmd =
    Command::new(..)` binding, a `cmd: &mut Command` parameter, and a
    `fn jl() -> Command` factory whose call is the head of the chain
    (`leviculum-std/tests/jl_filter.rs`). The factory form is why this is not a
    grep for `Command::new`: the spawn and the construction are in different
    functions.
    """
    name = re.escape(name)
    return bool(
        re.search(rf"\b{name}\b[^;=]*=\s*[^;]*Command::new", code)
        or re.search(rf"\b{name}\s*:\s*&?\s*mut\s+Command\b", code)
        or re.search(rf"\bfn\s+{name}\s*\([^)]*\)\s*->\s*(?:std::process::)?Command\b", code)
    )


def bare_spawns(code: str) -> list[int]:
    """Offsets of process spawns in `code` that do not go through the helper."""
    hits = []
    for m in PROCESS_SPAWN.finditer(code):
        words = receiver_words(code, m.start())
        if "Command" in words or (words and is_command_binding(code, words[-1])):
            hits.append(m.start())
    return hits


def line_of(src: str, offset: int) -> int:
    return src.count("\n", 0, offset) + 1


def sources() -> list[Path]:
    found = []
    for path in ROOT.rglob("*.rs"):
        rel = path.relative_to(ROOT)
        if SKIP_DIRS & set(rel.parts):
            continue
        found.append(path)
    return sorted(found)


# ---------------------------------------------------------------------------
# The canary. Checked before anything is reported about the tree, because a
# classifier that has stopped matching reports a clean tree forever.
# ---------------------------------------------------------------------------

CANARY_BARE = """
fn a() -> std::io::Result<Child> {
    Command::new("python3").arg("d.py").stdout(Stdio::piped()).spawn()
}
fn b() {
    let mut cmd = Command::new("socat");
    cmd.arg("-d");
    let _ = cmd.spawn();
}
fn jl() -> Command { Command::new("jl") }
fn c() {
    let _ = jl().args(&["-x"]).spawn();
}
fn d() -> std::io::Result<Child> {
    std::process::Command::new("sleep").arg("600").spawn()
}
"""

CANARY_CLEAN = """
fn a() -> std::io::Result<Child> {
    let mut cmd = Command::new("python3");
    cmd.arg("d.py");
    spawn_supervised(cmd)                       // not a bare spawn
}
fn b() {
    tokio::spawn(async { work().await });       // a runtime spawn, not a process
    std::thread::Builder::new().name("x".into()).spawn(move || loop {})?;
    let _ = "a string mentioning .spawn() and Command::new";
    // a comment mentioning Command::new(..).spawn()
}
"""


def canary() -> None:
    bare = bare_spawns(strip_noise(CANARY_BARE))
    if len(bare) != 4:
        sys.exit(
            f"[supervised-spawns] CANARY FAILED: the classifier found {len(bare)} "
            "bare spawns in a fixture that has exactly 4. It has stopped "
            "recognising the shape it exists to find."
        )
    clean = bare_spawns(strip_noise(CANARY_CLEAN))
    if clean:
        sys.exit(
            f"[supervised-spawns] CANARY FAILED: the classifier reported "
            f"{len(clean)} bare spawns in a fixture that has none. Every real "
            "finding it prints is suspect."
        )


# ---------------------------------------------------------------------------
# The pin file
# ---------------------------------------------------------------------------


def read_pins() -> tuple[dict[str, int], int]:
    pins: dict[str, int] = {}
    floor = 0
    for lineno, raw in enumerate(PIN_FILE.read_text().splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        count, _, rest = line.partition(" ")
        rest = rest.strip()
        if not count.isdigit() or not rest:
            sys.exit(
                f"[supervised-spawns] {PIN_FILE_REL}:{lineno}: malformed line: {raw!r}"
            )
        if rest == "supervised-call-sites-minimum":
            floor = int(count)
        else:
            pins[rest] = int(count)
    return pins, floor


def main() -> int:
    canary()

    census: dict[str, list[int]] = {}
    supervised_calls = 0
    for path in sources():
        src = path.read_text()
        code = strip_noise(src)
        supervised_calls += len(SUPERVISED.findall(code))
        hits = bare_spawns(code)
        if hits:
            rel = str(path.relative_to(ROOT))
            census[rel] = [line_of(src, off) for off in hits]

    if "--print" in sys.argv:
        for rel in sorted(census):
            print(f"{len(census[rel]):<4} {rel}")
        print(f"{supervised_calls:<4} supervised-call-sites-minimum")
        return 0

    pins, floor = read_pins()
    failures: list[str] = []

    for rel in sorted(set(census) | set(pins)):
        want = pins.get(rel, 0)
        lines = census.get(rel, [])
        if len(lines) == want:
            continue
        if want == 0:
            at = ", ".join(f"{rel}:{n}" for n in lines)
            failures.append(
                f"{len(lines)} bare process spawn(s) in an unpinned file: {at}\n"
                f"      Route them through leviculum_std::process::spawn_supervised,\n"
                f"      or add an entry to {PIN_FILE_REL} saying why not."
            )
        elif not lines:
            failures.append(
                f"{rel}: pinned at {want} bare spawn(s), found none — the file no "
                f"longer needs its entry in {PIN_FILE_REL}."
            )
        else:
            at = ", ".join(f"{rel}:{n}" for n in lines)
            failures.append(
                f"{rel}: pinned at {want} bare spawn(s), found {len(lines)}: {at}"
            )

    # Asymmetric on purpose: adding a supervised spawn must stay free, but
    # deleting one is exactly the regression a bare-spawn count cannot see,
    # because removing the process entirely also removes the bare spawn.
    if supervised_calls < floor:
        failures.append(
            f"supervised call sites fell from {floor} to {supervised_calls}. "
            f"If a supervised spawn was legitimately removed, lower the "
            f"`supervised-call-sites-minimum` line in {PIN_FILE_REL} and say why."
        )

    print(
        f"[supervised-spawns] {supervised_calls} supervised call site(s) "
        f"(floor {floor}); "
        f"{sum(len(v) for v in census.values())} bare process spawn(s) in "
        f"{len(census)} file(s), all pinned"
        if not failures
        else f"[supervised-spawns] {len(failures)} problem(s)"
    )
    for f in failures:
        print(f"  FAIL: {f}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
