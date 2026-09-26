#!/usr/bin/env python3
"""Run a test command and record what it actually EXECUTED (Guarantee B, step 1).

docs/src/concepts/checks-and-citations.md §Guarantee B: half of it exists --
scripts/check-ignored-counts.py pins how many tests are #[ignore]d. Nothing
says whether the rest are executed by anything. Two suites had been running
nowhere for a month (Codeberg #189) while every gate was green.

This script is the first half of the other half: a gate wrapper that emits a
manifest of the test names its command ran. Step 2 (not here) takes the union
of those manifests and reports every test that exists in no manifest.

    python3 scripts/run-with-manifest.py --gate mvr -- \
        cargo test -p leviculum-std --test mvr -- --test-threads=1

The command's stdout and stderr are merged and passed straight through, and
its exit status is this script's exit status, so a wrapped gate behaves like
the bare one.

THE FULL OUTPUT IS ALSO KEPT ON DISK
------------------------------------
Passing output through is not the same as preserving it. Whatever runs a gate
decides how much of its stdout survives, and a caller that summarised a red run
with `tail -6` threw away the only assertion diff it produced (Codeberg #195);
re-running turned the run green and the evidence was gone for good. So every
invocation also streams the merged output to <gate>.log next to the manifest,
line by line (a killed run keeps what it printed), and a run that exits non-zero
copies it to <gate>.failed.log, which nothing but the NEXT failure overwrites.
Green runs cannot clobber the last red one, which is the case that mattered.

WHY RUN OUTPUT AND NOT `cargo test --list`
------------------------------------------
A list records intent. `cargo test -- --exact <typo>` matches nothing, runs
zero tests and exits 0, so a by-name gate reads green whether or not it
measured anything -- scripts/run-status-parity.sh closes exactly that hole by
hand for itself (EXPECTED=3, parsed back out of the summary line). Parsing the
run generalises that fix: the manifest cannot contain a test that did not run,
and the reconciliation below cannot pass if it does.

WHERE THE MANIFESTS GO
----------------------
$LEVICULUM_MANIFEST_DIR, else
$XDG_STATE_HOME/leviculum-ci/test-manifests/<repo-slug>/<gate>.json
(default $HOME/.local/state). Not in the repo: this is run state, not source,
and a checked-in artefact gets diffed, blessed and eventually committed
wrong. Not in target/: `cargo clean` deletes it, and the tiers run with
different CARGO_TARGET_DIR values, which would silently split one union into
several. ~/.local/state/leviculum-ci/ is where this repo already keeps CI run
state (last-results.txt and the tier logs).

<repo-slug> is <dirname>-<hash of the real repo path>: two checkouts on one
host (the CI tree and the rig worktree) must not overwrite each other's
manifests, or step 2 reads a union assembled from two different trees.

The file carries gate, command, commit, dirtiness, host, repo and both
timestamps, which is what step 2 needs to age a manifest out.

One warning for whoever builds step 2. This repo aged out a tier-2 result
exactly that way from a pre-push hook, and the bound was unsatisfiable for
46 days: the marker had one writer, nothing was scheduled to run it, and the
remedy the failure message named wrote no marker at all. An age bound is only
as live as its writer, so step 2 must age a gate out against the manifest that
gate itself emits -- which is the point of writing one per `{{manifest}}`
invocation -- and never against a signal produced somewhere else.

A GATE MUST PASS, FAIL, OR SAY IT GAVE UP
-----------------------------------------
It must never end a fourth way: still running, verdict already determined,
nobody told. On 2026-08-07 `just standard` sat for two hours holding a red it
had decided in its first minute, and was found by noticing a log's mtime.

The mechanism, end to end. A leviculum-ffi test aborted in a destructor;
SIGABRT skips unwinding, so the `Drop` that would have killed the
scripts/test_daemon.py it had spawned never ran. The orphaned daemon had
inherited cargo's stdout, cargo exited and became a zombie, and the read loop
here kept reading -- `for raw in proc.stdout:` ends on **EOF of the pipe**, not
on **exit of the child**, and one surviving write end holds it open forever.
`proc.wait()` sat behind that loop and was never reached. Killing the orphan by
hand ended the run instantly with the exit code it had already had.

The class is wider than the one test: **a gate that waits for a pipe to close
instead of for its child to exit can be held open by any leaked grandchild**,
and every harness here that spawns an external process (Python daemons, C
binaries, lnsd/rnsd, docker) can leak one. So the fix is in the wrapper, not in
the test that leaked. See run_command() for the three properties and
reaping_canary() for the standing pair that keeps them true.

STANDING CANARIES
-----------------
Both run before anything real does, on every invocation.

canary() feeds the parser a fixture holding tests that must appear in a
manifest and tests that must never, plus a deliberately miscounted unit that
the reconciler must reject. A manifest writer that silently stops writing -- a
libtest format change, a regex that stops matching -- is green forever, which
is the defect the concept page exists to remove; a one-time demonstration at
implementation time decays.

reaping_canary() spawns a child that leaks a grandchild holding the pipe and
asserts this wrapper still terminates with the child's exit status, names what
it killed, and claims to have killed nothing when nothing leaked -- plus that
the timeout fires and reports. It bounds itself from the outside, so a
regression fails it in seconds instead of wedging the suite the way the real
thing wedged the run.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import signal
import socket
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# 2 (2026-08-07): timeout_s / timed_out / killed / survived_sigkill. Additive,
# but step 2 must be able to tell "this gate answered" from "this gate gave up",
# and a reader that cannot see the difference would count a timed-out gate's
# partial manifest as coverage.
SCHEMA = 2

# libtest, one result per line: `test <name> ... <status>`. The name may hold
# spaces (doc-tests: `leviculum-micron/src/lib.rs - (line 14)`), so the name is
# non-greedy up to the first ` ... `.
RESULT_LINE = re.compile(r"^test (?P<name>.+?) \.\.\. ?(?P<rest>.*)$")
# libtest decorates a #[should_panic] test as `test <name> - should panic ...`,
# while `--list` (what step 2 enumerates with) prints the bare name. Recording
# the decorated form would leave three real leviculum-core tests looking unrun
# forever, and no count would disagree -- the manifest would be wrong in a way
# only a name-level join can see. Found exactly that way, on this script's
# first union.
DECORATION = re.compile(r" - should panic$")
# `running 12 tests` / `running 1 test`
RUNNING_LINE = re.compile(r"^running (?P<count>\d+) tests?$")
# `test result: ok. 12 passed; 0 failed; 3 ignored; 0 measured; 5 filtered out; ...`
SUMMARY_LINE = re.compile(
    r"^test result: \w+\.\s*(?P<passed>\d+) passed;\s*(?P<failed>\d+) failed;"
    r"\s*(?P<ignored>\d+) ignored;\s*(?P<measured>\d+) measured;"
    r"\s*(?P<filtered>\d+) filtered out"
)
# cargo, on stderr: `     Running unittests src/lib.rs (target/.../deps/x-hash)`
CARGO_RUNNING = re.compile(r"^\s*Running (?P<desc>.+?) \((?P<exe>[^()]+)\)$")
# cargo, on stderr: `   Doc-tests leviculum_micron`
CARGO_DOCTESTS = re.compile(r"^\s*Doc-tests (?P<crate>\S+)$")
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")

# `ignored` may carry a reason (`ignored, needs hardware`); `ok` may carry a
# time with --report-time. Prefix match, so both are covered.
STATUSES = (("ok", "ok"), ("FAILED", "failed"), ("ignored", "ignored"), ("bench", "ok"))


def classify(rest: str, glued: bool = False) -> str | None:
    """Map the tail of a libtest result line to ok/failed/ignored, or None.

    None means the status has not been printed yet: with
    `--test-threads=1 --nocapture` libtest writes `test <name> ... `, lets the
    test print, and writes `ok` afterwards, so the status arrives on a later
    line -- or glued to the end of the test's own output, when what the test
    printed did not end in a newline. `glued` allows that second reading, and
    is only set where the line already began with a `test <name> ... ` prefix.
    """
    rest = rest.strip()
    if not rest:
        return None
    for token, status in STATUSES:
        if rest.startswith(token):
            return status
    if glued:
        for token, status in STATUSES:
            if rest.endswith(token):
                return status
    return None


class Unit:
    """One test executable's run: `-p leviculum-std --test mvr` and its names."""

    def __init__(self, selector: str, descriptor: str) -> None:
        self.selector = selector
        self.descriptor = descriptor
        self.ok: list[str] = []
        self.failed: list[str] = []
        self.ignored: list[str] = []
        self.unresolved: list[str] = []
        self.summary: dict[str, int] | None = None

    def record(self, name: str, status: str) -> None:
        getattr(self, status).append(name)

    def as_json(self) -> dict:
        return {
            "selector": self.selector,
            "descriptor": self.descriptor,
            # Executed = ok + failed. A failing test ran; an ignored one did
            # not. Kept as three lists rather than one, so step 2 can join on
            # the union without losing why a name is in it.
            "ok": sorted(self.ok),
            "failed": sorted(self.failed),
            "ignored": sorted(self.ignored),
            "unresolved": sorted(self.unresolved),
            "summary": self.summary,
        }

    def reconcile(self) -> str | None:
        """Compare the parsed names against libtest's own summary line."""
        if self.summary is None:
            return (
                f"{self.selector}: libtest printed no `test result:` summary. "
                f"The unit did not finish, or the output format changed."
            )
        got = (len(self.ok), len(self.failed), len(self.ignored))
        want = (
            self.summary["passed"] + self.summary["measured"],
            self.summary["failed"],
            self.summary["ignored"],
        )
        if got != want:
            return (
                f"{self.selector}: parsed {got[0]} passed / {got[1]} failed / "
                f"{got[2]} ignored, libtest reported {want[0]} / {want[1]} / "
                f"{want[2]}."
            )
        if self.unresolved:
            return (
                f"{self.selector}: {len(self.unresolved)} test(s) whose result "
                f"was never printed: {', '.join(sorted(self.unresolved)[:5])}"
            )
        return None


