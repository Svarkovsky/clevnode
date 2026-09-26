#!/usr/bin/env bash
#
# Verify the libleviculum install feels like a normal Unix C library: a staged
# `make install` produces the SONAME symlink chain, header, static archive, and
# pkg-config file, and a third-party consumer compiles, links, and runs against
# it purely through pkg-config, both dynamically and statically. Fails loudly on
# any drift (a renamed export breaking the header, a wrong .pc, a missing
# soname, a load failure).
#
# Usage:
#   scripts/verify-packaging.sh [TARGET]
# TARGET defaults to the host (x86_64-unknown-linux-gnu). Pass
# aarch64-unknown-linux-gnu to verify the cross-built package; that path
# cross-compiles the consumer and runs it under qemu, and SKIPs cleanly if the
# aarch64 toolchain (rustup target, gcc-aarch64-linux-gnu, qemu-user-static) is
# not installed.
#
# Requires: cargo, cc, pkg-config, readelf (binutils); plus the cross tools for
# a non-host TARGET.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ffi_dir="$repo_root/leviculum-ffi"
target="${1:-x86_64-unknown-linux-gnu}"

fail() { echo "FAIL: $*" >&2; exit 1; }

# Per-target toolchain: native compiler/runner for the host, cross compiler plus
# a qemu runner (and glibc sysroot) for aarch64.
runner=()
case "$target" in
    x86_64-unknown-linux-gnu)
        cc_bin="cc"
        ;;
    aarch64-unknown-linux-gnu)
        cc_bin="aarch64-linux-gnu-gcc"
        if ! command -v "$cc_bin" >/dev/null \
            || ! rustup target list --installed 2>/dev/null | grep -q "^$target$" \
            || ! command -v qemu-aarch64-static >/dev/null; then
            echo "SKIP: aarch64 toolchain not installed (need: rustup target add $target," \
                 "apt install gcc-aarch64-linux-gnu qemu-user-static)"
            exit 0
        fi
        runner=(qemu-aarch64-static)
        export QEMU_LD_PREFIX="/usr/aarch64-linux-gnu"
        ;;
    *)
        fail "unsupported target $target"
        ;;
esac

stage="$(mktemp -d)"
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT

# Run an installed consumer binary, transparently under the cross runner when
# verifying a non-host target. leviculum (dynamic case) is found via
# LD_LIBRARY_PATH; the guest libc loader via QEMU_LD_PREFIX.
run_consumer() {
    if [ ${#runner[@]} -gt 0 ]; then
        LD_LIBRARY_PATH="$stage/lib" "${runner[@]}" "$1"
    else
        LD_LIBRARY_PATH="$stage/lib" "$1"
    fi
}

echo "==> staged install ($target) into $stage"
make -C "$ffi_dir" install PREFIX="$stage" TARGET="$target" >/dev/null

export PKG_CONFIG_PATH="$stage/lib/pkgconfig"

echo "==> install layout"
[ -f "$stage/include/leviculum.h" ] || fail "header not installed"
[ -e "$stage/lib/libleviculum.so" ] || fail "libleviculum.so (dev symlink) missing"
[ -e "$stage/lib/libleviculum.so.0" ] || fail "libleviculum.so.0 (soname symlink) missing"
[ -f "$stage/lib/libleviculum.a" ] || fail "static archive libleviculum.a not installed"
real="$(readlink -f "$stage/lib/libleviculum.so")"
[ -f "$real" ] || fail "symlink chain does not resolve to a real .so"
soname="$(readelf -d "$real" | sed -n 's/.*SONAME).*\[\(.*\)\]/\1/p')"
[ "$soname" = "libleviculum.so.0" ] || fail "embedded SONAME is '$soname', expected libleviculum.so.0"

echo "==> pkg-config"
pkg-config --exists leviculum || fail "pkg-config does not see leviculum"
want_ver="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"
got_ver="$(pkg-config --modversion leviculum)"
[ "$got_ver" = "$want_ver" ] || fail "pkg-config modversion '$got_ver' != Cargo.toml '$want_ver'"

echo "==> compile + link a consumer purely via pkg-config (dynamic)"
"$cc_bin" "$ffi_dir/examples/c/consumer.c" $(pkg-config --cflags --libs leviculum) -o "$stage/consumer" \
    || fail "consumer failed to compile/link against the installed library"
readelf -d "$stage/consumer" | grep -q "NEEDED.*libleviculum.so.0" \
    || fail "consumer is not dynamically linked to libleviculum.so.0"

echo "==> run the dynamic consumer"
out="$(run_consumer "$stage/consumer")"
echo "$out" | sed 's/^/    /'
echo "$out" | grep -q "^leviculum $want_ver " || fail "consumer did not report version $want_ver"
echo "$out" | grep -q "builder create/free: ok" || fail "consumer could not use the library"

echo "==> static link against the archive (glibc stays dynamic) via pkg-config"
# pkg-config --static surfaces Libs.private (the archive's system deps); drop
# the main -lleviculum and force the archive with -l:libleviculum.a so only
# leviculum is static, glibc stays dynamic (as build-c-lnsd does).
priv="$(pkg-config --static --libs-only-l leviculum | sed 's/-lleviculum//g')"
"$cc_bin" "$ffi_dir/examples/c/consumer.c" \
    $(pkg-config --cflags leviculum) $(pkg-config --libs-only-L leviculum) \
    -l:libleviculum.a $priv -o "$stage/consumer_static" \
    || fail "static link against libleviculum.a failed (check Libs.private)"
if readelf -d "$stage/consumer_static" | grep -q "libleviculum"; then
    fail "static consumer still carries a dynamic libleviculum dependency"
fi
out_static="$(run_consumer "$stage/consumer_static")"
echo "$out_static" | grep -q "builder create/free: ok" || fail "static consumer did not run"
echo "    static consumer ran with no libleviculum.so dependency"

echo "PASS ($target): libleviculum installs and links as a standard Unix C library"
