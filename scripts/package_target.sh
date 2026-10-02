#!/bin/bash
set -e

BINARY="${1:?Binary path required}"
ARCH="${2:?Architecture name required}"
VERSION="${3:-v0.1.1}"
OUT_DIR="${4:-Releases}"
CFLAGS_INFO="${5:-Not specified}"
LDFLAGS_INFO="${6:-Not specified}"
TARGET_TRIPLE="${7:-Not specified}"
CC_NAME="${8:-gcc}"
TOOLCHAIN_INFO="${9:-musl-cross-make}"

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

case "$OUT_DIR" in
    /*) TARGET_DIR="$OUT_DIR" ;;
    *)  TARGET_DIR="$PROJECT_ROOT/$OUT_DIR" ;;
esac

STAGE_DIR="$(mktemp -d)"
mkdir -p "$TARGET_DIR"

cp "$BINARY" "$STAGE_DIR/clevnode"

mkdir -p "$STAGE_DIR/.reticulum"
if [ -f "$PROJECT_ROOT/clevnode/reticulum/config" ]; then
    cp "$PROJECT_ROOT/clevnode/reticulum/config" "$STAGE_DIR/.reticulum/config"
else
    touch "$STAGE_DIR/.reticulum/config"
fi

mkdir -p "$STAGE_DIR/posts"
if [ -f "$PROJECT_ROOT/clevnode/posts/index.mu" ]; then
    cp "$PROJECT_ROOT/clevnode/posts/index.mu" "$STAGE_DIR/posts/index.mu"
fi

cat << 'STARTUP' > "$STAGE_DIR/S90clevnode"
#!/bin/sh
ENABLED=yes
PROG="clevnode"
DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/$PROG"
LOG="$DIR/clevnode.log"

start() {
    [ "$ENABLED" != "yes" ] && return 0
    pidof "$PROG" >/dev/null && return 0
    echo -n "Starting $PROG... "
    cd "$DIR" || exit 1
    "$BIN" > "$LOG" 2>&1 &
    sleep 1
    pidof "$PROG" >/dev/null && echo "OK" || echo "FAILED"
}

stop() {
    echo -n "Stopping $PROG... "
    killall "$PROG" 2>/dev/null
    for i in 1 2; do
        sleep 1
        ! pidof "$PROG" >/dev/null && echo "OK" && return 0
    done
    killall -9 "$PROG" 2>/dev/null
    echo "Force killed"
}

case "$1" in
    start)   start ;;
    stop)    stop ;;
    restart) stop; sleep 1; start ;;
    status)  pidof "$PROG" >/dev/null && echo "Running" || echo "Stopped" ;;
    *)       echo "Usage: $0 {start|stop|restart|status}"; exit 1 ;;
esac
STARTUP
chmod +x "$STAGE_DIR/S90clevnode"

cat << BUILD_INFO > "$STAGE_DIR/build_info.txt"
Package: clevnode
Version: ${VERSION}
Target Triple: ${TARGET_TRIPLE}
Architecture Name: ${ARCH}
Build Date: $(date -u +"%Y-%m-%d %H:%M:%S UTC")

Compilation Flags:
  CFLAGS:  ${CFLAGS_INFO}
  LDFLAGS: ${LDFLAGS_INFO}
  Rust:    cargo +nightly -Zbuild-std=std,panic_abort -Zbuild-std-features=optimize_for_size --release

Toolchain Details:
  Compiler: ${CC_NAME}
  Info:     ${TOOLCHAIN_INFO}
  C Library: musl libc (static linkage)
BUILD_INFO

(
    cd "$STAGE_DIR"
    find . -type f ! -name "md5sums.txt" -print0 | xargs -0 md5sum > md5sums.txt
    ZIP_NAME="clevnode-${VERSION}-${ARCH}-static.zip"
    zip -r "$TARGET_DIR/$ZIP_NAME" clevnode .reticulum posts S90clevnode build_info.txt md5sums.txt >/dev/null
    cd "$TARGET_DIR"
    md5sum "$ZIP_NAME" > "$ZIP_NAME.md5"
    echo "Packaged $TARGET_DIR/$ZIP_NAME"
)

rm -rf "$STAGE_DIR"
