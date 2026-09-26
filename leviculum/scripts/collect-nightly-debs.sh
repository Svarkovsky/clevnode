#!/usr/bin/env bash
# Collects the .deb packages produced by cargo-deb, packs the raw
# binaries into per-arch userspace tarballs, and emits a source
# tarball from HEAD. Stages everything under dist/ with stable
# filenames so the rolling release URLs stay valid across nightly
# runs.
#
# Expects, for both musl triples under target/<triple>/release/:
#   lnsd, lnstest, lncp, lnstatus, lnprobe, lnomad, lblogd
# and one .deb per package and arch under target/debian/:
#   leviculum_*_{amd64,arm64}.deb
#   lnomad_*_{amd64,arm64}.deb
#   lblogd_*_{amd64,arm64}.deb
# and the lnflash firmware bundle from scripts/lnflash-bundle.sh:
#   target/lnflash/lnflash-<version>.tar.gz
# plus:
#   git available on PATH
#   LEVICULUM_BUILD_ID env var (embedded in the per-arch VERSION file)
#   .deb-version-<crate> files from scripts/deb-stamp.sh (ditto)
#
# Produces, for each of leviculum, lnomad and lblogd, and each of amd64
# and arm64:
#   dist/<pkg>-nightly-<arch>.deb          + .sha256
#   dist/<pkg>-nightly-<arch>.tar.gz       + .sha256   (just the binaries)
# plus one source tarball and one lnflash bundle:
#   dist/leviculum-nightly-source.tar.gz   + .sha256
#   dist/lnflash-nightly-amd64.tar.gz      + .sha256
# The .deb version lives in the control metadata and the embedded
# --version string, not in the filename. Binaries are pre-stripped
# at link time via [profile.release] strip = "debuginfo" in the
# workspace Cargo.toml — no extra strip step here. The source
# tarball is a git archive of HEAD (tracked files only).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DIST="dist"
rm -rf "$DIST"
mkdir -p "$DIST"

collect_deb() {
    local pkg="$1"        # leviculum | lnomad
    local arch_dash="$2"  # amd64 | arm64
    local stable="${pkg}-nightly-${arch_dash}.deb"

    # cargo-deb emits one .deb per package and arch under
    # target/debian/. The filename embeds the full nightly version,
    # which changes each run — glob to the unique file.
    local src
    src=$(ls -1 target/debian/"${pkg}"_*_"${arch_dash}".deb 2>/dev/null | head -n1)
    if [ -z "${src:-}" ]; then
        echo "error: no ${pkg} .deb found for ${arch_dash} under target/debian/" >&2
        exit 1
    fi

    cp "$src" "$DIST/$stable"
    (cd "$DIST" && sha256sum "$stable" >"$stable.sha256")
}

collect_deb leviculum amd64
collect_deb leviculum arm64
collect_deb lnomad amd64
collect_deb lnomad arm64
collect_deb lblogd amd64
collect_deb lblogd arm64