def selector_for(pkg: str, kinds: list[str], name: str) -> str:
    """The cargo selector that runs one test unit.

    Byte-identical in behaviour to selector_for() in
    scripts/check-ignored-counts.py: step 2 joins the two on this string, so
    the two must not drift. Change one, change the other.
    """
    for kind, flag in (("lib", "--lib"), ("proc-macro", "--lib")):
        if kind in kinds:
            return f"-p {pkg} {flag}"
    for kind in ("test", "bin", "example", "bench"):
        if kind in kinds:
            return f"-p {pkg} --{kind} {name}"
    return f"-p {pkg} --{kinds[0]} {name}"


def target_index() -> tuple[dict[tuple[str, str], str], dict[str, str]]:
    """Two lookups built from `cargo metadata`, no build required.

    * (source path relative to the package, underscored target name) -> selector.
      Both halves are needed: `src/lib.rs` repeats in every package, and
      leviculum-proxy's lib and bin targets share the executable stem
      `lora_proxy`. The pair is unique across this workspace for every
      testable target (only build scripts collide, and they produce no tests).
    * underscored lib target name -> package, for `Doc-tests <crate>`, whose
      header names the crate and not the package (`lora_proxy`, not
      `leviculum-proxy`).
    """
    proc = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        return {}, {}
    meta = json.loads(proc.stdout)
    by_src: dict[tuple[str, str], str] = {}
    by_crate: dict[str, str] = {}
    for pkg in meta["packages"]:
        pkg_dir = Path(pkg["manifest_path"]).parent
        for target in pkg["targets"]:
            kinds = target["kind"]
            if "custom-build" in kinds:
                continue
            try:
                rel = str(Path(target["src_path"]).relative_to(pkg_dir))
            except ValueError:
                rel = target["src_path"]
            stem = target["name"].replace("-", "_")
            by_src[(rel, stem)] = selector_for(pkg["name"], kinds, target["name"])
            # Any library kind can carry doc-tests, not just `lib`:
            # leviculum-ffi's is [cdylib, staticlib, rlib] and its header
            # reads `Doc-tests leviculum`.
            if {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"} & set(kinds):
                by_crate[stem] = pkg["name"]
    return by_src, by_crate


class Parser:
    """Streaming libtest/cargo output parser. One instance per run."""

    def __init__(self, by_src: dict[tuple[str, str], str], by_crate: dict[str, str]):
        self.by_src = by_src
        self.by_crate = by_crate
        self.units: list[Unit] = []
        self.current: Unit | None = None
        self.pending: str | None = None
        self.warnings: list[str] = []

    def _unit(self, selector: str, descriptor: str) -> None:
        self.current = Unit(selector, descriptor)
        self.units.append(self.current)
        self.pending = None

    def _from_running(self, desc: str, exe: str) -> None:
        # `unittests src/lib.rs` / `tests/mvr/main.rs`
        src = desc.split(" ", 1)[1] if desc.startswith("unittests ") else desc
        stem = re.sub(r"-[0-9a-f]{6,}$", "", Path(exe).name)
        selector = self.by_src.get((src, stem))
        if selector is None:
            selector = f"unresolved {src} ({stem})"
            self.warnings.append(
                f"could not map `{desc}` ({stem}) to a cargo selector; recorded "
                f"as `{selector}`"
            )
        self._unit(selector, desc)

    def feed(self, raw: str) -> None:
        line = ANSI.sub("", raw).rstrip("\n")

        m = CARGO_RUNNING.match(line)
        if m:
            self._from_running(m.group("desc"), m.group("exe"))
            return
        m = CARGO_DOCTESTS.match(line)
        if m:
            crate = m.group("crate")
            pkg = self.by_crate.get(crate)
            selector = f"-p {pkg} --doc" if pkg else f"unresolved doc {crate}"
            if pkg is None:
                self.warnings.append(f"unknown doc-test crate `{crate}`")
            self._unit(selector, f"Doc-tests {crate}")
            return

        if self.current is None:
            if not RESULT_LINE.match(line):
                return
            # A test binary invoked directly, with no cargo header above it.
            self.warnings.append(
                "test results appeared before any `Running`/`Doc-tests` header; "
                "recorded under `unattributed`"
            )
            self._unit("unattributed", "no cargo header")

        if RUNNING_LINE.match(line):  # `running 12 tests`, the unit's header
            return

        m = SUMMARY_LINE.match(line)
        if m:
            self._resolve_pending(line)
            self.current.summary = {k: int(v) for k, v in m.groupdict().items()}
            return

        m = RESULT_LINE.match(line)
        if m:
            name = DECORATION.sub("", m.group("name"))
            rest = m.group("rest")
            if self.pending is not None:
                # The previous test's status never arrived on its own line.
                self.current.unresolved.append(self.pending)
                self.pending = None
            status = classify(rest, glued=True)
            if status is None:
                self.pending = name
            else:
                self.current.record(name, status)
            return

        self._resolve_pending(line)

    def _resolve_pending(self, line: str) -> None:
        """A held-back `test <name> ... ` whose status arrives on a later line.

        Only reachable under `--test-threads=1 --nocapture`, where the test's
        own output lands between the two halves of its result line.
        """
        if self.pending is None or self.current is None:
            return
        status = classify(line)
        if status is not None:
            self.current.record(self.pending, status)
            self.pending = None

    def finish(self) -> None:
        if self.pending is not None and self.current is not None:
            self.current.unresolved.append(self.pending)
            self.pending = None


# --- running the command, and surviving what it leaves behind ---------------
#
# Three properties, none of which is "the tests stop leaking" -- that is a bug
# per leaking test, and this is the wrapper that must not be hostage to any of
# them. See the module docstring for the incident.
#
# 1. THE VERDICT WAITS ON THE CHILD, NOT ON THE PIPE. proc.wait() runs on the
#    main thread; a reader thread drains stdout. Nothing this script decides
#    depends on EOF ever arriving. That property alone ends the two hours, and
#    it is the one that still holds when everything below fails.
# 2. THE ORPHANS DIE WITH THE GATE. start_new_session=True makes the child lead
#    its own process group, and the whole group is killed once the child has
#    exited. The pipe then closes on its own, so the tail of the output is
#    drained normally rather than abandoned, and nothing is left running.
# 3. A HARD TIMEOUT THAT REPORTS. Named gate, waited duration, and what was
#    still alive; the log is already flushed per line, so it is on disk.
#
# What no property covers: an orphan that calls setsid() has left the group and
# survives the kill. It cannot hold the gate open -- property 1 does not care
# -- but it is still alive afterwards and the reader thread has to be
# abandoned. Both facts are reported rather than swallowed.

# Per wrapped command, not per tier. Chosen from what the manifests on this
# host actually recorded for an honest run: the longest is
# `workspace-all-targets` at 493 s, then `rnsd-interop` at 352 s and
# `status-parity` at 169 s. 1800 s is ~3.6x the longest honest run measured,
# which leaves room for a cold CARGO_TARGET_DIR (the first wrapped command in a
# fresh tree carries the workspace build; `just standard` cold is 20-40 min for
# the whole tier) without leaving room for a two-hour hang. It is also half the
# only backstop anyone had already accepted -- the nightly's `timeout 3600`
# around all of `just complete` -- so a per-command budget cannot swallow the
# per-tier one. Raise it for one gate with --timeout, for all of them with
# LEVICULUM_GATE_TIMEOUT; 0 disables, which is what a by-hand soak wants.
DEFAULT_TIMEOUT_S = 1800.0
# `timeout(1)`'s code for "the command was still running when the budget ran
# out". Borrowed rather than invented so a caller that already knows one number
# does not have to learn a second.
TIMEOUT_EXIT = 124
# Long enough for a daemon to put down a socket, a tempdir or (on the rig) a
# serial port; short enough that a verdict already decided is not held up by
# the bug that is being reported. Only ever paid when something did leak.
KILL_GRACE_S = 2.0
# After the group is gone the pipe's last write end is closed, so EOF is
# immediate. This is the allowance for the case where it is not -- an escaped
# setsid() orphan -- after which the reader is abandoned and said to be.
DRAIN_GRACE_S = 5.0


class Outcome:
    """How one wrapped command ended, beyond its exit status."""

    def __init__(self) -> None:
        self.rc: int | None = None
        self.timed_out = False
        self.timeout_s: float | None = None
        self.duration_s = 0.0
        # Alive in the child's process group after the child itself was gone
        # (or, on a timeout, alive when the budget ran out). Named, not counted.
        self.survivors: list[tuple[int, str]] = []
        self.stubborn: list[tuple[int, str]] = []  # still there after SIGKILL
        self.reader_stuck = False  # EOF never came; the tail of the log is lost

    def clean(self) -> bool:
        return not (self.timed_out or self.survivors or self.stubborn or self.reader_stuck)


def proc_snapshot() -> list[tuple[int, int, str, str]]:
    """(pid, pgid, state, command) for every process /proc will show us.

    Linux-specific, and naming is the whole point of it: "killed 1 survivor" is
    a workaround, "killed PID 960389 scripts/test_daemon.py --rns-port 43153"
    is a bug report against the test that leaked it. On a kernel without /proc
    the killing still works and only the names are missing.
    """
    procfs = Path("/proc")
    if not procfs.is_dir():
        return []
    me = os.getpid()
    rows = []
    for entry in procfs.iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        if pid == me:
            continue
        try:
            stat = (entry / "stat").read_text()
            # comm sits in parens and may itself contain spaces and parens, so
            # the fields after it are positional from the LAST ')': state,
            # ppid, pgrp.
            head, _, tail = stat.rpartition(")")
            fields = tail.split()
            state, pgid = fields[0], int(fields[2])
            comm = head.partition("(")[2]
            cmdline = (entry / "cmdline").read_bytes()
        except (OSError, ValueError, IndexError):
            continue  # it exited between the readdir and the read
        cmd = " ".join(cmdline.decode("utf-8", "replace").rstrip("\0").split("\0"))
        rows.append((pid, pgid, state, cmd or f"[{comm}]"))
    return sorted(rows)


def group_members(pgid: int) -> list[tuple[int, str]]:
    """Live, non-zombie members of process group `pgid`, as (pid, command).

    Zombies are skipped: an exited process holds no file descriptor, so it can
    neither hold the pipe open nor be killed again.
    """
    return [(pid, cmd) for pid, gid, state, cmd in proc_snapshot() if gid == pgid and state != "Z"]


def signal_group(pgid: int, sig: int) -> None:
    try:
        os.killpg(pgid, sig)
    except (ProcessLookupError, PermissionError, OSError):
        pass


def wait_for_empty(pgid: int, grace_s: float) -> bool:
    deadline = time.monotonic() + grace_s
    while time.monotonic() < deadline:
        if not group_members(pgid):
            return True
        time.sleep(0.05)
    return not group_members(pgid)


def reap_group(pgid: int, grace_s: float = KILL_GRACE_S) -> tuple[list, list]:
    """Kill what is left of a process group. Returns (killed, stubborn).

    SIGTERM then SIGKILL rather than a plain kill, and the extra step is worth
    it here: what leaks in this repo is daemons -- test_daemon.py, lnsd, rnsd --
    which hold a listening socket, a tempdir and sometimes a serial port, and a
    SIGTERM lets them put those down. The cost is bounded and conditional: when
    nothing leaked the member list is empty and this returns without signalling
    anything, so the ordinary green run pays one /proc scan.
    """
    alive = group_members(pgid)
    if not alive:
        return [], []
    signal_group(pgid, signal.SIGTERM)
    if wait_for_empty(pgid, grace_s):
        return alive, []
    signal_group(pgid, signal.SIGKILL)
    wait_for_empty(pgid, 1.0)
    return alive, group_members(pgid)


def forward_signals(pgid: int):
    """Relay INT/TERM/HUP to the child's group; returns a restore callable.

    start_new_session detaches the child from the terminal's foreground process
    group, so a Ctrl-C that used to reach cargo directly now reaches only this
    wrapper. Without the relay, interrupting a gate would leave a whole cargo
    tree running -- trading a hang at the end for an escape at the start.
    """
    previous: dict[int, object] = {}

    def relay(signum, _frame):
        signal_group(pgid, signum)

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        try:
            previous[sig] = signal.signal(sig, relay)
        except (ValueError, OSError):  # not the main thread, or no such signal
            pass

    def restore() -> None:
        for sig, handler in previous.items():
            try:
                signal.signal(sig, handler)
            except (ValueError, OSError):
                pass

    return restore


def run_command(
    command: list[str],
    *,
    cwd: str,
    env: dict,
    on_line,
    timeout_s: float | None,
    on_spawn=None,
    kill_grace_s: float = KILL_GRACE_S,
    drain_grace_s: float = DRAIN_GRACE_S,
) -> Outcome:
    """Run `command`, hand every output line to `on_line`, and always return.

    `on_spawn` is handed the Popen the moment it exists; reaping_canary() uses
    it to arm its own watchdog, and nothing in the real path needs it.
    """
    outcome = Outcome()
    outcome.timeout_s = timeout_s
    started = time.monotonic()

    proc = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        errors="replace",
        # Property 2: its own session, so its own process group, so one
        # os.killpg reaches every descendant that did not deliberately leave.
        start_new_session=True,
    )
    assert proc.stdout is not None
    # start_new_session makes the child a session and group leader, so its pgid
    # IS its pid. Deliberately not read back with os.getpgid(): between the fork
    # and the child's setsid() the parent would observe its OWN group, and a
    # killpg on that number is the one mistake here that is worse than the hang.
    # If start_new_session had somehow not taken, this group is empty and the
    # reap below finds nothing to kill, which is the safe direction to fail in.
    pgid = proc.pid
    abandoned = threading.Event()

    def pump() -> None:
        try:
            for raw in proc.stdout:
                on_line(raw)
        except Exception:
            # Once abandoned, the log this writes to is closed under it and the
            # descriptor may be too. That is the expected end of an abandoned
            # reader, not a failure to report.
            if not abandoned.is_set():
                raise

    reader = threading.Thread(target=pump, name="gate-output", daemon=True)
    reader.start()

    restore = forward_signals(pgid)
    if on_spawn is not None:
        on_spawn(proc)
    try:
        # Property 1. Everything this script decides hangs off this line, and
        # this line cannot be held open by anything the child leaked.
        try:
            outcome.rc = proc.wait(timeout=timeout_s if timeout_s else None)
        except subprocess.TimeoutExpired:
            outcome.timed_out = True
    finally:
        restore()

    # The child is done deciding, one way or the other, so anything still in
    # its group is either the timed-out child itself or something a test
    # leaked. Both are named before they are killed (property 3).
    outcome.survivors, outcome.stubborn = reap_group(pgid, kill_grace_s)
    if outcome.timed_out:
        try:
            outcome.rc = proc.wait(timeout=kill_grace_s + 1.0)
        except subprocess.TimeoutExpired:
            outcome.rc = None

    reader.join(drain_grace_s)
    if reader.is_alive():
        # Nothing in the group holds the pipe any more, so a reader still
        # blocked is waiting on a write end that escaped the group with its own
        # setsid(). It is ABANDONED here, not interrupted: proc.stdout.close()
        # from this thread would block on the very lock the reader holds while
        # inside read(), which is the same hang one level down -- measured, on
        # 2026-08-07, while building this. The thread is a daemon, so it costs
        # the interpreter nothing at exit, and `abandoned` tells it to die
        # quietly when the log closes under it.
        abandoned.set()
        outcome.reader_stuck = True
    else:
        try:
            proc.stdout.close()
        except OSError:
            pass

    outcome.duration_s = time.monotonic() - started
    return outcome


