#!/bin/bash
set -e

PROJECT_ROOT=$(pwd)

# --- Parse Command-Line Arguments ---
ENABLE_CLEAN=false
ENABLE_SSTRIP=false

for arg in "$@"; do
    case "$arg" in
        --clean)
            ENABLE_CLEAN=true
            ;;
        --sstrip)
            ENABLE_SSTRIP=true
            ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --clean     Perform a complete cleanup of all SDKs, caches, and artifacts"
            echo "  --sstrip    Download, build, and apply extreme ELF section stripping (sstrip)"
            echo "  --help, -h  Show this help message"
            exit 0
            ;;
        *)
            echo "Error: Unknown argument '$arg'"
            echo "Run '$0 --help' for usage."
            exit 1
            ;;
    esac
done

echo "============================================"
echo "[build.sh] Project root: ${PROJECT_ROOT}"
echo "[build.sh] Options: clean=${ENABLE_CLEAN}, sstrip=${ENABLE_SSTRIP}"
echo "============================================"

# --- 0. Handle --clean flag ---
if [ "$ENABLE_CLEAN" = true ]; then
    echo "[build.sh] FULL CLEAN mode: removing all build artifacts, SDKs, and caches..."
    rm -rf "${PROJECT_ROOT}/clevnode/clevnode"
    rm -rf "${PROJECT_ROOT}/clevnode/lblogd.toml"
    if [ -d "${PROJECT_ROOT}/clevnode/posts" ]; then find "${PROJECT_ROOT}/clevnode/posts" -type f ! -name "index.mu" -delete 2>/dev/null || true; fi
    rm -rf "${PROJECT_ROOT}/clevnode/files"
    rm -rf "${PROJECT_ROOT}/clevnode/identities"
    rm -rf "${PROJECT_ROOT}/clevnode/reticulum"
    rm -rf "${PROJECT_ROOT}/clevnode/Makefile"
    rm -rf "${PROJECT_ROOT}/clevnode/libleviculum.a"
    rm -rf "${PROJECT_ROOT}/leviculum/target"
    rm -rf "${PROJECT_ROOT}/leviculum/.cargo"
    rm -f "${PROJECT_ROOT}/leviculum/libunwind.a"
    rm -rf "${PROJECT_ROOT}/tools/openwrt-sdk-*"
    rm -f "${PROJECT_ROOT}/tools/sstrip"
    echo "[build.sh] Clean complete. Starting fresh build..."
    echo ""
fi

# --- 1. Download OpenWrt SDK if needed ---
SDK_DIR="${PROJECT_ROOT}/tools/openwrt-sdk-23.05.3-ath79-generic_gcc-12.3.0_musl.Linux-x86_64"
if [ ! -d "$SDK_DIR" ]; then
    echo "[build.sh] Downloading OpenWrt SDK (MIPS Big-Endian / ath79)..."
    mkdir -p "${PROJECT_ROOT}/tools"
    cd "${PROJECT_ROOT}/tools"
    
    SDK_FILE="openwrt-sdk-23.05.3-ath79-generic_gcc-12.3.0_musl.Linux-x86_64.tar.xz"
    PRIMARY_URL="https://downloads.openwrt.org/releases/23.05.3/targets/ath79/generic/openwrt-sdk-23.05.3-ath79-generic_gcc-12.3.0_musl.Linux-x86_64.tar.xz"
    BACKUP_URL="https://mirrors.tuna.tsinghua.edu.cn/openwrt/releases/23.05.3/targets/ath79/generic/openwrt-sdk-23.05.3-ath79-generic_gcc-12.3.0_musl.Linux-x86_64.tar.xz"
    # 
    if ! wget -c --tries=5 --timeout=30 --show-progress "$PRIMARY_URL"; then
        echo "[build.sh] Primary download failed or interrupted. Switching to backup mirror..."
        # 
        if ! wget -c --tries=5 --timeout=30 --show-progress "$BACKUP_URL"; then
            echo "[build.sh] Error: Failed to download SDK from both primary and backup sources."
            exit 1
        fi
    fi

    echo "[build.sh] Extracting OpenWrt SDK..."
    tar -xf "$SDK_FILE"
    rm -f "$SDK_FILE"
    cd "${PROJECT_ROOT}"
    echo "[build.sh] OpenWrt SDK downloaded and extracted."
else
    echo "[build.sh] OpenWrt SDK already present, skipping download."
fi

# --- 2. Build sstrip tool (only if --sstrip was requested) ---
if [ "$ENABLE_SSTRIP" = true ]; then
    if [ ! -f "${PROJECT_ROOT}/tools/sstrip" ]; then
        echo "[build.sh] Building sstrip from ELFkickers..."
        mkdir -p "${PROJECT_ROOT}/tools"
        cd "${PROJECT_ROOT}/tools"
        wget -q --show-progress http://www.muppetlabs.com/~breadbox/pub/software/ELFkickers-3.2.tar.gz
        tar -xf ELFkickers-3.2.tar.gz
        cd ELFkickers-3.2/elfrw
        make libelfrw.a
        cd ../sstrip
        gcc -O2 -I../elfrw sstrip.c ../elfrw/libelfrw.a -o ../../sstrip
        cd "${PROJECT_ROOT}/tools"
        rm -rf ELFkickers-3.2 ELFkickers-3.2.tar.gz
        echo "[build.sh] sstrip built successfully."
        cd "${PROJECT_ROOT}"
    else
        echo "[build.sh] sstrip tool already present, skipping build."
    fi
