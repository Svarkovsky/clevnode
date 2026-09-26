#!/bin/bash
# Guarantee C, step 1: the vendored references are at the commit the
# gitlink names.
#
# `reference/LXMF` sat twelve commits behind its gitlink for five weeks.
# Nothing was wrong with any individual citation; one fact was wrong, and
# every LXMF `file:line` in the tree quietly meant something else. The
# catch is O(1): `git submodule status` marks a disagreeing submodule with
# `+` (checked out somewhere other than the gitlink), `-` (not checked out
# at all) or `U` (merge conflict).
#
# This lives in a gate, not in a `#[test]`, on purpose. The `reference_lock`
# test that should have said so was itself red and unobserved for the whole
# drift -- a test can be the thing that runs nowhere. See
# docs/src/concepts/checks-and-citations.md.
#
# Exit 0 = all four references agree with their gitlinks. Exit 1 = at least
# one does not, or the checker itself is broken.
set -uo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR" || exit 1

# The vendored references citations resolve against. A submodule added
# later and not listed here is reported by the inventory check below, so
# this list cannot silently fall behind .gitmodules.
EXPECTED_SUBMODULES=(
    reference/Reticulum
    reference/LXMF
    reference/LXST
    reference/RNode_Firmware
)

# The gitlink commit, i.e. what the tree says the submodule should be at.
# Printed in the failure message so the reader can act without a second
# command. Best-effort: an unreadable gitlink prints as `<unknown>` rather
# than aborting the check.
gitlink_commit() {
    git ls-tree HEAD -- "$1" 2>/dev/null | awk '$2 == "commit" { print $3 }'
}

# Classify one `git submodule status` line. Prints `<flag>\t<sha>\t<path>`
# for a line that disagrees with its gitlink, nothing for a clean one.
#
# Format: a single leading flag column (space, `+`, `-` or `U`), then the
# commit, then the path, then an optional `(describe)`.
classify_line() {
    local line="$1" flag rest sha path
    [ -n "$line" ] || return 0
    flag="${line:0:1}"
    rest="${line:1}"
    sha="${rest%% *}"
    rest="${rest#* }"
    path="${rest%% *}"
    case "$flag" in
        ' ') return 0 ;;
        '+' | '-' | 'U') printf '%s\t%s\t%s\n' "$flag" "$sha" "$path" ;;
        *)
            printf 'BAD\t%s\t%s\n' "$sha" "$line"
            ;;
    esac
}

classify_status() {
    local line
    while IFS= read -r line; do
        classify_line "$line"
    done
}

# --- standing canary -------------------------------------------------
#
# Runs before anything is reported about the real tree. A gate that stops
# matching -- a parser that returns nothing because the status format
# shifted -- is green forever, which is the defect this check exists to
# remove. A one-time demonstration at implementation time decays; this one
# runs on every batch.
canary() {
    local dirty clean
    dirty=$(printf '%s\n' \
        '+1111111111111111111111111111111111111111 reference/LXMF (1.0.9-3-g1111111)' \
        | classify_status)
    if [ "$dirty" != "$(printf '+\t1111111111111111111111111111111111111111\treference/LXMF')" ]; then
        echo "check-submodule-pins: CANARY FAILED -- a deliberately mismatched" >&2
        echo "  submodule line was not reported. The checker cannot see the defect" >&2
        echo "  it exists to catch; fix the parser, do not skip this." >&2
        echo "  got: ${dirty:-<nothing>}" >&2
        return 1
    fi
    clean=$(printf '%s\n' \
        ' 2222222222222222222222222222222222222222 reference/LXMF (1.1.0)' \
        | classify_status)
    if [ -n "$clean" ]; then
        echo "check-submodule-pins: CANARY FAILED -- a clean submodule line was" >&2
        echo "  reported as a mismatch. The checker fails on correct trees." >&2
        echo "  got: $clean" >&2
        return 1
    fi
    return 0
}

canary || exit 1

# --- the real check --------------------------------------------------

status_out=$(git submodule status 2>&1)
if [ $? -ne 0 ]; then
    echo "check-submodule-pins: \`git submodule status\` failed:" >&2
    echo "$status_out" >&2
    exit 1
fi

# Inventory first: a reference that vanished from the status output would
# otherwise pass by producing no lines at all.
missing_from_status=()
for sub in "${EXPECTED_SUBMODULES[@]}"; do
    grep -qE "^.[0-9a-f]+ ${sub}( |$)" <<<"$status_out" || missing_from_status+=("$sub")
done
if [ ${#missing_from_status[@]} -gt 0 ]; then
    echo "check-submodule-pins: FAILED" >&2
    echo "" >&2
    for sub in "${missing_from_status[@]}"; do
        echo "  $sub is not listed by \`git submodule status\`." >&2
        echo "    Either the submodule was removed (then drop it from" >&2
        echo "    EXPECTED_SUBMODULES in this script and from any citation that" >&2
        echo "    resolves against it), or .gitmodules is out of sync with the" >&2
        echo "    working tree." >&2
        echo "" >&2
    done
    exit 1
fi

offenders=$(classify_status <<<"$status_out" | grep -F -f <(printf '%s\n' "${EXPECTED_SUBMODULES[@]}") || true)

if [ -z "$offenders" ]; then
    echo "check-submodule-pins: OK -- ${#EXPECTED_SUBMODULES[@]} vendored references match their gitlinks"
    exit 0
fi

echo "check-submodule-pins: FAILED" >&2
echo "" >&2
echo "A vendored reference is not at the commit this tree pins. Every" >&2
echo "\`file:line\` citation into it means something other than what it says," >&2
echo "and every test that reads it is testing a different snapshot." >&2
echo "" >&2

while IFS=$'\t' read -r flag sha path; do
    expected=$(gitlink_commit "$path")
    expected=${expected:-<unknown>}
    case "$flag" in
        '+')
            echo "  $path" >&2
            echo "    gitlink (this tree pins): $expected" >&2
            echo "    checked out:              $sha" >&2
            echo "    Two intents, pick the one you meant:" >&2
            echo "      - the checkout is accidental -> restore the gitlink:" >&2
            echo "          git -C $path checkout $expected" >&2
            echo "      - the bump is deliberate -> commit it, and re-verify the" >&2
            echo "        citations into this reference before you do:" >&2
            echo "          git add $path && git commit" >&2
            echo "    Do NOT reflexively run \`git submodule update --init\`: it" >&2
            echo "    discards a deliberate local checkout without saying so." >&2
            ;;
        '-')
            echo "  $path" >&2
            echo "    gitlink (this tree pins): $expected" >&2
            echo "    not checked out (no working tree)" >&2
            echo "    Citations into this reference cannot be verified at all." >&2
            echo "    Nothing local is at risk here, so initialise it:" >&2
            echo "          git submodule update --init $path" >&2
            ;;
        'U')
            echo "  $path" >&2
            echo "    gitlink (this tree pins): $expected" >&2
            echo "    unmerged: the submodule is in a merge conflict" >&2
            echo "    Resolve the conflict by choosing the commit you mean, then" >&2
            echo "    \`git add $path\`. Until then no citation into it is verifiable." >&2
            ;;
        *)
            echo "  unparseable \`git submodule status\` line: $path" >&2
            echo "    The status format changed under the checker. Fix the parser." >&2
            ;;
    esac
    echo "" >&2
done <<<"$offenders"

exit 1