def outcome_report(gate: str, outcome: Outcome) -> list[str]:
    """The lines a gate must print about how it ended. Empty when it ended well.

    Property 3 in full: a gate that cleans up silently hides the bug it just
    worked around, so every kill is attributed to a pid and a command line.
    """
    lines: list[str] = []
    if outcome.timed_out:
        lines.append(
            f"[manifest] TIMED OUT: gate `{gate}` exceeded its "
            f"{outcome.timeout_s:.0f}s budget and was killed after waiting "
            f"{outcome.duration_s:.0f}s."
        )
        lines.append(
            "[manifest]   This is a named failure, not a verdict about the "
            "tests: the gate gave up."
        )
    if outcome.survivors:
        what = "still alive when the budget ran out" if outcome.timed_out else (
            "still alive after the gate's own process had exited"
        )
        lines.append(f"[manifest] KILLED {len(outcome.survivors)} process(es) {what}:")
        for pid, cmd in outcome.survivors:
            lines.append(f"[manifest]   PID {pid}: {cmd}")
        if not outcome.timed_out:
            lines.append(
                "[manifest]   A leaked process is a bug in whatever spawned it, "
                "not a quirk of this wrapper."
            )
            lines.append(
                "[manifest]   It had inherited this gate's stdout, so before "
                "2026-08-07 it would have"
            )
            lines.append(
                "[manifest]   held the gate open forever instead of letting it "
                "report. File it."
            )
    if outcome.stubborn:
        lines.append(
            f"[manifest] {len(outcome.stubborn)} process(es) survived SIGKILL "
            "(uninterruptible, or not ours):"
        )
        for pid, cmd in outcome.stubborn:
            lines.append(f"[manifest]   PID {pid}: {cmd}")
    if outcome.reader_stuck:
        lines.append(
            "[manifest] The output pipe never reached EOF even with the child's "
            "process group gone."
        )
        lines.append(
            "[manifest]   Something escaped the group (setsid) and still holds "
            "a write end. The"
        )
        lines.append(
            "[manifest]   verdict below is correct; the last lines of the log "
            "may be missing."
        )
    return lines