else
    echo "[build.sh] sstrip option disabled (using standard strip)."
fi

# --- 3. Generate .cargo/config.toml (only if missing to preserve Cargo cache) ---
if [ ! -f "${PROJECT_ROOT}/leviculum/.cargo/config.toml" ]; then
    echo "[build.sh] Generating .cargo/config.toml..."
    mkdir -p "${PROJECT_ROOT}/leviculum/.cargo"
    cat << TOML_EOF > "${PROJECT_ROOT}/leviculum/.cargo/config.toml"
[build]
rustflags = [
    "-C", "target-feature=+crt-static",
    "-C", "force-unwind-tables=no",
    "-C", "embed-bitcode=no",
    "--remap-path-prefix==",
    "-C", "link-self-contained=no",
    "--sysroot=${SDK_DIR}/staging_dir/toolchain-mips_24kc_gcc-12.3.0_musl",
    "-C", "link-arg=-L${PROJECT_ROOT}/leviculum",
    "-C", "link-arg=-L${SDK_DIR}/staging_dir/toolchain-mips_24kc_gcc-12.3.0_musl/lib",
    "-C", "link-arg=-L${SDK_DIR}/staging_dir/toolchain-mips_24kc_gcc-12.3.0_musl/lib/gcc/mips-openwrt-linux-musl/12.3.0",
    "-C", "link-arg=-static",
    "-C", "link-arg=-static-libgcc",
    "-C", "link-arg=-static-libstdc++",
    "-C", "link-arg=-lc",
    "-C", "link-arg=-lm",
    "-C", "link-arg=-lgcc",
    "-C", "link-arg=-lgcc_eh",
]

[target.mips-unknown-linux-musl]
linker = "${SDK_DIR}/staging_dir/toolchain-mips_24kc_gcc-12.3.0_musl/bin/mips-openwrt-linux-gcc"

[unstable]
build-std = ["std", "panic_abort"]
build-std-features = ["optimize_for_size"]
TOML_EOF
fi

# --- 4. Copy libunwind.a workaround (only if missing or newer) ---
cp -u "${SDK_DIR}/staging_dir/toolchain-mips_24kc_gcc-12.3.0_musl/lib/gcc/mips-openwrt-linux-musl/12.3.0/libgcc_eh.a" "${PROJECT_ROOT}/leviculum/libunwind.a"

# --- 5. Generate clevnode/Makefile ---
echo "[build.sh] Generating clevnode/Makefile..."
{
  printf 'CC = %s\n' "${SDK_DIR}/staging_dir/toolchain-mips_24kc_gcc-12.3.0_musl/bin/mips-openwrt-linux-gcc"
  printf '\n'
  printf '# ==============================================================================\n'
  printf '# COMPILER AND LINKER OPTIMIZATION PROFILES\n'
  printf '# ==============================================================================\n'
  printf '#\n'
  printf '# OPTION 1: Experimental (UNSTABLE: -mips16 and -Wl,-N break Tokio and RT kernel stack)\n'
  printf '# CFLAGS = -Oz -Wall -Wextra -ffunction-sections -fdata-sections -fno-unwind-tables -fno-asynchronous-unwind-tables -falign-functions=1 -falign-jumps=1 -falign-loops=1 -falign-labels=1 -fno-math-errno -mtune=74kc -flto -fno-ident -fno-stack-protector -fomit-frame-pointer -mips16 -mno-shared -fmerge-all-constants\n'
  printf '# LDFLAGS = -static -Wl,--gc-sections -lpthread -lrt -lgcc -lgcc_eh -flto -Wl,--build-id=none -Wl,-z,norelro -Wl,-N -Wl,-O2 -Wl,--exclude-libs,ALL\n'
  printf '#\n'
  printf '# OPTION 2: Pipeline scheduling tuned for MIPS 74Kc superscalar core (ASUS RT-AC57U V3 / QCA9563, soft-float)\n'
  printf '# CFLAGS = -Os -Wall -Wextra -ffunction-sections -fdata-sections -fno-unwind-tables -fno-asynchronous-unwind-tables -fno-math-errno -march=24kc -mtune=74kc -mno-branch-likely -msoft-float -fno-ident -fno-stack-protector -fomit-frame-pointer -mno-shared\n'
  printf '# LDFLAGS = -static -Wl,--gc-sections -lpthread -lrt -lgcc -lgcc_eh -Wl,--build-id=none -Wl,-z,norelro -Wl,-O2 -Wl,--exclude-libs,ALL\n'
  printf '#\n'
  printf '# OPTION 3: Universal baseline for all MIPS Big-Endian routers (OpenWrt ath79, Qualcomm/Atheros 24Kc/74Kc, soft-float)\n'
  printf ' CFLAGS = -Os -Wall -Wextra -ffunction-sections -fdata-sections -fno-unwind-tables -fno-asynchronous-unwind-tables -fno-math-errno -march=24kc -mtune=24kc -mno-branch-likely -msoft-float -fno-ident -fno-stack-protector -fomit-frame-pointer -mno-shared\n'
  printf ' LDFLAGS = -static -Wl,--gc-sections -lpthread -lrt -lgcc -lgcc_eh -Wl,--build-id=none -Wl,-z,norelro -Wl,-O2 -Wl,--exclude-libs,ALL\n'
  printf '\n'
  printf 'INCLUDES = -I.\n'
  printf 'LIBS = libleviculum.a\n'
  printf 'STRIP = %s\n' "${SDK_DIR}/staging_dir/toolchain-mips_24kc_gcc-12.3.0_musl/bin/mips-openwrt-linux-strip"
  if [ "$ENABLE_SSTRIP" = true ]; then
      printf 'SSTRIP = %s\n' "${PROJECT_ROOT}/tools/sstrip"
  fi
  printf '\n'
  printf 'all: clevnode\n\n'
  printf 'clevnode: clevnode.c $(LIBS)\n'
  printf '\t$(CC) $(CFLAGS) $(INCLUDES) clevnode.c $(LIBS) -o clevnode $(LDFLAGS)\n'
  printf '\t$(STRIP) clevnode\n'
  if [ "$ENABLE_SSTRIP" = true ]; then
      printf '\t$(SSTRIP) clevnode\n'
  fi
  printf '\n'
  printf 'clean:\n'
  printf '\trm -f clevnode\n'
} > "${PROJECT_ROOT}/clevnode/Makefile"

