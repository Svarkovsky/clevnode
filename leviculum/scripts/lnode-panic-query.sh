#!/usr/bin/env bash
#
# Query an LNode's panic evidence over the debug CDC port.
#
# Sends the firmware's post-mortem query trigger (a single "p" byte) to
# the debug console and prints the reply: the [PANIC_COUNT] line plus,
# if a post-mortem record is stored in .uninit RAM, the
# [HARDFAULT_PMRT] / [PANIC_PMRT] block. The record survives soft
# resets and repeated reads (the boot replay only marks it seen), so
# this can be re-run at will; only power loss or a reflash wipes it.
#
# DTR+RTS are asserted on open (same trick as flash-lnodes-from-head.sh)
# because the CDC-ACM debug port transmits only with DTR raised. Pure
# python3 stdlib (termios/fcntl), no pyserial required.
#
# Usage: lnode-panic-query.sh <debug-port> [timeout-secs]
#   e.g. lnode-panic-query.sh /dev/leviculum-rak-debug
#        lnode-panic-query.sh /dev/serial/by-id/usb-...-if00 10
#
# Exit 0 when a complete [PM_QUERY] begin..done response was seen,
# 1 on timeout or unopenable port.

set -euo pipefail

if [ $# -lt 1 ]; then
    echo "usage: $0 <debug-port> [timeout-secs]" >&2
    exit 1
fi

PORT="$1"
TIMEOUT="${2:-5}"

python3 - "$PORT" "$TIMEOUT" <<'PY'
import sys, os, time, fcntl, termios, struct, select

port, secs = sys.argv[1], float(sys.argv[2])
try:
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
except OSError as e:
    print(f"cannot open {port}: {e}", file=sys.stderr)
    sys.exit(1)

TAGS = ("[PM_QUERY]", "[PANIC_COUNT]", "[HARDFAULT_PMRT]", "[PANIC_PMRT]")
try:
    iflag, oflag, cflag, lflag, ispeed, ospeed, cc = termios.tcgetattr(fd)
    iflag = oflag = lflag = 0
    cflag = termios.CLOCAL | termios.CREAD | termios.CS8
    ispeed = ospeed = termios.B115200
    termios.tcsetattr(fd, termios.TCSANOW,
                      [iflag, oflag, cflag, lflag, ispeed, ospeed, cc])
    dtr = getattr(termios, 'TIOCM_DTR', 0x002)
    rts = getattr(termios, 'TIOCM_RTS', 0x004)
    fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack('I', dtr | rts))
    # Drop whatever ring backlog drained before we asked, so a stale
    # response cannot be mistaken for ours. Best-effort: the firmware
    # keeps draining while we run, so a fresh flood line slipping
    # through is harmless (the tag filter drops it).
    time.sleep(0.2)
    termios.tcflush(fd, termios.TCIFLUSH)
    os.write(fd, b'p')

    deadline = time.monotonic() + secs
    buf, begun, done = b'', False, False
    while time.monotonic() < deadline and not done:
        r, _, _ = select.select([fd], [], [], deadline - time.monotonic())
        if not r:
            continue
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            break
        buf += chunk
        while b'\n' in buf:
            line, buf = buf.split(b'\n', 1)
            text = line.decode('utf-8', 'replace').replace('\r', '').strip()
            if not any(t in text for t in TAGS):
                continue
            if '[PM_QUERY] begin' in text:
                begun = True
            if begun:
                print(text)
            if begun and '[PM_QUERY] done' in text:
                done = True
                break
    if not done:
        print(f"timeout after {secs}s waiting for [PM_QUERY] done "
              f"(is the firmware new enough to answer the query?)",
              file=sys.stderr)
        sys.exit(1)
finally:
    os.close(fd)
PY
