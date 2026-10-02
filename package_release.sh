#!/bin/bash
# =============================================================================
# Script to package the local clevnode MIPS build into a distribution archive
# =============================================================================
set -e

PROJECT_ROOT="$(cd "$(dirname "$0")" && pwd)"
VERSION="${1:-v0.1.1}"
ARCH="mips-ath79-bigendian"

echo "========================================================================"
echo "          clevnode Local Release Packaging Script"
echo "========================================================================"

if [ ! -f "$PROJECT_ROOT/clevnode/clevnode" ]; then
    echo "[!] Error: clevnode binary not found! Please compile the project first (e.g. via ./build_mips_be.sh)."
    exit 1
fi

chmod +x "$PROJECT_ROOT/scripts/package_target.sh"
"$PROJECT_ROOT/scripts/package_target.sh" \
    "$PROJECT_ROOT/clevnode/clevnode" \
    "$ARCH" \
    "$VERSION" \
    "$PROJECT_ROOT/Releases" \
    "-Os -march=24kc -mtune=24kc -mno-branch-likely -msoft-float -fno-ident -fno-stack-protector -fomit-frame-pointer -mno-shared -no-pie" \
    "-static -no-pie -Wl,--gc-sections -lpthread -lrt -lgcc -Wl,--build-id=none -Wl,-z,norelro -Wl,-O2 -Wl,--exclude-libs,ALL" \
    "mips-unknown-linux-musl" \
    "mips-openwrt-linux-musl-gcc" \
    "OpenWrt SDK 23.05.3 ath79 toolchain"

echo "========================================================================"
echo "Package ready in Releases/clevnode-${VERSION}-${ARCH}-static.zip"
echo "========================================================================"
