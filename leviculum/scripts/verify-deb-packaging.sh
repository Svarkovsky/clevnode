#!/usr/bin/env bash
#
# Structural verification of the Debian packages built by `just build-deb`.
# Asserts what the .deb *claims* and what it *contains*, without installing
# anything: package identity and version, the files that must be inside,
# conffile registration, systemd unit validity, and maintainer-script
# syntax.
#
# The version assertions are the point of this script as much as the file
# lists are: the three packages are versioned independently, and nothing
# else in the build fails loudly if they silently collapse back onto one
# shared version.
#
# Usage:
#   scripts/verify-deb-packaging.sh [ARCH]
# ARCH defaults to amd64. Pass arm64 to check the cross-built packages.
# Architectures with no .deb present are reported as SKIP, not failure,
# so a host without the zig cross toolchain still verifies what it built.
#
# Requires: dpkg-deb (dpkg), and optionally systemd-analyze and file,
# whose checks are skipped when they are absent.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ARCH="${1:-amd64}"
DEB_DIR="target/debian"
FAILURES=0
CHECKS=0

pass() {
    CHECKS=$((CHECKS + 1))
    echo "  ok   $1"
}

fail() {
    CHECKS=$((CHECKS + 1))
    FAILURES=$((FAILURES + 1))
    echo "  FAIL $1"
}

skip() {
    echo "  skip $1"
}

# Assert that `haystack` contains `needle`, describing the check as `what`.
contains() {
    local haystack="$1" needle="$2" what="$3"
    case "$haystack" in
        *"$needle"*) pass "$what" ;;
        *) fail "$what (missing: ${needle})" ;;
    esac
}

# Locate the single .deb for a package and architecture. Prints the path,
# or nothing when there is none.
find_deb() {
    local pkg="$1"
    ls -1t "$DEB_DIR/${pkg}"_*_"${ARCH}".deb 2>/dev/null | head -n1
}

# ---------------------------------------------------------------------
# Per-package checks
# ---------------------------------------------------------------------

