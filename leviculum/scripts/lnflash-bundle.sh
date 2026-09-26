#!/usr/bin/env bash
# Build the lnflash tarball: a stranger unpacks it on any Linux, runs one
# binary, and their board ends up running our firmware.
#
#     tar xzf lnflash-<version>.tar.gz
#     cd lnflash-<version>
#     sudo ./lnflash
#
# Nothing installed, no repo, no network. This script is the build side of
# that promise, so everything it puts in the tarball comes from this checkout
# and nowhere else — a bundle built from a Meshtastic checkout is exactly the
# hidden dependency our clone-and-deploy policy forbids.
#
# Output: target/lnflash/lnflash-<version>.tar.gz
#
#   OUT_DIR    where to write (default target/lnflash)
#   SKIP_FIRMWARE=1  reuse an already-built firmware ELF instead of building
#                    it. For iterating on the bundle itself; a release build
#                    must not use it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NRF_DIR="$ROOT/leviculum-nrf"
NRF_TARGET="$NRF_DIR/target/thumbv7em-none-eabihf/release"
OUT_DIR="${OUT_DIR:-$ROOT/target/lnflash}"

# The boards this bundle carries. One record per board, four fields:
#
#   <name>|<cargo bin>|<cargo features>|<vendored SoftDevice stem, or empty>
#
# `name` is the catalogue key in lnflash/catalogue.toml, and it is the same
# name the manifest sections and the staging directories use — that is what
# lets everything below be a loop instead of a branch. The fourth field names
# the pair under lnflash/payload/<name>/ that a SoftDevice remedy ships as,
# `<stem>_softdevice.hex` beside `<stem>_license-agreement.txt`; a board that
# carries no remedy leaves it empty and simply gets no remedy section.
#
# A third board is one more line here. Nothing below this list mentions a
# board by name: the builds, the UF2 conversion, the staging, the manifest and
# the licence assertions against the finished tarball all walk it.
#
# Which image a board gets is settled in docs/src/concepts/board-support-scope.md:
# one build serves a pinout family, so the RAK4631 ships the baseboard build
# that also runs on a bare module rather than a second, stripped one.
BOARDS=(
    "t114|t114|bsp-t114|s140_nrf52_7.3.0"
    "rak4631|rak4631|bsp-rak4631,rak-baseboard|"
)

# Fixed properties of the Adafruit nRF52 UF2 family, the same two constants
# leviculum-nrf/tools/uf2-runner.sh writes into every flashed image and the
# same two lnflash/catalogue.toml records per board. They are not per-board
# fields here because every board in the list above is an nRF52840 running
# that bootloader; a board outside the family needs a new transport in
# lnflash, not a new column (docs/src/concepts/lnode-flashing.md, "Four axes").
FLASH_BASE=0x27000
FAMILY_ID=0xADA52840

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
GIT_SHA="$(git rev-parse --short HEAD)"
BUILT="$(git log -1 --format=%cs)"
STAGE="$OUT_DIR/lnflash-$VERSION"
TARBALL="$OUT_DIR/lnflash-$VERSION.tar.gz"

say() { printf '[lnflash-bundle] %s\n' "$*"; }

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    say "WARNING: tracked files are modified. The firmware will report"
    say "         dirty=true and the manifest records git_sha=$GIT_SHA anyway."
fi

# --- The application images --------------------------------------------------
# Built here rather than vendored, so a bundle always carries the firmware the
# commit it was built from produces.
mkdir -p "$OUT_DIR"

find_objcopy() {
    local sysroot candidate
    sysroot="$(rustc --print sysroot 2>/dev/null || true)"
    if [ -n "$sysroot" ]; then
        candidate="$(find "$sysroot/lib/rustlib" -name llvm-objcopy -type f 2>/dev/null | head -1)"
        [ -n "$candidate" ] && { echo "$candidate"; return; }
    fi
    command -v llvm-objcopy || command -v arm-none-eabi-objcopy || true
}
OBJCOPY="$(find_objcopy)"
[ -n "$OBJCOPY" ] || { echo "no objcopy found (llvm-tools or arm-none-eabi-binutils)" >&2; exit 1; }