# --- standing canary: the parser --------------------------------------------
#
# Runs before the real command, every invocation. Two halves, as
# docs/src/concepts/checks-and-citations.md §Standing canaries requires: a test
# that must always show up in a manifest, and tests that must never.

CANARY_LINES = [
    "     Running tests/canary.rs (target/debug/deps/canary-0123456789abcdef)",
    "",
    "running 5 tests",
    "test canary::must_appear_ok ... ok",
    "test canary::must_appear_should_panic - should panic ... ok",
    "test canary::must_appear_failed ... FAILED",
    "test canary::must_never_appear_ignored ... ignored, no hardware on this bench",
    "    test canary::must_never_appear_indented ... ok",
    "note: test canary::must_never_appear_prose ... ok",
    "test canary::must_appear_split ... ",
    "some output the test printed in --nocapture mode",
    "ok",
    "",
    # Deliberately miscounted: three tests passed above, this claims five. The
    # reconciler must reject it, and must accept the corrected counts below.
    "test result: FAILED. 5 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; "
    "finished in 0.01s",
]

CANARY_MUST_APPEAR = {
    "canary::must_appear_ok",
    "canary::must_appear_split",
    # Under its `--list` name, not libtest's decorated one.
    "canary::must_appear_should_panic",
}
CANARY_MUST_NOT_APPEAR_DECORATED = "canary::must_appear_should_panic - should panic"
CANARY_MUST_NOT_APPEAR = {
    "canary::must_never_appear_ignored",
    "canary::must_never_appear_indented",
    "canary::must_never_appear_prose",
}
CANARY_TRUE_SUMMARY = {
    "passed": 3,
    "failed": 1,
    "ignored": 1,
    "measured": 0,
    "filtered": 0,
}