# --- 6. Compile Rust library (incremental build via Cargo) ---
echo "[build.sh] Compiling Rust library (libleviculum.a)..."
cd "${PROJECT_ROOT}/leviculum"
export PATH="${SDK_DIR}/staging_dir/toolchain-mips_24kc_gcc-12.3.0_musl/bin:${PATH}"
cargo +nightly build --target mips-unknown-linux-musl --release -p leviculum-ffi -Zbuild-std
cd "${PROJECT_ROOT}"

# --- 7. Compile C monolith (clevnode) ---
echo "[build.sh] Compiling C monolith (clevnode)..."
cd "${PROJECT_ROOT}/clevnode"
cp -u ../leviculum/target/mips-unknown-linux-musl/release/libleviculum.a ./libleviculum.a
make
cd "${PROJECT_ROOT}"

# --- 8. Create runtime configuration files & directories ---
echo "[build.sh] Preparing runtime directories and default configs..."
mkdir -p "${PROJECT_ROOT}/clevnode/posts"
mkdir -p "${PROJECT_ROOT}/clevnode/files"
mkdir -p "${PROJECT_ROOT}/clevnode/identities"
mkdir -p "${PROJECT_ROOT}/clevnode/reticulum"

# Copy default blog config template if not present
if [ ! -f "${PROJECT_ROOT}/clevnode/lblogd.toml" ] && [ -f "${PROJECT_ROOT}/lblogd.toml" ]; then
    cp "${PROJECT_ROOT}/lblogd.toml" "${PROJECT_ROOT}/clevnode/lblogd.toml"
fi

# Generate default Reticulum network config template if missing
if [ ! -f "${PROJECT_ROOT}/clevnode/reticulum/config" ]; then
    cat > "${PROJECT_ROOT}/clevnode/reticulum/config" << 'INI_EOF'
# Reticulum configuration for clevnode (INI format)
# Reference: https://reticulum.network/manual/interfaces.html

[reticulum]
enable_transport = yes
share_instance = yes
shared_instance_port = 37428
flush_interval = 900

[logging]
loglevel = 1

[interfaces]
  [[Local Wi-Fi Server]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242

  [[WDGWars Node]]
    type = TCPClientInterface
    enabled = yes
    target_host = rns.wdgwars.pl
    target_port = 4242
    bootstrap_only = yes
    name = WDGWars Node
INI_EOF
fi

# --- 9. Cleanup build artifacts ---
echo "[build.sh] Build complete. Preserving caches for incremental compilation."

# --- Done ---
echo ""
echo "============================================"
echo "[build.sh] BUILD COMPLETED SUCCESSFULLY!"
echo "============================================"
echo ""
echo "Binary info:"
ls -lh "${PROJECT_ROOT}/clevnode/clevnode"
echo ""
if [ "$ENABLE_SSTRIP" = true ]; then
    echo "Optimization: Extreme ELF section stripping (sstrip) was APPLIED."
else
    echo "Optimization: Standard GNU strip was applied (safe default)."
fi
echo "Ready to deploy to the router!"