# bin2uf2 is leviculum-nrf's, already the producer of every image we flash.
# Reusing it keeps one UF2 writer on the build side rather than two.
BIN2UF2="$NRF_DIR/target/bin2uf2"
if [ ! -x "$BIN2UF2" ] || [ "$NRF_DIR/tools/bin2uf2.rs" -nt "$BIN2UF2" ]; then
    say "building bin2uf2"
    rustc -O -o "$BIN2UF2" "$NRF_DIR/tools/bin2uf2.rs"
fi

for record in "${BOARDS[@]}"; do
    IFS='|' read -r name bin features _stem <<<"$record"

    if [ "${SKIP_FIRMWARE:-0}" != 1 ]; then
        say "building the $name firmware ($features)"
        (cd "$NRF_DIR" && cargo build --release --bin "$bin" --features "$features")
    fi

    ELF="$NRF_TARGET/$bin"
    [ -f "$ELF" ] || { echo "no firmware ELF at $ELF" >&2; exit 1; }

    # ELF -> flat binary. -R .bss -R .uninit excludes NOBITS sections, without
    # which a pre-2020 llvm-objcopy emits a ~500 MB binary at the wrong
    # addresses.
    say "converting the $name firmware to UF2"
    "$OBJCOPY" -O binary -R .bss -R .uninit "$ELF" "$OUT_DIR/$name.bin.tmp"
    "$BIN2UF2" --base "$FLASH_BASE" --family "$FAMILY_ID" \
        "$OUT_DIR/$name.bin.tmp" "$OUT_DIR/$name.uf2.tmp"
done

# --- The binary --------------------------------------------------------------
# musl-static by workspace default, so it runs on any Linux with no libc of
# its own to find.
say "building lnflash"
cargo build -p lnflash --release
LNFLASH="$ROOT/target/x86_64-unknown-linux-musl/release/lnflash"
[ -f "$LNFLASH" ] || { echo "no lnflash binary at $LNFLASH" >&2; exit 1; }

# --- Assemble ----------------------------------------------------------------
rm -rf "$STAGE"
mkdir -p "$STAGE/firmware"
install -m 0755 "$LNFLASH" "$STAGE/lnflash"
install -m 0644 "$ROOT/lnflash/payload/README-bundle.md" "$STAGE/README.md"
install -m 0644 "$ROOT/LICENSE" "$STAGE/LICENSE"
# Codeberg #288. The statically linked Rust in this bundle — the lnflash binary
# and every firmware UF2 — carries MIT- and BSD-licensed crates whose licences
# require the notice to travel with the binary. THIRD-PARTY-NOTICES covers all
# of it: it is generated from the two lockfiles, host binaries in part 1 and
# the firmware in part 2, and both firmware binaries are built from the same
# leviculum-nrf lockfile that part 2 describes. The SoftDevice beside it keeps
# its own licence file; that blob is never linked in, so its terms are a
# separate matter (see the manifest's remedy section).
install -m 0644 "$ROOT/THIRD-PARTY-NOTICES" "$STAGE/THIRD-PARTY-NOTICES"

for record in "${BOARDS[@]}"; do
    IFS='|' read -r name _bin _features stem <<<"$record"
    mkdir -p "$STAGE/firmware/$name"
    install -m 0644 "$OUT_DIR/$name.uf2.tmp" \
        "$STAGE/firmware/$name/leviculum-$name-$VERSION.uf2"
    rm -f "$OUT_DIR/$name.bin.tmp" "$OUT_DIR/$name.uf2.tmp"
    [ -n "$stem" ] || continue
    # The SoftDevice travels as a file with its licence beside it — never
    # linked into the binary, because Nordic's clauses 4 and 5 cannot become
    # part of one combined work with AGPL code.
    install -m 0644 "$ROOT/lnflash/payload/$name/${stem}_softdevice.hex" \
        "$STAGE/firmware/$name/"
    install -m 0644 "$ROOT/lnflash/payload/$name/${stem}_license-agreement.txt" \
        "$STAGE/firmware/$name/"
done

sha() { sha256sum "$STAGE/firmware/$1" | cut -d' ' -f1; }

