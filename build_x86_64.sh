#!/bin/bash
set -e

PROJECT_ROOT="$(cd "$(dirname "$0")" && pwd)"

# --- Parse Arguments ---
ENABLE_CLEAN=false

for arg in "$@"; do
    case "$arg" in
        --clean)
            ENABLE_CLEAN=true
            ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --clean     Clean target directories and artifacts before build"
            echo "  --help, -h  Show this help message"
            exit 0
            ;;
        *)
            echo "Error: Unknown argument '$arg'"
            exit 1
            ;;
    esac
done

echo "============================================"
echo "[build_x86_64.sh] Project root: ${PROJECT_ROOT}"
echo "============================================"

# --- Find musl C compiler ---
if [ -x "/home/user/test/ytsub/toolchains/x86_64-linux-musl-native/bin/x86_64-linux-musl-gcc" ]; then
    MUSL_CC="/home/user/test/ytsub/toolchains/x86_64-linux-musl-native/bin/x86_64-linux-musl-gcc"
elif command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
    MUSL_CC="x86_64-linux-musl-gcc"
elif command -v musl-gcc >/dev/null 2>&1; then
    MUSL_CC="musl-gcc"
else
    echo "[!] Warning: No musl-gcc found, falling back to standard gcc"
    MUSL_CC="gcc"
fi

echo "[build_x86_64.sh] Using C compiler: ${MUSL_CC}"

# --- Handle --clean flag ---
if [ "$ENABLE_CLEAN" = true ]; then
    echo "[build_x86_64.sh] Cleaning build artifacts..."
    rm -f "${PROJECT_ROOT}/clevnode/clevnode"
    rm -f "${PROJECT_ROOT}/clevnode/libleviculum.a"
    rm -rf "${PROJECT_ROOT}/leviculum/target/x86_64-unknown-linux-musl"
fi

# Ensure rustup target is installed
rustup target add x86_64-unknown-linux-musl 2>/dev/null || true

# --- Compile Rust library ---
# CRITICAL: force-unwind-tables=no and embed-bitcode=no prevent ~440 KB of DWARF .eh_frame overhead
echo "[build_x86_64.sh] Compiling Rust library (libleviculum.a) with force-unwind-tables=no..."
cd "${PROJECT_ROOT}/leviculum"
RUSTFLAGS="-C force-unwind-tables=no -C embed-bitcode=no" \
    cargo +nightly build \
        --target x86_64-unknown-linux-musl \
        --release \
        -p leviculum-ffi \
        -Zbuild-std=std,panic_abort \
        -Zbuild-std-features=optimize_for_size
cd "${PROJECT_ROOT}"

# --- Compile C monolith ---
echo "[build_x86_64.sh] Compiling C monolith (clevnode)..."
cd "${PROJECT_ROOT}/clevnode"
cp -u ../leviculum/target/x86_64-unknown-linux-musl/release/libleviculum.a ./libleviculum.a

${MUSL_CC} -Os -march=x86-64 \
    -fno-ident -fno-stack-protector -fomit-frame-pointer \
    -fno-unwind-tables -fno-asynchronous-unwind-tables \
    -I. clevnode.c libleviculum.a \
    -o clevnode \
    -static -Wl,--gc-sections -lpthread -lrt -lgcc \
    -Wl,--build-id=none -Wl,-z,norelro -Wl,-O2 -Wl,--exclude-libs,ALL

strip clevnode
cd "${PROJECT_ROOT}"

echo ""
echo "============================================"
echo "[build_x86_64.sh] BUILD COMPLETED SUCCESSFULLY!"
echo "============================================"
ls -lh "${PROJECT_ROOT}/clevnode/clevnode"
size "${PROJECT_ROOT}/clevnode/clevnode"