# Per-arch userspace binary tarball: the package's binaries plus
# README/LICENSE and a VERSION pointer. Drop-in for users who want the
# tools without root, system service, or .deb tooling.
pack_bin_tarball() {
    local pkg="$1"          # leviculum | lnomad | lblogd
    local arch_dash="$2"    # amd64 | arm64
    local rust_triple="$3"  # x86_64-unknown-linux-musl | aarch64-unknown-linux-musl
    local readme_src="$4"   # per-package README, path relative to repo root
    shift 4                 # remaining args: binaries to include

    local name="${pkg}-nightly-${arch_dash}"
    local stage="$DIST/$name"
    local src="target/${rust_triple}/release"

    mkdir -p "$stage/bin" "$stage/doc"
    for bin in "$@"; do
        cp "$src/$bin" "$stage/bin/$bin"
    done
    cp "$readme_src" "$stage/doc/README.md"
    cp LICENSE "$stage/doc/"
    # Codeberg #288: LICENSE alone is our AGPL and covers none of the
    # MIT/BSD crates that are statically linked into the binaries beside
    # it. Generated from Cargo.lock by scripts/gen-notices.py and tracked,
    # so this is a copy of a checked-in file — the tarball build needs no
    # extra tool and no network. `just notices-guard` (Tier 0) is what
    # keeps it describing the binaries it ships with.
    cp THIRD-PARTY-NOTICES "$stage/doc/"
    # CHANGELOG.md documents the leviculum stack. Shipping it inside the
    # lnomad and lblogd tarballs would attach a changelog to a version it
    # does not describe, now that those two are versioned independently.
    if [ "$pkg" = leviculum ]; then
        cp CHANGELOG.md "$stage/doc/"
    fi
    # The package version comes from the stamp file the build wrote, so
    # the tarball names the same version as the .deb beside it. Stripping
    # the ~nightly suffix leaves the plain package version; the build id
    # on the next line carries the date and commit.
    local crate="$pkg"
    [ "$pkg" = leviculum ] && crate=leviculum-cli
    local version="unknown"
    if [ -r ".deb-version-${crate}" ]; then
        version="$(sed 's/~.*//' ".deb-version-${crate}")"
    fi
    cat >"$stage/VERSION" <<EOF
${pkg} nightly build
version: ${version}
build-id: ${LEVICULUM_BUILD_ID:-unknown}
arch: linux-${arch_dash}
EOF

    tar -C "$DIST" -czf "$DIST/$name.tar.gz" "$name"
    rm -rf "$stage"
    (cd "$DIST" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
}

pack_bin_tarball leviculum amd64 x86_64-unknown-linux-musl README.md lnsd lnstest lncp lnstatus lnprobe
pack_bin_tarball leviculum arm64 aarch64-unknown-linux-musl README.md lnsd lnstest lncp lnstatus lnprobe
pack_bin_tarball lnomad amd64 x86_64-unknown-linux-musl lnomad/README.md lnomad
pack_bin_tarball lnomad arm64 aarch64-unknown-linux-musl lnomad/README.md lnomad
pack_bin_tarball lblogd amd64 x86_64-unknown-linux-musl lblogd/README.md lblogd
pack_bin_tarball lblogd arm64 aarch64-unknown-linux-musl lblogd/README.md lblogd

# The lnflash bundle is already a finished tarball — binary, firmware
# UF2, SoftDevice, licences and a manifest of checksums over all of it —
# so it is renamed rather than repacked. Repacking would invalidate
# nothing (the checksums are over the payload files, not the archive)
# but would put a second archive layout in the release for no reason.
#
# amd64 only: the UF2 and the SoftDevice are architecture-independent,
# the host binary inside is not, and the bundle script builds it for the
# runner's own architecture.
#
# Named exactly, not globbed: target/ survives between runs on a
# developer machine, so a glob would happily stage last release's
# bundle. The version comes from the same place the bundle script takes
# it from, so the two cannot disagree.
lnflash_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
lnflash_src="target/lnflash/lnflash-${lnflash_version}.tar.gz"
if [ ! -f "$lnflash_src" ]; then
    echo "error: no lnflash bundle at ${lnflash_src}" >&2
    echo "       run scripts/lnflash-bundle.sh first" >&2
    exit 1
fi
cp "$lnflash_src" "$DIST/lnflash-nightly-amd64.tar.gz"
(cd "$DIST" && sha256sum lnflash-nightly-amd64.tar.gz \
    >lnflash-nightly-amd64.tar.gz.sha256)

# Source tarball at the same commit as the binaries. git archive
# emits only tracked files, so vendor/ submodules and target/ never
# enter the tarball. The --prefix gives `tar xzf` a clean directory
# layout: leviculum-nightly-source/{Cargo.toml, reticulum-*, …}.
git archive --format=tar.gz --prefix=leviculum-nightly-source/ \
    -o "$DIST/leviculum-nightly-source.tar.gz" HEAD
(cd "$DIST" && sha256sum leviculum-nightly-source.tar.gz \
    >leviculum-nightly-source.tar.gz.sha256)

echo "=== dist/ ==="
ls -la "$DIST"
