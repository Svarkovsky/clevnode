#!/usr/bin/env python3
"""Keep a witness on one LNode debug port for the length of a tier-3 run.

Codeberg #353. A T114 on the rig resets itself mid-run: the USB device
disappears for 2-3 s and re-enumerates under the same serial. The board says
why it reset — `[PANIC_COUNT]`, `[HARDFAULT_PMRT]`, `[PANIC_PMRT]`, the
persistent log tail from the boot that died — and until now it said it to
nobody, because nothing was listening on a debug port during a tier-3 run.
Every occurrence has been diagnosed from the outside and closed as "suspected
self-reset".

What this process does, and deliberately does not do
----------------------------------------------------

It does NOT hold the debug port. It watches the port PATH and attaches only
after the path has gone away and come back — that is, only after a reset it
exists to explain.

That is not timidity, it is the only correct behaviour on this rig: periculum
owns the LNode debug port during a scenario. It opens the port for its own
per-scenario `[debug-capture]` log, and its per-scenario setup runs
`reclaim_serial_port`, which `lsof -t`s the port and SIGTERM/SIGKILLs whatever
holds it (periculum/src/runner.rs, `reclaim_serial_port` /
`start_debug_captures`). A witness that held the port continuously would be
killed at every scenario boundary, would split the byte stream with
periculum's own capture while both were open, and would turn periculum's
"leaked holder" warning — a real diagnostic — into per-scenario noise.

Attaching only across a vanish avoids all three, and loses nothing, because
periculum's capture is exactly what fails at a reset: on a read error it
sleeps 500 ms, tries ONE reopen, and `break`s the thread if that fails. The
board is gone for 2-3 s, so that reopen always fails and the capture thread
is dead before the board returns. The gap this process fills is the gap that
kills the evidence.

Getting the evidence off the board
----------------------------------

Attaching after the reset is not enough on its own. The firmware emits its
boot replay once into an 8 KiB log ring, and runtime output laps it before a
late-attaching host can read it (leviculum-nrf/src/lib.rs, `postmortem_query`
says so in as many words). So this sends the firmware's post-mortem query — a
single `p` byte on the debug CDC — which re-emits `[PANIC_COUNT]` and the
stored post-mortem block bracketed by `[PM_QUERY] begin` / `[PM_QUERY] done`.
The records are `peek`ed, not consumed, so asking is free and repeatable
(same mechanism as scripts/lnode-panic-query.sh). It is sent twice, because
the first can land while the CDC endpoint is still settling after
enumeration.

Both sides of the reset end up in the one file: the post-mortem block is what
the board says about the boot that died, and the persistent log tail replayed
with it is that boot's last lines.

DTR and RTS are asserted on open because the LNode debug CDC transmits only
with them raised; a port opened without them is silent, and silence read as
"the board had nothing to say" is the failure this whole file exists to stop.
Pure stdlib (termios/fcntl), no pyserial on the rig.

Usage:
  debug-witness-reader.py --port <path> --out <file> [--label <text>]
                          [--poll <s>] [--capture <s>] [--acquire <s>]

Runs until SIGTERM/SIGINT. Exits 0. Never exits because a port went away:
that is the event, not an error.
"""

import argparse
import fcntl
import os
import select
import signal
import struct
import subprocess
import sys
import termios
import time

RUNNING = True


def _stop(_signum, _frame):
    global RUNNING
    RUNNING = False


def stamp():
    """Local ISO-8601 with offset, so a vanish timestamp from the runner's
    log (also local, also ISO-8601) can be found in here without arithmetic."""
    return time.strftime("%Y-%m-%dT%H:%M:%S%z")


