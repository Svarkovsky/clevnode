#!/usr/bin/env bash
# Pin the build id and the per-package Debian versions for one nightly
# build, and persist them so every later step sees identical values.
#
# Why a script rather than inline commands: this used to be duplicated
# between .woodpecker/nightly.yml and the Justfile's _deb-stamp recipe,
# and the two drifted (the Justfile still built only the leviculum
# package long after CI had added lnomad). One implementation, two
# callers.
#
# Produces, in the repo root:
#   .build-id                    nightly.<UTCdate>-<sha7>
#   .deb-version-<crate>         <crate version>~nightly.<UTCdate>.<sha7>
#
# and, under target/deb-changelog/:
#   <binary package name>        a Debian-format changelog for that build
#
# The changelog is generated rather than committed because Debian policy
# wants its top entry to carry the package's own version, and these
# versions are stamped per build. A committed file would go stale on the
# first nightly and stay wrong.
#
# Both are gitignored. The build id is deliberately shared across all
# packages: it stamps *when and from what commit* a build came, which is
# one fact per run. The Debian versions are per package, because the
# packages are versioned independently — leviculum tracks the protocol
# stack, lblogd and lnomad track their own products.
#
# The `~` before "nightly" is not decoration: in Debian version ordering
# `~` sorts BEFORE the empty string, so 0.1.0~nightly.20260728.abc1234
# compares as older than a future release 0.1.0. Nightlies therefore
# upgrade cleanly into a release without an epoch bump.
#
# Commit and date come from CI when present and from git/date otherwise,
# so a local `just build-deb` produces the same shape as CI.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Every crate that ships a Debian package. Keyed by crate name, which is
# what `cargo deb -p` and `cargo pkgid -p` both take; the resulting .deb
# may be named differently (leviculum-cli ships as "leviculum").
CRATES=(leviculum-cli lnomad lblogd)

# The .deb is not always named after its crate: leviculum-cli ships as
# "leviculum". The changelog's first token must be the *binary package*
# name, so the mapping is spelled out here rather than assumed.
pkg_name() {
    case "$1" in
    leviculum-cli) echo leviculum ;;
    *) echo "$1" ;;
    esac
}

SHA="${CI_COMMIT_SHA:-$(git rev-parse HEAD)}"
SHA7="$(printf '%.7s' "$SHA")"
DATE="$(date -u +%Y%m%d)"
# RFC 5322, which is what a Debian changelog trailer takes. Honour
# SOURCE_DATE_EPOCH so a reproducible build gets a stable timestamp.
if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
    STAMP="$(date -uR -d "@${SOURCE_DATE_EPOCH}")"
else
    STAMP="$(date -uR)"
fi
MAINTAINER="Lew Palm <lp@lew-palm.de>"
CHANGELOG_DIR="target/deb-changelog"

echo "nightly.${DATE}-${SHA7}" >.build-id

for crate in "${CRATES[@]}"; do
    # `cargo pkgid` resolves the version through cargo itself rather than
    # by grepping a manifest, which is what makes per-package versions
    # work at all: three crates, three answers, no assumption that any of
    # them equals the workspace version.
    version="$(cargo pkgid -p "$crate" | sed 's/.*[#@]//')"
    if [ -z "$version" ]; then
        echo "error: could not resolve a version for crate ${crate}" >&2
        exit 1
    fi
    deb_version="${version}~nightly.${DATE}.${SHA7}"
    echo "$deb_version" >".deb-version-${crate}"

    mkdir -p "$CHANGELOG_DIR"
    pkg="$(pkg_name "$crate")"
    cat >"${CHANGELOG_DIR}/${pkg}" <<EOF
${pkg} (${deb_version}) unstable; urgency=medium

  * Nightly build from commit ${SHA7}.

 -- ${MAINTAINER}  ${STAMP}
EOF
done

echo "[deb-stamp] build-id=$(cat .build-id)"
for crate in "${CRATES[@]}"; do
    echo "[deb-stamp] ${crate}=$(cat ".deb-version-${crate}")"
done