cat > "$STAGE/firmware/manifest.toml" <<EOF
# What this bundle carries: the release it is, and one image per board.
#
# The board facts — USB IDs, flash geometry, Board-ID, SoftDevice constraint —
# are NOT here. They are hardware properties, they do not change when a release
# is cut, and the sessions that only configure a running board need them with
# no bundle on disk at all, so they live in lnflash/catalogue.toml, compiled
# into the binary (Codeberg #342). Keeping a second copy here would be a second
# copy to drift.
#
# Generated by scripts/lnflash-bundle.sh. Do not edit by hand — the checksums
# are what stand between a truncated download and somebody's flash.

[bundle]
version = "$VERSION"
built   = "$BUILT"
EOF

for record in "${BOARDS[@]}"; do
    IFS='|' read -r name _bin _features stem <<<"$record"
    image="$name/leviculum-$name-$VERSION.uf2"
    cat >> "$STAGE/firmware/manifest.toml" <<EOF

[board.$name.app]
file    = "$image"
sha256  = "$(sha "$image")"
# Checked back off the [FW_BUILD] banner on the debug port after the flash.
git_sha = "$GIT_SHA"
EOF
    [ -n "$stem" ] || continue
    cat >> "$STAGE/firmware/manifest.toml" <<EOF

[board.$name.remedy.softdevice]
file    = "$name/${stem}_softdevice.hex"
sha256  = "$(sha "$name/${stem}_softdevice.hex")"
# Mandatory, and lnflash refuses to load a bundle whose licence file is
# missing. Nordic's clause 2 requires the notice to travel with the blob.
license = "$name/${stem}_license-agreement.txt"
# The hex is distributed untouched and converted at run time, which avoids the
# question of whether repacking counts as the modification clause 5 forbids.
convert = "hex-to-uf2"
EOF
done

# --- Check it before shipping it ---------------------------------------------
say "checking the bundle"
"$STAGE/lnflash" --bundle "$STAGE" --check-bundle

tar -czf "$TARBALL" -C "$OUT_DIR" "lnflash-$VERSION"

# The licence files are asserted against the TARBALL rather than the stage
# directory, because the tarball is what ships and a `tar` that quietly leaves
# a file out is precisely the failure worth catching here. --check-bundle above
# cannot do it: it walks the manifest, and the manifest describes what lnflash
# writes to a board, not what the archive must carry for the distribution to be
# lawful (Codeberg #288).
listing="$(tar -tzf "$TARBALL")"
wanted=(
    "lnflash-$VERSION/LICENSE"
    "lnflash-$VERSION/THIRD-PARTY-NOTICES"
)
# Every board's image, and every vendored blob's licence beside it. Derived
# from the board list rather than written out, so a board added above cannot
# ship with its image silently missing from the archive.
for record in "${BOARDS[@]}"; do
    IFS='|' read -r name _bin _features stem <<<"$record"
    wanted+=("lnflash-$VERSION/firmware/$name/leviculum-$name-$VERSION.uf2")
    [ -n "$stem" ] || continue
    wanted+=("lnflash-$VERSION/firmware/$name/${stem}_license-agreement.txt")
done
for want in "${wanted[@]}"; do
    printf '%s\n' "$listing" | grep -qxF "$want" \
        || { echo "bundle is missing $want" >&2; exit 1; }
done
# Not just present: both halves of the notice file have to be in it. A
# truncated or half-generated file passes a name check and fails the obligation
# — the firmware section is the one that is easy to lose, because it comes from
# the second, separate workspace.
for marker in "PART 1 — host binaries" "PART 2 — LNode firmware image"; do
    grep -qF "$marker" "$STAGE/THIRD-PARTY-NOTICES" \
        || { echo "THIRD-PARTY-NOTICES is missing the '$marker' section" >&2; exit 1; }
done
say "licence files present in the tarball (LICENSE, THIRD-PARTY-NOTICES, SoftDevice agreement)"

say "wrote $TARBALL ($(du -h "$TARBALL" | cut -f1))"
say "contents:"
printf '%s\n' "$listing" | sed 's/^/  /'
