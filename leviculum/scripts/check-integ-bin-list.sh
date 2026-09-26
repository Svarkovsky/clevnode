#!/bin/bash
# One list of integration binaries, and it lives in `build-integ-bins`.
#
# The Justfile recipe `build-integ-bins` names every binary periculum mounts
# into its node containers, and `periculum check-freshness` asserts each of
# them is newer than the last production-source change. A caller that spells
# out its own `cargo build --bin ...` line instead of calling the recipe has
# made a second copy of that list, and the two drift silently: the recipe
# grew `--bin lxmf-node`, scripts/run-tier3-hw.sh did not, and from then on
# every hardware nightly that followed a source change died in the freshness
# preflight naming a binary nothing had built (2026-08-19).
#
# The failure is invisible until it fires, and it fires on the rig at 03:37.
# So the shape is banned by name in the two callers that mount those
# binaries: neither may carry a `cargo build` line with `--bin`/`--bins` on
# it. Both must go through the recipe.
#
# Scope is deliberately narrow — two named files, not a tree sweep. Any other
# `cargo build --bin` in the tree (a developer convenience, a doc example) is
# nobody's second list, and a guard that shouts about those gets disabled.
#
# `cargo build --release` with no `--bin` is fine and appears in
# run-tier3-hw.sh: that one builds periculum itself, in periculum's own
# checkout, which has nothing to do with this list.
#
# In a gate rather than a `#[test]` for the reason the whole check-* family
# is: it reads files that no test binary compiles, and it must run on the
# push path where the author still has the file open.
#
# Exit 0 = both files clean. Exit 1 = a banned line, or the checker's own
# self-test failed.
#
# Usage:
#   bash scripts/check-integ-bin-list.sh
set -uo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR" || exit 1

# The callers that mount the integ binaries. Scoped by name: this is a ban on
# a specific duplication, not a style rule about cargo invocations.
GUARDED_FILES=(
    scripts/run-tier3-hw.sh
    .woodpecker/nightly.yml
)

# Print `file:line: text` for every banned line in $1, and nothing otherwise.
#
# Backslash continuations are joined first, so a `cargo build --release \`
# split across two lines is read as the one command it is; the reported line
# number is where the command starts. Comment lines are skipped after the
# join — a commented-out cargo line builds nothing. The leading `- ` of a
# YAML sequence entry is not a comment marker and is stripped before the
# comment test.
banned_lines() {
    awk '
    {
        line = $0
        start = FNR
        while (line ~ /\\$/ && (getline nxt) > 0) {
            sub(/\\$/, "", line)
            line = line nxt
        }
        head = line
        sub(/^[ \t]*(-[ \t]+)?/, "", head)
        if (substr(head, 1, 1) == "#") next
        if (line ~ /cargo[ \t]+build/ && line ~ /--bins?([ \t=]|$)/) {
            sub(/^[ \t]+/, "", line)
            printf "%s:%d: %s\n", FILENAME, start, line
        }
    }
    ' "$1"
}

# --- Self-test ------------------------------------------------------------
#
# "No banned line found" is satisfied forever by a checker that stopped
# looking, so the classifier is exercised on fixtures before it is allowed to
# say anything about the tree: three shapes it must catch, four it must not.
# A checker that gets any of the seven wrong fails the gate here rather than
# passing the tree silently.
SELFTEST_DIR="$(mktemp -d)"
trap 'rm -rf "$SELFTEST_DIR"' EXIT

cat > "$SELFTEST_DIR/bad-plain" <<'EOF'
cargo build --release --bin lnsd --bin lnstest --bin lncp
EOF

cat > "$SELFTEST_DIR/bad-continued" <<'EOF'
CARGO_TARGET_DIR="$CACHE_TARGET" CARGO_INCREMENTAL=0 \
  cargo build --release --bin lnsd
EOF

cat > "$SELFTEST_DIR/bad-yaml" <<'EOF'
steps:
  build:
    commands:
      - cargo build --release --bins
EOF

cat > "$SELFTEST_DIR/good" <<'EOF'
# cargo build --release --bin lnsd   (what this used to be)
      - # cargo build --release --bin lncp
( cd "$REPO_DIR" && cargo build --release )
just build-integ-bins
EOF

selftest_failed=0
for fixture in bad-plain bad-continued bad-yaml; do
    if [ -z "$(banned_lines "$SELFTEST_DIR/$fixture")" ]; then
        echo "check-integ-bin-list: SELF-TEST FAILED — fixture '$fixture' was not caught."
        selftest_failed=1
    fi
done
if [ -n "$(banned_lines "$SELFTEST_DIR/good")" ]; then
    echo "check-integ-bin-list: SELF-TEST FAILED — clean fixture was flagged:"
    banned_lines "$SELFTEST_DIR/good"
    selftest_failed=1
fi
if [ "$selftest_failed" -ne 0 ]; then
    echo "The checker itself is broken; its verdict on the tree means nothing."
    exit 1
fi

# --- The gate -------------------------------------------------------------

found=0
for f in "${GUARDED_FILES[@]}"; do
    if [ ! -f "$f" ]; then
        # A guarded file that vanished or was renamed silently disables half
        # this check, so it is a failure and not a skip.
        echo "check-integ-bin-list: FAILED — guarded file '$f' does not exist."
        echo "It was renamed or removed: point GUARDED_FILES at its replacement,"
        echo "or drop the entry deliberately."
        found=1
        continue
    fi
    hits="$(banned_lines "$f")"
    if [ -n "$hits" ]; then
        echo "$hits"
        found=1
    fi
done

if [ "$found" -ne 0 ]; then
    echo
    echo "check-integ-bin-list: FAILED — a second list of integration binaries."
    echo "The list lives in the Justfile recipe 'build-integ-bins' and nowhere"
    echo "else. Call the recipe:"
    echo
    echo "    ( cd \"\$REPO_DIR\" && CARGO_TARGET_DIR=\"\$CACHE_TARGET\" \\"
    echo "      CARGO_INCREMENTAL=0 just build-integ-bins )"
    echo
    echo "A copied list drifts from the one 'periculum check-freshness' asserts"
    echo "against, and the drift only shows up as a hardware nightly aborting in"
    echo "its preflight (2026-08-19)."
    exit 1
fi

echo "check-integ-bin-list: OK — ${#GUARDED_FILES[@]} files build via build-integ-bins."
exit 0