class Witness:
    def __init__(self, args):
        self.args = args
        self.out = open(args.out, "a", buffering=1, encoding="utf-8", errors="replace")

    def note(self, text):
        """A line about the witness itself. `#` so a reader can tell our
        bookkeeping from what the board said."""
        self.out.write(f"{stamp()} # {text}\n")

    def said(self, text):
        self.out.write(f"{stamp()} {text}\n")

    def other_holders(self):
        """PIDs other than ours with the port open, per lsof.

        Only a guard: with the arm-on-vanish rule above, nobody should hold
        the port when we attach. If the board returns fast enough that
        periculum's single reopen attempt succeeds, this keeps us from
        stealing half its bytes.
        """
        try:
            out = subprocess.run(
                ["lsof", "-t", self.args.port],
                capture_output=True,
                text=True,
                timeout=10,
            ).stdout
        except (OSError, subprocess.SubprocessError):
            # No lsof, or it hung. Best-effort: not a reason to stay away
            # from a port that just came back.
            return []
        mine = os.getpid()
        pids = []
        for line in out.split():
            try:
                pid = int(line)
            except ValueError:
                continue
            if pid != mine:
                pids.append(pid)
        return pids

    def open_port(self):
        """Open the debug CDC with DTR+RTS raised, or None."""
        try:
            fd = os.open(self.args.port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        except OSError as exc:
            self.note(f"could not open {self.args.port}: {exc}")
            return None
        try:
            iflag, oflag, cflag, lflag, ispeed, ospeed, cc = termios.tcgetattr(fd)
            iflag = oflag = lflag = 0
            cflag = termios.CLOCAL | termios.CREAD | termios.CS8
            ispeed = ospeed = termios.B115200
            termios.tcsetattr(
                fd, termios.TCSANOW, [iflag, oflag, cflag, lflag, ispeed, ospeed, cc]
            )
            dtr = getattr(termios, "TIOCM_DTR", 0x002)
            rts = getattr(termios, "TIOCM_RTS", 0x004)
            try:
                fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("I", dtr | rts))
            except OSError:
                # A pty has no modem-control lines. On a real CDC-ACM this
                # does not fail, and refusing to read here would trade a
                # captured post-mortem for a tidy error path.
                pass
        except OSError as exc:
            self.note(f"could not configure {self.args.port}: {exc}")
            os.close(fd)
            return None
        return fd

    def capture(self):
        """Attach and log until the window closes or the port dies again."""
        waited = 0.0
        holders = self.other_holders()
        while holders and waited < self.args.acquire:
            self.note(f"yielding: port held by pid(s) {','.join(map(str, holders))}")
            time.sleep(self.args.poll)
            waited += self.args.poll
            if not RUNNING:
                return
            holders = self.other_holders()
        if holders:
            self.note(
                f"gave up after {waited:.1f}s: pid(s) "
                f"{','.join(map(str, holders))} own the port"
            )
            return

        fd = self.open_port()
        if fd is None:
            return
        self.note("attached")
        deadline = time.monotonic() + self.args.capture
        # Two queries: the first can be swallowed while the CDC endpoint is
        # still settling after enumeration.
        queries = [time.monotonic() + 0.3, time.monotonic() + 5.0]
        buf = b""
        try:
            while RUNNING and time.monotonic() < deadline:
                while queries and time.monotonic() >= queries[0]:
                    queries.pop(0)
                    try:
                        os.write(fd, b"p")
                        self.note("sent post-mortem query ('p')")
                    except OSError as exc:
                        self.note(f"post-mortem query failed: {exc}")
                timeout = min(self.args.poll, max(0.0, deadline - time.monotonic()))
                if queries:
                    timeout = min(timeout, max(0.0, queries[0] - time.monotonic()))
                try:
                    ready, _, _ = select.select([fd], [], [], timeout)
                except OSError as exc:
                    self.note(f"port lost while attached: {exc}")
                    return
                if not ready:
                    continue
                try:
                    chunk = os.read(fd, 4096)
                except OSError as exc:
                    self.note(f"port lost while attached: {exc}")
                    return
                if not chunk:
                    # A pty whose writer went away reports EOF rather than an
                    # error. Same event, same handling: re-arm.
                    self.note("port lost while attached: end of stream")
                    return
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    text = line.decode("utf-8", "replace").replace("\r", "").strip()
                    if text:
                        self.said(text)
            if buf:
                self.said(buf.decode("utf-8", "replace").replace("\r", "").strip())
            self.note("capture window closed")
        finally:
            try:
                os.close(fd)
            except OSError:
                pass

    def run(self):
        self.note(
            f"witness armed port={self.args.port} label={self.args.label} "
            f"pid={os.getpid()}"
        )
        self.note(
            "attaches only after this path disappears and returns; periculum "
            "owns the port during a scenario"
        )
        present = os.path.exists(self.args.port)
        if not present:
            self.note("port is not there yet; waiting for it to appear")
        gone_at = None
        while RUNNING:
            now_present = os.path.exists(self.args.port)
            if now_present and not present:
                gap = "" if gone_at is None else f" after {time.monotonic() - gone_at:.1f}s"
                self.note(f"port came back{gap} — this is the reset the witness is for")
                self.capture()
                # The port may have gone again during the capture; re-read
                # rather than assume, so the next transition is not missed.
                now_present = os.path.exists(self.args.port)
                if now_present:
                    self.note("re-armed")
            elif not now_present and present:
                gone_at = time.monotonic()
                self.note("port gone")
            present = now_present
            if RUNNING:
                time.sleep(self.args.poll)
        self.note("witness stopped")
        self.out.close()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", required=True, help="debug port path to watch")
    parser.add_argument("--out", required=True, help="witness log to append to")
    parser.add_argument("--label", default="", help="board label for the header")
    parser.add_argument("--poll", type=float, default=0.5, help="poll interval, s")
    parser.add_argument(
        "--capture", type=float, default=90.0, help="how long to read after a return, s"
    )
    parser.add_argument(
        "--acquire",
        type=float,
        default=20.0,
        help="how long to wait for another holder to let go, s",
    )
    args = parser.parse_args(argv)

    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    Witness(args).run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