def canary() -> bool:
    """True if the parser still sees what it exists to see."""
    parser = Parser({("tests/canary.rs", "canary"): "-p canary --test canary"}, {})
    for line in CANARY_LINES:
        parser.feed(line)
    parser.finish()

    def fail(msg: str) -> bool:
        sys.stderr.write(f"[manifest] CANARY FAILED -- {msg}\n")
        sys.stderr.write(
            "[manifest]   The manifest writer cannot see what it exists to record.\n"
            "[manifest]   libtest's output format changed under the parser, or the\n"
            "[manifest]   parser was edited. Fix it; do not skip this check: a\n"
            "[manifest]   writer that silently stops writing is green forever.\n"
        )
        return False

    if len(parser.units) != 1:
        return fail(f"expected 1 unit from the fixture, parsed {len(parser.units)}")
    unit = parser.units[0]
    if unit.selector != "-p canary --test canary":
        return fail(f"the `Running` line no longer resolves: {unit.selector!r}")

    executed = set(unit.ok) | set(unit.failed)
    missing = CANARY_MUST_APPEAR - executed
    if missing:
        return fail(f"tests that must always be recorded were not: {sorted(missing)}")
    if "canary::must_appear_failed" not in unit.failed:
        return fail("a FAILED test was not recorded as executed-and-failed")
    if CANARY_MUST_NOT_APPEAR_DECORATED in executed:
        return fail(
            "a #[should_panic] test was recorded under libtest's decorated name, "
            "which no `--list` enumeration will ever match"
        )
    present = CANARY_MUST_NOT_APPEAR & executed
    if present:
        return fail(f"tests that must never be recorded were: {sorted(present)}")
    if "canary::must_never_appear_ignored" not in unit.ignored:
        return fail("an ignored test was not recorded as ignored")

    # The reconciler is the other half of the guarantee: it is what makes "the
    # manifest matches what actually ran" mechanical rather than asserted. The
    # fixture's summary claims four passing tests where two are in it, so a
    # working reconciler must reject it -- and must accept the true counts.
    if unit.reconcile() is None:
        return fail("the reconciler accepted a unit whose summary disagrees with it")
    unit.summary = dict(CANARY_TRUE_SUMMARY)
    if (problem := unit.reconcile()) is not None:
        return fail(f"the reconciler rejected a consistent unit: {problem}")
    return True


# --- standing canary: the reaping wrapper -----------------------------------
#
# Also on every invocation. The parser canary above cannot see this failure at
# all -- a wrapper that hangs has parsed everything correctly -- and the shape
# of the defect is exactly the shape §Standing canaries exists for: a gate that
# stops terminating reports nothing forever, and nothing else notices.