# check_package <deb-name> <crate> <expected-binaries...>
check_package() {
    local pkg="$1" crate="$2"
    shift 2
    local binaries=("$@")

    echo "== ${pkg} (${ARCH})"

    local deb
    deb="$(find_deb "$pkg")"
    if [ -z "$deb" ]; then
        skip "no ${pkg} .deb for ${ARCH} under ${DEB_DIR}/"
        return 0
    fi
    echo "  file: $deb"

    # --- control metadata ---
    local name version arch
    name="$(dpkg-deb -f "$deb" Package)"
    version="$(dpkg-deb -f "$deb" Version)"
    arch="$(dpkg-deb -f "$deb" Architecture)"

    [ "$name" = "$pkg" ] && pass "Package is ${pkg}" || fail "Package is ${name}, expected ${pkg}"
    [ "$arch" = "$ARCH" ] && pass "Architecture is ${ARCH}" || fail "Architecture is ${arch}, expected ${ARCH}"

    # The version must match the stamp file for this crate exactly. That
    # is what proves the per-package derivation reached cargo-deb rather
    # than one shared string being handed to all three.
    local stamp=".deb-version-${crate}"
    if [ -r "$stamp" ]; then
        local expected
        expected="$(cat "$stamp")"
        [ "$version" = "$expected" ] \
            && pass "Version is ${version}" \
            || fail "Version is ${version}, stamp says ${expected}"
    else
        skip "no ${stamp}; cannot check version (ran build-deb?)"
    fi

    # --- payload ---
    local contents
    contents="$(dpkg-deb -c "$deb")"
    local bin
    for bin in "${binaries[@]}"; do
        contains "$contents" " ./usr/bin/${bin}" "ships /usr/bin/${bin}"
    done
    contains "$contents" "/usr/share/doc/${pkg}/README.md" "ships its README"

    # Codeberg #288: the binaries are musl-static, so every MIT- and
    # BSD-licensed crate travels inside them. Both families require the
    # notice to accompany a binary distribution, and the package's own
    # copyright file (from license-file) carries our AGPL only. A .deb
    # without this file is a notice violation, not a cosmetic gap.
    contains "$contents" "/usr/share/doc/${pkg}/THIRD-PARTY-NOTICES" \
        "ships third-party licence notices"

    # Policy 12.7: every package installs a changelog, compressed. These
    # versions carry no Debian revision, which makes the packages native,
    # and a native package's changelog is changelog.gz — the .Debian
    # infix belongs to non-native ones only. deb-finalize.sh renames what
    # cargo-deb wrote; assert the result, not the intermediate.
    contains "$contents" "/usr/share/doc/${pkg}/changelog.gz" \
        "ships a changelog under its native name"
    if echo "$contents" | grep -q "/usr/share/doc/${pkg}/changelog.Debian.gz"; then
        fail "changelog still carries the non-native .Debian infix"
    fi

    # Documented lintian overrides for the tags a musl-static binary
    # always trips. Without them a lintian run reports errors that are
    # deliberate build choices, and real findings drown in the noise.
    contains "$contents" "/usr/share/lintian/overrides/${pkg}" \
        "ships its lintian overrides"

    # Policy 12.1: every program gets a manual page, and man pages are
    # installed compressed. cargo-deb does the gzipping, so a plain .1
    # here means the asset landed outside usr/share/man/.
    for bin in "${binaries[@]}"; do
        contains "$contents" "/usr/share/man/man1/${bin}.1.gz" \
            "ships a manual page for ${bin}"
    done

    # --- static linkage ---
    # A musl-static binary must have no interpreter. A dynamically linked
    # one would still install but fail on hosts with an older glibc, which
    # is exactly what these packages promise not to do.
    if command -v file >/dev/null 2>&1; then
        local tmp
        tmp="$(mktemp -d)"
        dpkg-deb -x "$deb" "$tmp"
        local desc
        desc="$(file "$tmp/usr/bin/${binaries[0]}")"
        # Both spellings mean no interpreter: plain static, and static-pie
        # for a position-independent static binary, which is what rustc
        # produces for the musl targets by default.
        case "$desc" in
            *"statically linked"* | *"static-pie linked"*)
                pass "/usr/bin/${binaries[0]} is statically linked" ;;
            *) fail "/usr/bin/${binaries[0]} is not static: ${desc#*: }" ;;
        esac
        rm -rf "$tmp"
    else
        skip "file(1) not installed; cannot check static linkage"
    fi

    # --- maintainer scripts ---
    local ctrl
    ctrl="$(mktemp -d)"
    dpkg-deb -e "$deb" "$ctrl"
    local script
    for script in preinst postinst prerm postrm; do
        [ -f "$ctrl/$script" ] || continue
        if sh -n "$ctrl/$script" 2>/dev/null; then
            pass "${script} is syntactically valid"
        else
            fail "${script} has a syntax error"
        fi
        # cargo-deb replaces the debhelper marker with generated service
        # handling. A leftover literal marker means the substitution did
        # not happen and the service would never be enabled.
        if grep -q 'DEBHELPER' "$ctrl/$script"; then
            fail "${script} still contains an unsubstituted debhelper marker"
        else
            pass "${script} has no unsubstituted debhelper marker"
        fi
    done
    # Nothing beyond this set belongs in a control archive. cargo-deb sweeps
    # the `maintainer-scripts` directory for every name Debian reserves, so a
    # data file sharing one of those names ships twice: once at its intended
    # install path, and once as an executable control script. `config` is the
    # trap in practice — dpkg-preconfigure runs it as /bin/sh. The syntax
    # loop above cannot catch that, because a TOML config parses as valid
    # shell and merely exits non-zero at the first line it tries to run.
    local member stray=0
    for member in "$ctrl"/*; do
        [ -e "$member" ] || continue
        case "$(basename "$member")" in
        control | conffiles | md5sums | preinst | postinst | prerm | postrm | triggers | shlibs) ;;
        *)
            fail "unexpected control archive member: $(basename "$member")"
            stray=1
            ;;
        esac
    done
    if [ "$stray" -eq 0 ]; then
        pass "control archive has no unexpected members"
    fi

    # md5sums is what `dpkg -V` and `debsums` verify installed files
    # against. cargo-deb cannot write it; scripts/deb-finalize.sh adds it
    # after the build, so its absence means that step was skipped.
    if [ -f "$ctrl/md5sums" ]; then
        local missing_sums=0
        for bin in "${binaries[@]}"; do
            if ! grep -q " usr/bin/${bin}\$" "$ctrl/md5sums"; then
                fail "md5sums has no entry for usr/bin/${bin}"
                missing_sums=1
            fi
        done
        if [ "$missing_sums" -eq 0 ]; then
            pass "md5sums covers every shipped binary"
        fi
        # Conffiles are excluded by convention, since the admin is
        # expected to edit them and a mismatch there is not a finding.
        local conf
        if [ -f "$ctrl/conffiles" ]; then
            while read -r conf; do
                [ -n "$conf" ] || continue
                if grep -q " ${conf#/}\$" "$ctrl/md5sums"; then
                    fail "md5sums lists conffile ${conf}"
                fi
            done <"$ctrl/conffiles"
        fi
    else
        fail "no md5sums control file (did scripts/deb-finalize.sh run?)"
    fi

    # An empty field is malformed: a package with no dependencies omits
    # Depends rather than shipping it blank. cargo-deb emits it either
    # way, so deb-finalize.sh strips it.
    if grep -qE '^[A-Za-z0-9-]+:[[:space:]]*$' "$ctrl/control"; then
        fail "control has an empty field: $(grep -m1 -E '^[A-Za-z0-9-]+:[[:space:]]*$' "$ctrl/control")"
    else
        pass "control has no empty fields"
    fi
    # --- lintian ---
    # The authority on Debian policy, so let it speak rather than
    # re-implementing its checks here. Errors fail; warnings are printed
    # but do not, because some are judgement calls the maintainer has
    # taken deliberately. Tags the build knowingly triggers belong in
    # packaging/lintian/<pkg> instead of being filtered here.
    if command -v lintian >/dev/null 2>&1; then
        local lint_out
        lint_out="$(lintian --tag-display-limit 0 "$deb" 2>/dev/null | grep -v '^N:' || true)"
        local lint_errors
        lint_errors="$(echo "$lint_out" | grep -c '^E:' || true)"
        if [ "$lint_errors" -gt 0 ]; then
            echo "$lint_out" | grep '^E:' | sed 's/^/      /'
            fail "lintian reports ${lint_errors} error(s)"
        else
            pass "lintian reports no errors"
        fi
        if echo "$lint_out" | grep -q '^W:'; then
            echo "$lint_out" | grep '^W:' | sed 's/^/      note: /'
        fi
    else
        skip "lintian not installed; policy checks not run"
    fi

    if [ -f "$ctrl/conffiles" ]; then
        echo "  conffiles: $(tr '\n' ' ' <"$ctrl/conffiles")"
    fi
    CONFFILES="$(cat "$ctrl/conffiles" 2>/dev/null || true)"
    CONTENTS="$contents"
    CTRL_DIR="$ctrl"
}

# ---------------------------------------------------------------------

echo "verify-deb-packaging: arch=${ARCH}"
echo

check_package leviculum leviculum-cli lnsd lnstest lncp lnstatus lnprobe
contains "$CONFFILES" "/etc/reticulum/config" "registers /etc/reticulum/config as a conffile"
contains "$CONTENTS" "lnsd.service" "ships the lnsd systemd unit"
rm -rf "$CTRL_DIR"
echo

check_package lnomad lnomad lnomad
# lnomad is deliberately service-free: it is a terminal browser, and
# installing it must not add a unit to the system.
if printf '%s' "$CONTENTS" | grep -q 'systemd'; then
    fail "lnomad ships a systemd unit, which it must not"
else
    pass "ships no systemd unit"
fi
rm -rf "$CTRL_DIR"
echo

check_package lblogd lblogd lblogd
contains "$CONFFILES" "/etc/lblogd/config.toml" "registers /etc/lblogd/config.toml as a conffile"
contains "$CONTENTS" "/etc/lblogd/config.toml" "ships the default config"
contains "$CONTENTS" "lblogd.service" "ships the lblogd systemd unit"
rm -rf "$CTRL_DIR"
echo

# ---------------------------------------------------------------------
# Unit files, checked at the source rather than per package: the same
# file goes into every architecture's .deb.
# ---------------------------------------------------------------------

echo "== systemd units"
if command -v systemd-analyze >/dev/null 2>&1; then
    for unit in packaging/debian/lnsd.service packaging/lblogd/lblogd.service; do
        # `verify` resolves the unit against the *build host*, which has
        # neither the units these order themselves after nor (necessarily)
        # the binary they start. Both are properties of the host, not of
        # the package, so those two diagnostics are filtered out; every
        # other complaint — an unknown directive, a malformed value — is a
        # real defect and fails. That the ExecStart binary is actually
        # shipped is asserted below, against the package rather than the
        # host, which is the stronger check anyway.
        output="$(systemd-analyze verify "$unit" 2>&1 || true)"
        filtered="$(printf '%s' "$output" \
            | grep -v -e 'Cannot find unit' -e 'is not executable' -e '^$' || true)"
        if [ -z "$filtered" ]; then
            pass "$(basename "$unit") is valid"
        else
            fail "$(basename "$unit"): ${filtered}"
        fi
    done
else
    skip "systemd-analyze not installed; cannot verify units"
fi

# The unit's ExecStart must name a path the package actually installs. A
# typo here would leave a service that installs cleanly, starts, and
# immediately fails with status=203/EXEC.
for pair in "packaging/debian/lnsd.service:leviculum" "packaging/lblogd/lblogd.service:lblogd"; do
    unit="${pair%%:*}"
    pkg="${pair##*:}"
    deb="$(find_deb "$pkg")"
    if [ -z "$deb" ]; then
        skip "no ${pkg} .deb; cannot check its ExecStart path"
        continue
    fi
    # A unit that shells out in ExecReload depends on that binary being
    # installed. /bin/kill is a procps binary, not a shell builtin, and
    # procps is only Priority: important — a minimal install can lack it,
    # in which case `systemctl reload` fails 203/EXEC while the service
    # otherwise looks perfectly healthy. Caught in a container test on
    # 2026-07-28; asserted here so the declaration cannot be dropped.
    if grep -q '^ExecReload=/bin/kill' "$unit"; then
        deb_for_reload="$(find_deb "$pkg")"
        if [ -n "$deb_for_reload" ]; then
            deps="$(dpkg-deb -f "$deb_for_reload" Depends)"
            case "$deps" in
                *procps*) pass "$(basename "$unit") uses /bin/kill and ${pkg} depends on procps" ;;
                *) fail "$(basename "$unit") runs /bin/kill but ${pkg} does not depend on procps" ;;
            esac
        fi
    fi

    exec_path="$(sed -n 's/^ExecStart=\([^ ]*\).*/\1/p' "$unit" | head -n1)"
    # Listed into a variable before grepping: `grep -q` exits at the first
    # match, and under `set -o pipefail` the SIGPIPE it delivers to
    # dpkg-deb would fail the pipeline precisely when the match succeeds.
    listing="$(dpkg-deb -c "$deb")"
    if printf '%s\n' "$listing" | grep -q " \.${exec_path}\$"; then
        pass "$(basename "$unit") starts ${exec_path}, which ${pkg} ships"
    else
        fail "$(basename "$unit") starts ${exec_path}, which ${pkg} does not ship"
    fi
done
echo

# ---------------------------------------------------------------------
# The independent-versioning invariant itself.
# ---------------------------------------------------------------------

echo "== independent versions"
lev_deb="$(find_deb leviculum)"
blog_deb="$(find_deb lblogd)"
if [ -n "$lev_deb" ] && [ -n "$blog_deb" ]; then
    lev_v="$(dpkg-deb -f "$lev_deb" Version | sed 's/~.*//')"
    blog_v="$(dpkg-deb -f "$blog_deb" Version | sed 's/~.*//')"
    if [ "$lev_v" != "$blog_v" ]; then
        pass "leviculum ${lev_v} and lblogd ${blog_v} version separately"
    else
        # Not impossible in principle — two independent versions may
        # coincide — but today they differ, and a silent collapse back
        # onto the workspace version would look exactly like this.
        fail "leviculum and lblogd both report ${lev_v}: did the versions re-couple?"
    fi
else
    skip "need both leviculum and lblogd .debs to compare versions"
fi
echo

echo "verify-deb-packaging: ${CHECKS} checks, ${FAILURES} failed"
[ "$FAILURES" -eq 0 ]
