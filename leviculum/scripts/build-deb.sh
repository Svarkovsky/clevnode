#!/usr/bin/env bash
# Build the musl-static Debian packages for one architecture.
#
# Why a script rather than inline commands: this sequence used to be
# spelled out twice, in the Justfile's build-deb* recipes and in
# .woodpecker/nightly.yml, and the two drifted — the same failure
# deb-stamp.sh was extracted to prevent, one layer up. Commit 4c580cc5
# added manual pages to the three .deb manifests and the rendering step
# that produces them to the Justfile only, so every nightly from
# 2026-07-29 on died in `cargo deb` with
#
#   error: Can't resolve asset: leviculum-cli/../target/man/lnsd.1
#
# and the rolling release went stale for eight days. The nightly also
# never called deb-finalize.sh, which the Justfile recipes did: once the
# assets resolved it would have gone on publishing packages without
# md5sums, with a mis-named changelog and an empty Depends field. Both
# gaps are drift between two copies of one procedure, so there is now
# one implementation and two callers.
#
# Usage:
#   scripts/build-deb.sh amd64
#   scripts/build-deb.sh arm64
#
# Expects scripts/deb-stamp.sh to have run. The versions are read from
# its stamp files rather than derived here, so an amd64 + arm64 pair
# from one build carries identical version strings even when the two
# builds straddle midnight UTC.
#
# Requires cargo-deb; arm64 additionally needs cargo-zigbuild and a
# ziglang on PATH (`just _deb-prereqs` installs both), and python3 for
# the manual pages. Produces target/debian/*_<arch>.deb, which cargo-deb
# also hardlinks under target/<triple>/debian/.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ARCH="${1:-}"
case "$ARCH" in
amd64) TRIPLE=x86_64-unknown-linux-musl ;;
arm64) TRIPLE=aarch64-unknown-linux-musl ;;
*)
    echo "usage: $0 <amd64|arm64>" >&2
    exit 2
    ;;
esac

# Every crate that ships a Debian package, and every binary the three of
# them install between them. Kept in the same order as deb-stamp.sh's
# CRATES, which writes the version files read below.
CRATES=(leviculum-cli lnomad lblogd)
BINS=(lnsd lnstest lncp lnstatus lnprobe lnomad lblogd)

# The .deb is not always named after its crate: leviculum-cli ships as
# "leviculum". Same mapping as deb-stamp.sh's pkg_name; it is spelled
# out in both places rather than shared, because a two-line case is
# cheaper to read twice than an extra sourced file is to follow.
pkg_name() {
    case "$1" in
    leviculum-cli) echo leviculum ;;
    *) echo "$1" ;;
    esac
}

# The stamp files are inputs to this script, not something it derives.
# Failing loudly beats building packages whose version strings silently
# disagree with the ones the other architecture shipped.
for crate in "${CRATES[@]}"; do
    [ -r ".deb-version-${crate}" ] || {
        echo "error: .deb-version-${crate} missing — run scripts/deb-stamp.sh first" >&2
        exit 1
    }
done
[ -r .build-id ] || {
    echo "error: .build-id missing — run scripts/deb-stamp.sh first" >&2
    exit 1
}
LEVICULUM_BUILD_ID="$(cat .build-id)"
export LEVICULUM_BUILD_ID
echo "[build-deb] ${ARCH} (${TRIPLE}) build-id=${LEVICULUM_BUILD_ID}"

# Render docs/src/man/*.1.md to roff under target/man/, which the three
# .deb manifests install from. The Markdown is the single source: mdbook
# publishes the same files, so the shipped manual pages and the online
# documentation cannot drift apart.
#
# The selftest runs first because nothing downstream reads the roff: a
# mangled conversion does not fail the build, it ships. It cost a silently
# wrong lnomad(1) once already (a code span holding a backtick shifted
# every span after it), so the conversions have a standing guard.
python3 scripts/md2man.py --selftest
python3 scripts/md2man.py --outdir target/man docs/src/man/*.1.md

# Incremental-relink insurance: a repeated build with an unchanged
# LEVICULUM_BUILD_ID can skip relinking some binaries and leave them
# with a stale version string. Fresh CI containers rarely hit this, but
# the clean is cheap.
clean_args=()
for crate in "${CRATES[@]}"; do
    clean_args+=(-p "$crate")
done
cargo clean "${clean_args[@]}"

# cargo-zigbuild uses Zig as the C compiler/linker — the only way to
# reach aarch64-musl from an amd64 host without docker-in-docker or a
# dedicated arm64 runner.
build_cmd=(cargo build)
[ "$ARCH" = arm64 ] && build_cmd=(cargo zigbuild)
bin_args=()
for bin in "${BINS[@]}"; do
    bin_args+=(--bin "$bin")
done
"${build_cmd[@]}" --release --target "$TRIPLE" "${bin_args[@]}"

# One --deb-version per package: the three are versioned independently,
# so a single shared version string would be wrong for at least two of
# them.
#
# --no-strip: rust already strips debuginfo at link time (see workspace
# Cargo.toml [profile.release] strip = "debuginfo"). cargo-deb's default
# strip --strip-all corrupted x86_64 musl-static binaries (SIGSEGV at
# startup), so it is skipped entirely.
debs=()
for crate in "${CRATES[@]}"; do
    version="$(cat ".deb-version-${crate}")"
    cargo deb -p "$crate" --target "$TRIPLE" --no-build --no-strip \
        --deb-version "$version"
    debs+=("target/debian/$(pkg_name "$crate")_${version}_${ARCH}.deb")
done

# The three things cargo-deb gets wrong or cannot produce: native
# changelog name, md5sums, empty Depends. Idempotent, in place.
bash scripts/deb-finalize.sh "${debs[@]}"

for deb in "${debs[@]}"; do
    echo "[build-deb] produced: ${deb}"
done