# How long the canary's own watchdog waits before forcing the pipe shut. Two
# orders of magnitude above what a passing arm takes (~0.1 s) and two orders
# below the hang it exists to catch. A regression costs the suite this, once.
CANARY_BOUND_S = 20.0
# Not 0 and not 1: an exit status that no interpreter, signal or shell produces
# by accident, so "the child's status was passed through" cannot pass by luck.
CANARY_CHILD_EXIT = 42
# Long enough that the grandchild is certainly still holding the pipe when the
# child exits, and irrelevant afterwards because it gets killed.
CANARY_GRANDCHILD_SLEEP_S = 300

# The defect, reproduced in nine lines: a child that spawns a grandchild which
# inherits the gate's stdout, then exits without killing it. This is what an
# aborted Rust test does when SIGABRT skips the Drop that would have killed its
# daemon; the wrapper cannot tell the two apart and must not need to.
CANARY_LEAK_SOURCE = """
import subprocess, sys
subprocess.Popen([sys.executable, "-c", "import time; time.sleep({sleep})  # {marker}"])
print("canary: leaked a grandchild holding this pipe, now exiting", flush=True)
sys.exit({code})
"""
CANARY_CLEAN_SOURCE = """
import sys
print("canary: leaked nothing", flush=True)
sys.exit(0)
"""
CANARY_HANG_SOURCE = """
import sys, time
print("canary: never exiting", flush=True)
time.sleep({sleep})
"""
# The degradation path. This orphan calls setsid() too, so it leaves the group
# and the reap cannot reach it: it goes on holding the pipe after the gate has
# its verdict. The gate must report anyway, on time, and say the tail of the
# log may be missing. Nothing here can make that orphan die -- the property is
# that the gate stops depending on it.
CANARY_ESCAPEE_SOURCE = """
import subprocess, sys
subprocess.Popen([sys.executable, "-c", "import time; time.sleep({sleep})  # {marker}"],
                 start_new_session=True)
print("canary: leaked an orphan that escaped the process group", flush=True)
sys.exit({code})
"""


def kill_by_marker(marker: str) -> None:
    """SIGKILL every process whose argv carries `marker`.

    The canary's watchdog, and deliberately not a process-group kill: the arm
    that matters runs against a wrapper that may have regressed to leaving the
    child in OUR group, where a killpg would take this script with it. A marker
    unique to this invocation is the only handle that is correct in both worlds.
    """
    for pid, _pgid, _state, cmd in proc_snapshot():
        if marker in cmd:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass


def canary_arm(
    source: str,
    *,
    timeout_s: float | None,
    marker: str | None,
    drain_grace_s: float = 2.0,
):
    """Run one canary child under a watchdog. Returns (outcome, lines, tripped).

    `tripped` is the whole point: it is True when the watchdog had to force the
    pipe shut, which is precisely the failure -- the wrapper did not terminate
    on its own and, on the pre-2026-08-07 code, would have waited forever.
    """
    lines: list[str] = []
    tripped = threading.Event()
    timers: list[threading.Timer] = []

    def on_spawn(proc: subprocess.Popen) -> None:
        def trip() -> None:
            tripped.set()
            if marker:
                kill_by_marker(marker)
            try:
                proc.kill()
            except OSError:
                pass

        timer = threading.Timer(CANARY_BOUND_S, trip)
        timer.daemon = True
        timer.start()
        timers.append(timer)

    try:
        outcome = run_command(
            [sys.executable, "-c", source],
            cwd=str(ROOT),
            env=dict(os.environ),
            on_line=lines.append,
            timeout_s=timeout_s,
            on_spawn=on_spawn,
            kill_grace_s=0.5,
            drain_grace_s=drain_grace_s,
        )
    finally:
        for timer in timers:
            timer.cancel()
        if marker:
            kill_by_marker(marker)  # belt: an arm that raised leaves nothing behind
    return outcome, lines, tripped.is_set()


