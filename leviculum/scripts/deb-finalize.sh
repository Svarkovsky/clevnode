#!/usr/bin/env bash
#
# Repair the three things cargo-deb gets wrong or cannot produce, in one
# unpack/repack pass over a finished .deb.
#
# 1. Changelog name. cargo-deb always installs changelog.Debian.gz, but a
#    version without a Debian revision describes a native package, whose
#    changelog is plain changelog.gz. lintian:
#    wrong-name-for-changelog-of-native-package.
#
# 2. md5sums. cargo-deb 3.6.3 writes control, conffiles, config, preinst,
#    prerm and postrm, and has no md5sums support at all — a gap in the
#    tool, not a misconfiguration. Debian expects the file: `dpkg -V` and
#    `debsums` verify installed files against it, and without it every
#    packaged file reports as unverifiable.
#
#    Conffiles are excluded, matching dh_md5sums: their whole point is
#    that the admin edits them, so a mismatch there is expected rather
#    than a finding.
#
# 3. Empty control fields. A package with no dependencies should omit
#    Depends entirely, but cargo-deb emits `Depends:` with an empty value
#    whether the manifest sets `depends = ""` or leaves it out. An empty
#    field is malformed, so it is dropped here.
#
# `--root-owner-group` on the repack is load-bearing: `dpkg-deb -R`
# restores file modes but leaves the tree owned by whoever ran it, and
# without that flag the rebuilt package would install files owned by the
# build user instead of root.
#
# Usage:
#   scripts/deb-finalize.sh PACKAGE.deb [PACKAGE.deb ...]
#
# Rewrites each package in place. Idempotent.

set -euo pipefail

if [ $# -eq 0 ]; then
    echo "usage: $0 PACKAGE.deb [PACKAGE.deb ...]" >&2
    exit 2
fi

for deb in "$@"; do
    if [ ! -f "$deb" ]; then
        echo "error: no such package: $deb" >&2
        exit 1
    fi

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    dpkg-deb -R "$deb" "$tmp/pkg"

    # --- 1. changelog name ----------------------------------------------
    # A Debian version without a revision (no hyphen) describes a *native*
    # package, and a native package's changelog is changelog.gz — only a
    # non-native one adds the .Debian infix. cargo-deb always writes the
    # .Debian form, which lintian flags as
    # wrong-name-for-changelog-of-native-package. These versions are
    # native (0.1.0~nightly.<date>.<sha>), so rename. Done before the
    # sums below, or md5sums would name a file that no longer exists.
    pkgname="$(awk '/^Package:/ {print $2; exit}' "$tmp/pkg/DEBIAN/control")"
    pkgversion="$(awk '/^Version:/ {print $2; exit}' "$tmp/pkg/DEBIAN/control")"
    docdir="$tmp/pkg/usr/share/doc/${pkgname}"
    if [ -f "$docdir/changelog.Debian.gz" ] && [ "${pkgversion##*-}" = "$pkgversion" ]; then
        mv "$docdir/changelog.Debian.gz" "$docdir/changelog.gz"
        echo "[deb-finalize] $(basename "$deb"): native package, changelog.Debian.gz -> changelog.gz"
    fi

    # --- 2. md5sums -----------------------------------------------------
    conffiles="$tmp/conffiles"
    if [ -f "$tmp/pkg/DEBIAN/conffiles" ]; then
        # Stored with a leading slash; strip it to match the relative
        # paths `find` produces below.
        sed 's|^/||' "$tmp/pkg/DEBIAN/conffiles" >"$conffiles"
    else
        : >"$conffiles"
    fi

    # Sorted, for a file that is stable across rebuilds. LC_ALL=C so the
    # order does not shift with the builder's locale. NUL separators keep
    # paths with spaces intact.
    (
        cd "$tmp/pkg"
        find . -path ./DEBIAN -prune -o -type f -print0 |
            sed -z 's|^\./||' |
            LC_ALL=C sort -z |
            while IFS= read -r -d '' file; do
                if grep -qxF "$file" "$conffiles"; then
                    continue
                fi
                printf '%s  %s\n' "$(md5sum "$file" | cut -d' ' -f1)" "$file"
            done
    ) >"$tmp/pkg/DEBIAN/md5sums"
    chmod 0644 "$tmp/pkg/DEBIAN/md5sums"

    # --- 3. empty control fields ---------------------------------------
    # A field line is `Name:` optionally followed by a value; continuation
    # lines start with whitespace. A field is dropped only when its value
    # is empty *and* no continuation line follows.
    awk '
        /^[A-Za-z0-9-]+:[[:space:]]*$/ {
            name = $0
            if ((getline next_line) > 0) {
                if (next_line ~ /^[[:space:]]/) { print name; print next_line }
                else { print next_line }
            }
            next
        }
        { print }
    ' "$tmp/pkg/DEBIAN/control" >"$tmp/control.new"
    mv "$tmp/control.new" "$tmp/pkg/DEBIAN/control"
    chmod 0644 "$tmp/pkg/DEBIAN/control"

    dpkg-deb --root-owner-group -Zxz -b "$tmp/pkg" "$deb" >/dev/null

    count="$(wc -l <"$tmp/pkg/DEBIAN/md5sums")"
    echo "[deb-finalize] $(basename "$deb"): md5sums for ${count} file(s)"

    rm -rf "$tmp"
    trap - EXIT
done
