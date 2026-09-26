#!/bin/bash
# =============================================================================
# Script to package the clevnode release into a zip archive with MD5 checksums
# =============================================================================
set -e

PROJECT_ROOT="$(cd "$(dirname "$0")" && pwd)"
RELEASE_DIR="$PROJECT_ROOT/Releases"
STAGING_DIR="$RELEASE_DIR/staging"

echo "========================================================================"
echo "               clevnode Release Packaging Script"
echo "========================================================================"

# Check if the build output exists
if [ ! -f "$PROJECT_ROOT/clevnode/clevnode" ]; then
    echo "[!] Error: clevnode binary not found! Please compile the project first."
    exit 1
fi

# Create Release and Staging directories
echo "[*] Creating Release directories..."
mkdir -p "$RELEASE_DIR"
rm -rf "$STAGING_DIR"
mkdir -p "$STAGING_DIR"

# Copy binary
echo "[*] Copying binary..."
cp "$PROJECT_ROOT/clevnode/clevnode" "$STAGING_DIR/clevnode"

# Copy reticulum config with dot
echo "[*] Copying config to .reticulum/config..."
mkdir -p "$STAGING_DIR/.reticulum"
if [ -f "$PROJECT_ROOT/clevnode/reticulum/config" ]; then
    cp "$PROJECT_ROOT/clevnode/reticulum/config" "$STAGING_DIR/.reticulum/config"
else
    echo "[!] Warning: clevnode/reticulum/config not found! Creating default empty config."
    touch "$STAGING_DIR/.reticulum/config"
fi

# Copy posts
echo "[*] Copying posts directory..."
if [ -d "$PROJECT_ROOT/clevnode/posts" ]; then
    cp -r "$PROJECT_ROOT/clevnode/posts" "$STAGING_DIR/posts"
else
    echo "[!] Warning: clevnode/posts directory not found! Creating default empty posts folder."
    mkdir -p "$STAGING_DIR/posts"
fi

# Generate S90clevnode
echo "[*] Generating S90clevnode startup script..."
cat << 'STARTUP' > "$STAGING_DIR/S90clevnode"
#!/bin/sh
ENABLED=yes
PROG="clevnode"
DIR="\$(cd "\$(dirname "\$0")" && pwd)"
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

chmod +x "$STAGING_DIR/S90clevnode"

# Calculate MD5 for internal files
echo "[*] Calculating MD5 checksums for internal files..."
cd "$STAGING_DIR"
find . -type f ! -name "md5sums.txt" -print0 | xargs -0 md5sum > md5sums.txt

# Pack into zip
echo "[*] Packaging release into zip..."
ZIP_NAME="clevnode-v0.1.0-mips-ath79-static.zip"
zip -r "$RELEASE_DIR/$ZIP_NAME" clevnode .reticulum posts S90clevnode md5sums.txt

# Cleanup staging
rm -rf "$STAGING_DIR"

# Calculate MD5 for the zip archive itself
echo "[*] Calculating MD5 checksum for the ZIP archive..."
cd "$RELEASE_DIR"
md5sum "$ZIP_NAME" > "$ZIP_NAME.md5"

echo "========================================================================"
echo "Release packaged successfully!"
echo "ZIP Archive:  $RELEASE_DIR/$ZIP_NAME"
echo "Checksum File: $RELEASE_DIR/$ZIP_NAME.md5"
echo "========================================================================"