def reaping_canary() -> bool:
    """True if a leaked grandchild still cannot hold this wrapper open.

    Three arms, and the pair the concept page asks for is the first two: a run
    that must report a kill and a run that must report none. Without the
    negative arm, a wrapper that killed and blamed something on every green run
    would print noise that everybody learns to skip, which is the same defect
    as printing nothing.

    BOUNDED FROM OUTSIDE THE THING UNDER TEST. The watchdog is a timer in this
    process that kills the grandchild by an argv marker, so the pipe closes by
    force whatever run_command() does or does not do. A regression therefore
    fails here in CANARY_BOUND_S seconds; it does not wedge the suite for two
    hours the way the incident wedged the run. Walking into that trap while
    fixing it would be poor form.
    """

    def fail(msg: str) -> bool:
        sys.stderr.write(f"[manifest] REAPING CANARY FAILED -- {msg}\n")
        sys.stderr.write(
            "[manifest]   A gate must pass, fail, or say it gave up. This checks\n"
            "[manifest]   that a process a test leaked cannot hold it in a fourth\n"
            "[manifest]   state instead -- alive, verdict decided, nobody told.\n"
            "[manifest]   Fix run_command() in scripts/run-with-manifest.py; do\n"
            "[manifest]   not skip it, because a wrapper that stops terminating\n"
            "[manifest]   reports nothing forever and nothing else notices.\n"
        )
        return False

    named = Path("/proc").is_dir()  # elsewhere we can kill but not name

    # Arm 1: the incident. A grandchild holds the pipe after the child is gone.
    marker = f"leviculum-canary-{os.getpid()}-{os.urandom(4).hex()}"
    outcome, lines, tripped = canary_arm(
        CANARY_LEAK_SOURCE.format(
            sleep=CANARY_GRANDCHILD_SLEEP_S, marker=marker, code=CANARY_CHILD_EXIT
        ),
        timeout_s=CANARY_BOUND_S * 2,  # must never be what ends this arm
        marker=marker,
    )
    if tripped:
        return fail(
            "the wrapper did not return after its child exited; the canary's own "
            "watchdog had to kill the leaked grandchild to unblock it. This is "
            "the two-hour hang of 2026-08-07, caught in "
            f"{CANARY_BOUND_S:.0f}s."
        )
    if outcome.timed_out:
        return fail("the leak arm hit the hard timeout instead of the child's exit")
    if outcome.rc != CANARY_CHILD_EXIT:
        return fail(
            f"the child's exit status was not passed through: got {outcome.rc}, "
            f"want {CANARY_CHILD_EXIT}"
        )
    if not any("leaked a grandchild" in line for line in lines):
        return fail("the child's output never reached the reader")
    if not outcome.survivors:
        return fail(
            "a leaked grandchild was not reported as killed. The gate terminated, "
            "but silently cleaning up hides the bug in the test that leaked"
        )
    if named and not any(marker in cmd for _pid, cmd in outcome.survivors):
        return fail(
            f"the survivor was reported without naming it: {outcome.survivors}. "
            "A count is a workaround; a pid and a command line is a bug report"
        )
    if outcome.stubborn:
        return fail(f"the leaked grandchild survived SIGKILL: {outcome.stubborn}")

    # Arm 2: the negative control. Nothing leaked, so nothing may be blamed.
    outcome, _lines, tripped = canary_arm(
        CANARY_CLEAN_SOURCE, timeout_s=CANARY_BOUND_S * 2, marker=None
    )
    if tripped or outcome.rc != 0:
        return fail(f"a child that leaked nothing did not exit cleanly: rc={outcome.rc}")
    if not outcome.clean():
        return fail(
            f"a clean run reported survivors {outcome.survivors} / stubborn "
            f"{outcome.stubborn}. A kill reported every run is noise, and noise "
            "is how a real one goes unread"
        )

    # Arm 3: the backstop. A child that never exits must produce a named
    # failure, not a wait. Short budget on purpose -- the property under test is
    # that the budget is enforced at all, and it costs the canary a quarter of
    # a second to prove it.
    outcome, _lines, tripped = canary_arm(
        CANARY_HANG_SOURCE.format(sleep=CANARY_GRANDCHILD_SLEEP_S),
        timeout_s=0.25,
        marker=None,
    )
    if tripped:
        return fail("the hard timeout did not fire; the canary's watchdog ended the arm")
    if not outcome.timed_out:
        return fail(f"a child that never exits was not timed out (rc={outcome.rc})")
    if not outcome_report("canary", outcome):
        return fail("a timed-out gate produced no report of what happened")
    if outcome.stubborn:
        return fail(f"the timed-out child survived: {outcome.stubborn}")

    # Arm 4: the degradation. An orphan with its own session cannot be reaped,
    # so it holds the pipe for as long as it likes. The gate must report within
    # its drain grace regardless, and must say the tail may be missing.
    #
    # This arm exists because the first attempt at the fix failed it: closing
    # the read end to interrupt the reader blocks on the lock the reader holds
    # inside read(), which is the original hang one level down. Nothing else
    # here would have caught that -- arms 1 to 3 all pass with it.
    marker = f"leviculum-canary-escapee-{os.getpid()}-{os.urandom(4).hex()}"
    grace = 0.5
    outcome, lines, tripped = canary_arm(
        CANARY_ESCAPEE_SOURCE.format(
            sleep=CANARY_GRANDCHILD_SLEEP_S, marker=marker, code=CANARY_CHILD_EXIT
        ),
        timeout_s=CANARY_BOUND_S * 2,
        marker=marker,
        drain_grace_s=grace,
    )
    if tripped:
        return fail(
            "an orphan that escaped the process group held the gate open; the "
            "canary's watchdog had to kill it. The reap is best-effort, but the "
            "verdict must not depend on it"
        )
    if outcome.rc != CANARY_CHILD_EXIT:
        return fail(
            f"the child's exit status was lost behind an escaped orphan: got "
            f"{outcome.rc}, want {CANARY_CHILD_EXIT}"
        )
    if outcome.duration_s > grace + CANARY_BOUND_S / 4:
        return fail(
            f"the gate took {outcome.duration_s:.1f}s to report behind an escaped "
            f"orphan, with a drain grace of {grace}s. It is waiting on the pipe "
            "again"
        )
    if not outcome.reader_stuck:
        return fail(
            "an escaped orphan still held the pipe, but the gate did not say the "
            "tail of its log may be missing"
        )
    if not any("escaped the process group" in line for line in lines):
        return fail("the child's output never reached the reader")
    return True


# --- manifest ---------------------------------------------------------------


def manifest_dir() -> Path:
    override = os.environ.get("LEVICULUM_MANIFEST_DIR")
    if override:
        return Path(override)
    state = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local" / "state")
    slug = f"{ROOT.name}-{hashlib.sha1(str(ROOT).encode()).hexdigest()[:8]}"
    return Path(state) / "leviculum-ci" / "test-manifests" / slug


