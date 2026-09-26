#!/usr/bin/env bash
# Codeberg #196: the in-driver core processor seam (design B).
#
# The core-lock budget (docs/src/concepts/core-lock-budget.md) forbids two
# things to anything the driver's event loop calls: it may not `.await`, and it
# may not call back into the driver's public async API, because those methods
# end in `action_dispatch_tx.send(output).await` on a bounded channel the same
# loop drains. A prohibition that lives in a doc comment is one this project has
# repeatedly found to be no rule at all, so the seam's signature is shaped to
# make both unrepresentable — and this gate is what proves it stays that way.
#
# Each fixture under leviculum-std/tests/cf_*.rs is a `CoreProcessor` impl that
# attempts one forbidden move. Every one of them must fail to compile, and must
# fail with the SPECIFIC error named here: "does not compile" is not the
# assertion, "does not compile FOR THIS REASON" is. A fixture that started
# failing on a typo would still be red, and would tell us nothing.
#
# In a gate rather than a #[test] on purpose (the same reasoning as
# check-submodules): a #[test] cannot make its own crate fail to compile, and
# shelling out to cargo from inside `cargo test` blocks on the build-directory
# lock the outer invocation holds.
#
# Error text is matched by code plus a stable substring of the primary message,
# not by a full stderr snapshot. rust-toolchain.toml floats on `stable`, and a
# snapshot would go red on every rustc release that reworded a note.
#
# The pinned code must be the ONLY code emitted, which is load-bearing rather
# than tidiness. E0728 is raised during AST lowering, before name resolution
# has to succeed: while the seam did not exist yet, both `await` fixtures
# emitted E0728 *and* an unresolved-import E0432 for `CoreProcessor`, so a
# "contains E0728" assertion was green against a trait that was not there.
# Requiring the exact set is what makes these fixtures test the seam.

set -euo pipefail

cd "$(dirname "$0")/.."

FEATURE=__compile_fail_fixtures

# fixture:error-code:required substrings of the primary diagnostics, `|`-separated.
#
# Every substring must be present. cf_dispatch_from_handle attempts one route
# per type that can reach the dispatch channel, so it names three: a fixture
# that still fails to compile because *one* route is refused would otherwise
# keep asserting a claim about all three.
#
# The code may be the sentinel LINT instead of an E-number, for a fixture whose
# refusal comes from a `deny`d lint rather than from a type error (`#[must_use]`
# is a warning by default; the fixture's own `#![deny]` is what makes it
# fatal). Such a fixture must emit NO `error[E…]` at all — same exact-set
# discipline as above, read the other way round: a lint fixture that started
# failing on a renamed import would emit E0432 and be caught, instead of
# quietly keeping its claim alive.
CASES=(
    "cf_await_in_processor:E0728:\`await\` is only allowed inside \`async\` functions and blocks"
    "cf_dispatch_from_handle:E0599:no method named \`packet_sender\` found|no method named \`link_handle\` found|no method named \`node\` found"
    "cf_stamp_in_processor:E0728:\`await\` is only allowed inside \`async\` functions and blocks"
    "cf_drop_dispatch_result:LINT:unused \`DispatchResult\` that must be used"
)

rc=0
for case in "${CASES[@]}"; do
    target="${case%%:*}"
    rest="${case#*:}"
    code="${rest%%:*}"
    needle="${rest#*:}"

    # Left operand of `&&`, so `set -e` does not fire on the expected failure.
    # A non-compile failure (bad feature, missing target) emits no `error[E…]`
    # code and is caught by the exact-set check below as "got none".
    out=$(cargo check -p leviculum-std --test "$target" --features "$FEATURE" 2>&1) && {
        echo "FAIL $target: compiled successfully — the seam no longer forbids this" >&2
        rc=1
        continue
    }

    # How this fixture's refusal is named in the gate's own output.
    if [ "$code" = LINT ]; then
        label="lint"
    else
        label="error[$code]"
    fi

    # `|| codes=""`: with `set -e -o pipefail`, a grep that matches nothing
    # aborts the script on the assignment itself. That made "got none" — the
    # case the comment above promises to report — a silent exit 1 instead.
    # It could not fire while every fixture was E-coded; a LINT fixture emits
    # no code by design and hits it every run.
    codes=$(grep -oE '^error\[E[0-9]+\]' <<<"$out" | sort -u | tr -d '\n') || codes=""
    if [ "$code" = LINT ]; then
        if [ -n "$codes" ]; then
            echo "FAIL $target: lint fixture must emit no error code, got $codes:" >&2
            grep -E '^error(\[E[0-9]+\])?:' <<<"$out" | head -5 >&2
            rc=1
            continue
        fi
    elif [ "$codes" != "error[$code]" ]; then
        echo "FAIL $target: expected exactly error[$code], got ${codes:-none}:" >&2
        grep -E '^error(\[E[0-9]+\])?:' <<<"$out" | head -5 >&2
        rc=1
        continue
    fi

    missing=""
    IFS='|' read -ra needles <<<"$needle"
    for want in "${needles[@]}"; do
        grep -qF "$want" <<<"$out" || missing+="       expected message to contain: $want"$'\n'
    done
    if [ -n "$missing" ]; then
        echo "FAIL $target: $label present but not the intended one" >&2
        printf '%s' "$missing" >&2
        grep -A1 -F "$label" <<<"$out" | head -8 >&2
        rc=1
        continue
    fi

    echo "ok   $target: $label — ${needles[0]}${needles[1]:+ (+$((${#needles[@]} - 1)) more)}"
done

if [ "$rc" -ne 0 ]; then
    echo "[check-processor-compile-fail] the core-processor seam no longer makes the" >&2
    echo "                              forbidden moves unrepresentable (Codeberg #196)" >&2
fi
exit "$rc"
