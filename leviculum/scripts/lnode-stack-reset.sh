#!/usr/bin/env bash
#
# Restart an LNode's stack high-water measurement over the debug CDC port.
#
# Sends the firmware's watermark-reset trigger (a single "s" byte) and
# prints the two [STACK] lines it answers with:
#
#   [STACK] ... tag=pre-reset   <- the peak of the phase that just ended
#   [STACK] ... tag=reset       <- the new ceiling (`painted=`)
#
# `min_free` on the pre-reset line is the number to record; `peak_used`
# is its complement against the region size. Without a reset every
# reading reports the boot transient forever, which is the deepest single
# moment on most boards and tells you nothing about load (#255 phase A).
#
# Intended use — one measurement per phase:
#   lnode-stack-reset.sh /dev/leviculum-t114-debug   # arm
#   ...drive the phase (BLE traffic, LoRa relay, telemetry)...
#   lnode-stack-reset.sh /dev/leviculum-t114-debug   # read + re-arm
#
# DTR+RTS are asserted on open (same trick as lnode-panic-query.sh)
# because the CDC-ACM debug port transmits only with DTR raised. Pure
# python3 stdlib (termios/fcntl), no pyserial required.
#
# Usage: lnode-stack-reset.sh <debug-port> [timeout-secs]
#
# Exit 0 when the tag=reset line was seen, 1 on timeout or unopenable
# port.

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
    # Drop the ring backlog that drained before we asked, so a periodic
    # `tag=tick` line from before the reset cannot be read as the answer.
    time.sleep(0.2)
    termios.tcflush(fd, termios.TCIFLUSH)
    os.write(fd, b's')

    deadline = time.monotonic() + secs
    buf, done = b'', False
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
            if '[STACK]' not in text:
                continue
            if 'tag=pre-reset' in text or 'tag=reset' in text:
                print(text)
            if 'tag=reset' in text:
                done = True
                break
    if not done:
        print(f"timeout after {secs}s waiting for [STACK] tag=reset "
              f"(is the firmware new enough to answer the reset?)",
              file=sys.stderr)
        sys.exit(1)
finally:
    os.close(fd)
PY