def git(*args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(ROOT), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    return proc.stdout.strip() if proc.returncode == 0 else ""


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Run a test command and record the tests it executed."
    )
    ap.add_argument("--gate", required=True, help="manifest name, e.g. mvr")
    ap.add_argument(
        "--timeout",
        type=float,
        default=None,
        metavar="SECONDS",
        help=(
            f"budget for this gate (default {DEFAULT_TIMEOUT_S:.0f}s, or "
            "$LEVICULUM_GATE_TIMEOUT; 0 disables)"
        ),
    )
    ap.add_argument(
        "--no-tests",
        action="store_true",
        help=(
            "the wrapped command compiles or checks but executes no tests "
            "(e.g. `cargo check`); zero executed tests is the expected "
            "outcome, and executing any is a failure"
        ),
    )
    ap.add_argument("command", nargs=argparse.REMAINDER)
    args = ap.parse_args()

    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        ap.error("no command given (use `--gate <name> -- <command>`)")
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", args.gate):
        ap.error(f"gate name {args.gate!r} must be lowercase kebab-case")

    # Per-gate flag beats the global env var beats the default. The env var is
    # what a soak or a cold-cache first run reaches for; the flag is what the
    # Justfile uses when one gate is honestly slower than the rest.
    timeout_s = args.timeout
    if timeout_s is None:
        raw = os.environ.get("LEVICULUM_GATE_TIMEOUT")
        if raw:
            try:
                timeout_s = float(raw)
            except ValueError:
                ap.error(f"LEVICULUM_GATE_TIMEOUT={raw!r} is not a number of seconds")
        else:
            timeout_s = DEFAULT_TIMEOUT_S
    if timeout_s < 0:
        ap.error("--timeout must not be negative (0 disables the budget)")

    if not canary():
        return 1
    if not reaping_canary():
        return 1

    by_src, by_crate = target_index()
    parser = Parser(by_src, by_crate)

    env = dict(os.environ)
    if sys.stdout.isatty():
        # The command's output goes through a pipe now, so cargo would drop
        # colour that the unwrapped gate had. ANSI is stripped before parsing.
        env.setdefault("CARGO_TERM_COLOR", "always")

    out_dir = manifest_dir()
    out_dir.mkdir(parents=True, exist_ok=True)
    # Manifest tmp files are per-invocation (see the write below), so one
    # left lying around belongs to a crashed or killed run. Sweep this
    # gate's leftovers once they are clearly stale; a fresh one may belong
    # to a live concurrent invocation and stays.
    for stale in out_dir.glob(f"{args.gate}.json.*.tmp"):
        try:
            if time.time() - stale.stat().st_mtime > 86400:
                stale.unlink()
        except OSError:
            pass  # gone already, or racing with its owner: both fine
    log_path = out_dir / f"{args.gate}.log"
    failed_log_path = out_dir / f"{args.gate}.failed.log"

    started = time.time()
    # Line-buffered and flushed per line: a run that is killed, times out or
    # panics its way out still leaves everything it had printed on disk. That
    # is what makes the timeout's "and here is the partial log" true for free.
    with open(log_path, "w", errors="replace") as log:
        log.write(f"$ {shlex.join(command)}\n")
        log.flush()

        def on_line(raw: str) -> None:
            sys.stdout.write(raw)
            sys.stdout.flush()
            log.write(raw)
            log.flush()
            parser.feed(raw)

        outcome = run_command(
            command,
            cwd=os.getcwd(),
            env=env,
            on_line=on_line,
            timeout_s=timeout_s,
        )
        # How the run ended goes into the log as well as onto stderr. A nightly
        # keeps the log and throws the terminal away, and "which gate gave up,
        # after how long, with what still alive" is the part it must keep.
        report = outcome_report(args.gate, outcome)
        for line in report:
            sys.stderr.write(line + "\n")
            log.write(line + "\n")
        log.flush()
    parser.finish()
    finished = time.time()

    if outcome.timed_out:
        rc = TIMEOUT_EXIT
    elif outcome.rc is None:
        rc = TIMEOUT_EXIT
    elif outcome.rc < 0:
        # proc.wait() returns -N for "killed by signal N". Passing that to
        # sys.exit() would report SIGABRT as 250; the shell convention every
        # caller here already reads is 128+N.
        rc = 128 - outcome.rc
    else:
        rc = outcome.rc

    if rc != 0:
        # Only a failure overwrites the failure log, so the green runs that
        # follow a red one cannot erase what the red one printed.
        failed_log_path.write_text(log_path.read_text(errors="replace"), errors="replace")

    executed = sum(len(u.ok) + len(u.failed) for u in parser.units)
    for warning in parser.warnings:
        sys.stderr.write(f"[manifest] WARNING: {warning}\n")

    payload = {
        "schema": SCHEMA,
        "gate": args.gate,
        "command": command,
        "repo": str(ROOT),
        "commit": git("rev-parse", "HEAD"),
        "dirty": bool(git("status", "--porcelain")),
        "host": socket.gethostname(),
        "started": datetime.fromtimestamp(started, timezone.utc).isoformat(),
        "finished": datetime.fromtimestamp(finished, timezone.utc).isoformat(),
        "duration_s": round(finished - started, 3),
        "exit_code": rc,
        # A hang leaves no manifest at all, so these three fields are how the
        # NEXT reader of a manifest learns that a gate gave up rather than
        # answered, and what it had to kill to be able to say so.
        "timeout_s": timeout_s or None,
        "timed_out": outcome.timed_out,
        "killed": [{"pid": pid, "command": cmd} for pid, cmd in outcome.survivors],
        "survived_sigkill": [{"pid": pid, "command": cmd} for pid, cmd in outcome.stubborn],
        "executed": executed,
        "log": str(log_path),
        "failed_log": str(failed_log_path) if rc != 0 else None,
        "units": [u.as_json() for u in parser.units],
    }

    out = out_dir / f"{args.gate}.json"
    # Unique per invocation (Codeberg #224): two concurrent runs of the same
    # gate -- a pre-push `just fast` under a running `just standard` -- used
    # to share `<gate>.json.tmp`, one os.replace consumed it and the other
    # crashed a green gate with FileNotFoundError. The final replace is
    # last-writer-wins, which is right for run state: whichever run finished
    # last is the freshest record of this gate.
    tmp = out_dir / f"{args.gate}.json.{os.getpid()}.tmp"
    tmp.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n")
    os.replace(tmp, out)

    print(
        f"[manifest] {args.gate}: {executed} tests executed across "
        f"{len(parser.units)} unit(s) -> {out}"
    )
    print(f"[manifest] full output: {log_path}")

    if rc != 0:
        # The gate failed on its own terms. The manifest of a failed run is
        # still written -- a failing test did execute -- but nothing below is
        # asserted about a run that did not finish.
        failed = sorted(n for u in parser.units for n in u.failed)
        sys.stderr.write(
            f"[manifest] {args.gate}: exit {rc}"
            + (f", failing: {', '.join(failed)}" if failed else "")
            + "\n"
        )
        sys.stderr.write(
            f"[manifest] FULL OUTPUT OF THIS FAILURE: {failed_log_path}\n"
            "[manifest] It is kept until the next failure of this gate, whatever\n"
            "[manifest] the caller did with the stdout above (Codeberg #195).\n"
        )
        return rc

    problems = [p for p in (u.reconcile() for u in parser.units) if p]
    if problems:
        sys.stdout.flush()
        sys.stderr.write(
            "[manifest] FAILED: the manifest does not match what libtest said ran.\n"
        )
        for problem in problems:
            sys.stderr.write(f"[manifest]   {problem}\n")
        sys.stderr.write(
            "[manifest] The command passed; the record of it is wrong, which is\n"
            "[manifest] worse than no record. Fix the parser in "
            "scripts/run-with-manifest.py.\n"
        )
        return 1

    # A gate that runs no tests by design (a compile-only `cargo check`, wrapped
    # for the timeout, the log retention and the reaping) declares that with
    # --no-tests. The declaration is checked in both directions: a declared
    # gate that DOES run tests would carry a manifest step 2 never reads, so
    # its tests would look unrun forever — which is the same defect the
    # executed-zero failure below exists for, mirrored.
    if args.no_tests:
        if executed:
            sys.stdout.flush()
            sys.stderr.write(
                f"[manifest] FAILED: gate `{args.gate}` is declared --no-tests "
                f"but executed {executed} test(s).\n"
                "[manifest] Drop the flag so the executed-count contract covers\n"
                "[manifest] them, or stop running tests under this gate.\n"
            )
            return 1
        return 0

    if executed == 0:
        sys.stdout.flush()
        sys.stderr.write(
            f"[manifest] FAILED: `{' '.join(command)}` exited 0 and executed no "
            f"tests.\n"
            "[manifest] That is the hazard this whole mechanism exists for: a\n"
            "[manifest] by-name selector that matches nothing runs nothing and\n"
            "[manifest] exits 0, so the gate reads green having measured nothing.\n"
            "[manifest] Fix the selector, or do not wrap a command that runs no\n"
            "[manifest] tests.\n"
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
