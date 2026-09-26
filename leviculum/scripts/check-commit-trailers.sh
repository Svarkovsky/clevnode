#!/usr/bin/env bash
#
# Commit-message authorship guard (Codeberg #205).
#
# CONTRIBUTING.md: "no AI trailers. Commit under your real name and a
# reachable e-mail address." Until this script nothing enforced that in
# either repository, and on 2026-08-07 a commit carrying
# `Co-Authored-By: <model> <noreply@…>` landed in periculum, in a pass whose
# author had cited that exact rule, correctly, hours earlier. It was caught
# only because a human read the message before pushing, and amended it.
#
# This script is byte-identical in leviculum and periculum. The rule is
# stated in both CONTRIBUTING.md files in the same words, so the two
# repositories agree rather than one relying on the other.
#
# The rule is not unclear and it is not forgotten. It is unenforced *against
# a default*: the assistant harnesses used here instruct their agents to
# append that trailer to every commit. A rule that has to win against a
# default, with nothing behind it, stays perpetually half-kept, and its
# violation is invisible unless a person reads every message before every
# push. That is what this closes.
#
# WHOSE MESSAGE IT IS (policy 2026-06-13)
#
#   The rule the repository actually holds is: **a machine-authorship
#   trailer is a violation on our commits and not on anyone else's.** We do
#   not edit, and do not refuse, a message somebody outside the project
#   wrote -- their attribution is theirs. The commit-msg hook has keyed on
#   the author e-mail since 2026-06-13; until 2026-08-07 this script did
#   not, and the two rules could not both be satisfied: merging any
#   external contribution carrying such a trailer -- PR #201 is one -- made
#   `just fast`, the pre-push hook and the forge check permanently red on a
#   tree with nothing wrong with it. That was a defect in the brief for
#   #205, which argued the rule entirely in terms of our own harness
#   default and never mentioned external contributors.
#
#   Authorship is the discriminator and `git log --format=%ae` is the whole
#   mechanism. The `ours` lines in the baseline file are the set of
#   e-mails this project commits under.
#
#   An exemption nobody counts is an off switch: once foreign commits are
#   skipped by class, anyone can acquire the exemption by setting an author
#   e-mail. So the baseline file pins how many *foreign* commits above the
#   baseline carry a trailer, exactly as it pins `legacy`, and this
#   recomputes and compares it every run. A new one forces a diff and a
#   human look, which is the point -- we still want to know when it
#   happens, we just do not want to edit someone else's message.
#
# WHAT COUNTS AS A VIOLATION: a matching line at COLUMN 0.
#
#   An indented line is prose, not a trailer. This is the whole of the
#   quoting rule, and it is deliberate:
#
#   * Column 0 is where the default writes. A harness emitting a trailer is
#     emitting a real trailer -- git's `interpret-trailers`, Codeberg,
#     GitHub and GitLab all harvest from column 0 and nowhere else. Matching
#     there is matching the thing being enforced against, not a paraphrase
#     of it.
#   * It leaves a deterministic escape for a message that must *discuss* a
#     trailer -- this guard's own commit message does -- namely the
#     indentation git already uses for quoted material. One keystroke, and
#     visible in review.
#   * The alternative considered was git's own trailer block: the last
#     paragraph, which is the only place a trailer is semantically a
#     trailer. Rejected, because the default's `Generated with <tool>` line
#     sits in its own paragraph *above* the trailer block, so that rule
#     would cover half the default and read as covering all of it.
#
#   A leading space defeats this check. That is not a gap: this guards
#   against a tool's default, not against a person who has decided to lie
#   about authorship, and no message-shaped check reaches the second.
#
# Usage:
#   check-commit-trailers.sh                # baseline..HEAD  (the CI mode)
#   check-commit-trailers.sh <git-range>    # e.g. HEAD~5..HEAD, or a sha
#   check-commit-trailers.sh --message FILE # one message file (commit-msg)
#
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

BASELINE_FILE="scripts/commit-trailer-baseline.txt"

# The e-mails this project commits under -- one `ours <e-mail>` line each in
# the baseline file, because that is per-repository configuration and this
# script is not. Everything else is somebody else's message. `--message`
# mode reads the pending author out of `git var GIT_AUTHOR_IDENT` and asks
# the same question.
#
# It is a set and not one address because it has to be: `lp@lew-palm.de` is
# the one Lew, the reviewer and the coder all use at a terminal, but Codeberg
# stamps a web-UI edit with the forge's noreply address instead, and three
# commits in this history carry it. Those are ours. A discriminator that
# missed them would hand the foreign exemption to the project lead's own
# commits, quietly, which is the failure this whole file is about.
#
# Every line here is an identity that stops being checked. Who is inside the
# project is a declaration and not a fact this script can derive, so the
# control on it is not a computation -- it is that the list lives in the
# reviewed file next to the counts, and adding to it is a diff.
[ -f "$BASELINE_FILE" ] || {
    echo "ERROR: $BASELINE_FILE is missing" >&2
    exit 2
}
OURS=$(awk '$1 == "ours" { printf "%s ", $2 }' "$BASELINE_FILE")
OURS="${OURS% }"
[ -n "$OURS" ] || {
    echo "ERROR: $BASELINE_FILE names no 'ours <e-mail>' line, so this guard" >&2
    echo "       cannot tell our commits from anybody else's. Every commit" >&2
    echo "       would take the foreign exemption. Failing closed." >&2
    exit 2
}

# The vendor list is from the issue. It is matched case-insensitively and is
# not meant to be exhaustive -- a name nobody has heard of yet is caught by
# review, not here.
VENDORS='claude|anthropic|gpt|copilot|gemini'

# Three shapes, all anchored at column 0, all matched against a lowercased
# line:
#   1. the trailer itself, whoever it names;
#   2. `Generated with <tool>` -- required to name a vendor, because
#      "Generated with scripts/generate_test_vectors.py" is a sentence this
#      repository could legitimately write;
#   3. a line opening with the robot emoji, which is the harness's own
#      marker and has no legitimate use at column 0.
PATTERN="^(co-authored-by:.*(${VENDORS})|(🤖[[:space:]]*)?generated with.*(${VENDORS})|🤖)"

# Scans a stream of `\036<sha>\t<author-email>\n<message>` records; prints
# one line per offending message line as
# `<sha>\t<author-email>\t<line-no>\t<text>`. Exits 1 if any.
scan_records() {
    awk -v re="$PATTERN" '
        BEGIN { RS = "\036"; FS = "\n"; bad = 0 }
        NF == 0 || $1 == "" { next }
        {
            split($1, hdr, "\t")
            for (i = 2; i <= NF; i++) {
                if (tolower($i) ~ re) {
                    printf "%s\t%s\t%d\t%s\n", hdr[1], hdr[2], i - 1, $i
                    bad = 1
                }
            }
        }
        END { exit bad }
    '
}

# The record stream for `git log`-selectable revisions. Its own function so
# the authorship canary can check that %ae is still arriving in the field
# the ours/foreign split reads.
records() {
    git log --format='%x1e%H%x09%ae%n%B' "$@"
}

# Prints the offending lines for `git log`-selectable revisions.
scan_revs() {
    records "$@" | scan_records
}

# Is this e-mail one of ours?
is_ours() {
    case " $OURS " in *" $1 "*) return 0 ;; esac
    return 1
}

# Splits scan output by the author of the commit each hit belongs to.
# `select_author ours` keeps the violations; `select_author foreign` keeps
# the messages we have decided not to touch. An empty author field counts as
# foreign, and the plumbing canary below is what stops that reading green.
select_author() {
    awk -F'\t' -v want="$1" -v ours=" $OURS " '
        { mine = ($2 != "" && index(ours, " " $2 " ") > 0) ? "ours" : "foreign" }
        mine == want
    '
}

# Scan output on stdin -> one line per distinct offending commit.
offending_commits() {
    cut -f1 | sed '/^$/d' | sort -u
}

# Counts the distinct offending commits in a (possibly empty) scan result.
count_commits() {
    printf '%s\n' "$1" | offending_commits | wc -l | tr -d '[:space:]'
}

# --- standing canaries ----------------------------------------------------
#
# The concept page requires a permanent pair, not a one-time demonstration:
# a gate that stops matching -- a pattern that no longer compiles the way it
# used to, an awk that returns nothing -- is green forever, which is the
# defect the page exists to remove. These run before the guard reports
# anything else, on every invocation, and cost nothing measurable.
#
# leviculum's pinned `legacy` count is a second and stronger canary: 16 of
# the 1275 commits below its baseline must match and 1259 must not,
# recomputed every run, so a pattern that stopped matching or started
# matching everything goes red there without this. periculum's count is 0
# and covers only one direction, which is why the pair is here rather than
# only in the baseline file.
canary() {
    local bad good
    bad=$(printf 'subject\n\nbody\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>\n')
    good=$(printf 'subject\n\nthe commit carried\n  Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>\nand I amended it.\n\nCloses: #205\n')
    if printf '\036canary\t%s\n%s\n' "${OURS%% *}" "$bad" | scan_records >/dev/null; then
        echo "CANARY: a machine-authorship trailer was NOT matched. This guard" >&2
        echo "        cannot see the defect it exists to catch, and every green" >&2
        echo "        run since it broke means nothing." >&2
        exit 3
    fi
    if ! printf '\036canary\t%s\n%s\n' "${OURS%% *}" "$good" | scan_records >/dev/null; then
        echo "CANARY: a message that only *quotes* a trailer, indented, was" >&2
        echo "        matched. A guard with false positives gets switched off." >&2
        exit 3
    fi
}
canary

# The author arm needs its own pair, for the same reason. "No violation
# found" is what a guard that has stopped noticing foreign trailers reports
# forever, and it is also what a guard that has started calling *everything*
# foreign reports forever. Both directions, every run.
#
# Two things break independently, so both are checked:
#   1. the plumbing -- if `records` ever stops emitting %ae where the split
#      reads it (a --format that dropped it, a header that gained a field),
#      every commit classifies as foreign and our own violations go green
#      from then on. Checked against git itself, on HEAD, so it cannot pass
#      by agreeing with a stale copy of the format string;
#   2. the classification -- checked on a synthetic set carrying a hit from
#      every side, because on a tree whose history happens to hold no
#      foreign hit an inverted comparison would also read green. Every
#      declared identity is in that set, so one that the matcher stops
#      recognising -- the forge's web-UI address is the easy one to lose to
#      a tightened comparison -- goes red here. What this cannot check is an
#      identity deleted from the baseline file, because the file is the only
#      statement of who we are; that one is caught by reading the diff.
authorship_canary() {
    local head_sha head_ae seen synth got_ours got_foreign want_ours ae
    head_sha=$(git rev-parse HEAD)
    head_ae=$(git log -1 --format='%ae' HEAD)
    seen=$(records -1 HEAD | awk 'BEGIN { RS = "\036"; FS = "\n" }
                                  $1 != "" { print $1; exit }')
    if [ "$seen" != "$(printf '%s\t%s' "$head_sha" "$head_ae")" ]; then
        echo "CANARY: the record header for HEAD is '$seen', not" >&2
        echo "        '$head_sha<TAB>$head_ae'. The author e-mail is not" >&2
        echo "        arriving where the ours/foreign split reads it, so every" >&2
        echo "        commit now counts as somebody else's and this guard has" >&2
        echo "        stopped checking our own messages." >&2
        exit 3
    fi

    # One synthetic hit per declared identity, plus one from outside.
    synth=''
    want_ours=''
    for ae in $OURS; do
        synth="${synth}$(printf 'ours:%s\t%s\t5\t%s\n' \
            "$ae" "$ae" 'Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>')
"
        want_ours="${want_ours}ours:${ae} "
    done
    synth="${synth}$(printf 'theirs\tsomebody@example.org\t5\t%s\n' \
        'Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>')"

    got_ours=$(printf '%s\n' "$synth" | select_author ours | cut -f1 | tr '\n' ' ')
    got_foreign=$(printf '%s\n' "$synth" | select_author foreign | cut -f1 | tr '\n' ' ')
    if [ "$got_ours" != "$want_ours" ] || [ "$got_foreign" != "theirs " ]; then
        echo "CANARY: the ours/foreign split called '$got_ours' ours and" >&2
        echo "        '$got_foreign' foreign; it should have been '$want_ours'" >&2
        echo "        and 'theirs '. Either one of our own identities is being" >&2
        echo "        handed the foreign exemption, or somebody else's message" >&2
        echo "        is being refused." >&2
        exit 3
    fi
}
authorship_canary

explain() {
    cat >&2 <<'EOF'

CONTRIBUTING.md: no AI trailers. Commit under your real name and a
reachable e-mail address.

To fix the most recent commit:

    git commit --amend        # delete the offending line, save

For older commits in the range:

    git rebase -i <baseline>  # `reword` each one the report names

If the line above is prose that *quotes* a trailer rather than being one,
indent it by one space. This guard only matches at column 0, because that
is the only place a trailer is a trailer.
EOF
}

# Reports the foreign hits, which are never a rejection.
note_foreign() {
    local hits="$1" n
    n=$(count_commits "$hits")
    [ "$n" -gt 0 ] || return 0
    echo "[check-commit-trailers] $n foreign commit(s) carry a machine-authorship"
    echo "    line. Left verbatim on purpose -- we do not edit a message somebody"
    echo "    outside the project wrote. Counted, not ignored:"
    printf '%s\n' "$hits" | while IFS=$'\t' read -r sha ae no text; do
        [ -n "$sha" ] || continue
        printf '    %s  <%s>  line %s: %s\n' "${sha:0:12}" "$ae" "$no" "$text"
    done
}

# Runs a scan and tolerates its exit 1, which only means "found some".
# Anything above that is awk or git failing, and an undetermined range is
# not a green range.
scan_or_die() {
    local out rc=0
    out=$(scan_revs "$@") || rc=$?
    [ "$rc" -le 1 ] || {
        echo "ERROR: scanning '$*' failed (exit $rc)" >&2
        exit 2
    }
    printf '%s' "$out"
}

# --- one message file (the commit-msg hook) ------------------------------

if [ "${1:-}" = "--message" ]; then
    [ -n "${2:-}" ] || { echo "ERROR: --message needs a file" >&2; exit 2; }
    # The pending commit's author -- what git is about to write, and what
    # the range check will read back afterwards. Keying on it here is what
    # makes the hook and the range check one rule instead of two: a
    # contributor who installs our hooks is not told to rewrite their own
    # attribution.
    author_email=$(git var GIT_AUTHOR_IDENT | sed -n 's/.*<\(.*\)>.*/\1/p')
    # Cut git's `--verbose` diff at the scissors line before scanning, so a
    # diff hunk of this very script cannot trip the guard. Comment lines
    # cannot match anyway -- they start with `#`, not with a trailer.
    hits=$(
        sed '/^#\{0,1\} \{0,1\}-\{8,\} >8 -\{8,\}/,$d' "$2" \
            | { printf '\036(message)\t%s\n' "$author_email"; cat; } \
            | scan_records
    ) || {
        if ! is_ours "$author_email"; then
            echo "[check-commit-trailers] machine-authorship line in a commit by" >&2
            echo "    <$author_email>, which is not one of ours. Left alone: we do" >&2
            echo "    not edit or refuse a message somebody outside the project wrote." >&2
            echo "    The pushed-range check counts it against the pinned foreign" >&2
            echo "    total in $BASELINE_FILE." >&2
            exit 0
        fi
        echo "[check-commit-trailers] REJECTED: machine-authorship line in the commit message" >&2
        echo "$hits" | while IFS=$'\t' read -r _ _ no text; do
            printf '    line %s: %s\n' "$no" "$text" >&2
        done
        explain
        exit 1
    }
    exit 0
fi

# --- a range, or the pinned baseline -------------------------------------

if [ $# -gt 0 ]; then
    RANGE_DESC="$1"
    all=$(scan_or_die "$1")
    ours=$(printf '%s\n' "$all" | select_author ours)
    foreign=$(printf '%s\n' "$all" | select_author foreign)
    note_foreign "$foreign"
    if [ -z "$ours" ]; then
        echo "[check-commit-trailers] OK: no machine-authorship lines of ours in $RANGE_DESC"
        exit 0
    fi
    echo "[check-commit-trailers] REJECTED: machine-authorship line(s) in $RANGE_DESC" >&2
    printf '%s\n' "$ours" | while IFS=$'\t' read -r sha _ no text; do
        printf '    %s  line %s: %s\n' "${sha:0:12}" "$no" "$text" >&2
    done
    explain
    exit 1
fi

# No argument: the CI mode. Three parts, and the third is the newest.
#
# `baseline..HEAD` is the guard, on our own commits. `legacy` is what stops
# the baseline being used as an off switch. Some commits below the baseline
# may carry the trailer -- they predate the rule having anything behind it,
# and rewriting published history to remove them is the larger harm -- so
# the baseline file pins how many, and this recomputes it every run.
# Bumping the baseline forward to silence a fresh failure moves a violation
# across that line in one direction or the other, and either way the count
# changes and a reviewer sees it. Same objection the concept page makes to
# expiry dates: a number a one-line diff can bump is not a bound.
#
# `foreign` is that argument applied to the author exemption. Commits above
# the baseline that carry a trailer and are not ours are admitted -- and
# counted, for exactly the reason `legacy` is counted. Left uncounted, the
# exemption is an off switch anybody can reach by setting an author e-mail.

BASELINE=$(awk '$1 == "baseline" { print $2 }' "$BASELINE_FILE")
LEGACY=$(awk '$1 == "legacy" { print $2 }' "$BASELINE_FILE")
FOREIGN=$(awk '$1 == "foreign" { print $2 }' "$BASELINE_FILE")
[ -n "$BASELINE" ] && [ -n "$LEGACY" ] && [ -n "$FOREIGN" ] || {
    echo "ERROR: $BASELINE_FILE must set 'baseline', 'legacy' and 'foreign'" >&2
    exit 2
}

# A shallow CI clone has to be deepened before any of these mean anything.
# Depth 1 contains the baseline when the baseline *is* HEAD, so testing for
# the object alone is not enough -- the legacy count would come out 0 and
# the range would come out empty, and both would look like an answer.
# Deepen on the shallow flag, and refuse to proceed if it did not take: a
# push of five commits with the trailer in the third must not read green.
if [ "$(git rev-parse --is-shallow-repository)" = "true" ]; then
    git fetch --unshallow >/dev/null 2>&1 \
        || git fetch --deepen=100000 >/dev/null 2>&1 \
        || true
fi
if [ "$(git rev-parse --is-shallow-repository)" = "true" ] \
    || ! git cat-file -e "${BASELINE}^{commit}" 2>/dev/null; then
    echo "ERROR: this clone is shallow, or does not contain the baseline commit" >&2
    echo "       $BASELINE, so the checked range cannot be" >&2
    echo "       established and 'git fetch --unshallow' did not fix it." >&2
    echo "       Deepen the clone (woodpecker: clone settings 'partial: false')." >&2
    echo "       Failing closed: an undetermined range is not a green range." >&2
    exit 2
fi

# The legacy count stays author-blind. It pins a fact about history -- how
# many commits down there carry the line at all -- and history is what it
# is; splitting it by author would let a rewrite swap one class for the
# other without moving either number.
legacy_now=$(count_commits "$(scan_or_die "$BASELINE")")
if [ "$legacy_now" -ne "$LEGACY" ]; then
    echo "ERROR: $LEGACY commits at or below the baseline carry a machine-authorship" >&2
    echo "       line; $legacy_now do now. The baseline moved, or history was" >&2
    echo "       rewritten. Both are things a reviewer has to see." >&2
    echo "       Baseline: $BASELINE ($BASELINE_FILE)" >&2
    exit 1
fi

all=$(scan_or_die "${BASELINE}..HEAD")
ours=$(printf '%s\n' "$all" | select_author ours)
foreign=$(printf '%s\n' "$all" | select_author foreign)

foreign_now=$(count_commits "$foreign")
note_foreign "$foreign"
if [ "$foreign_now" -ne "$FOREIGN" ]; then
    echo "ERROR: $FOREIGN commit(s) above the baseline carry a machine-authorship" >&2
    echo "       line under an author that is not one of ours; $foreign_now do" >&2
    echo "       now. Ours are: $OURS." >&2
    echo "       Nothing is rejected here -- the count is the point. Read the" >&2
    echo "       commits listed above, satisfy yourself that they are somebody" >&2
    echo "       else's work and not one of ours committed under another" >&2
    echo "       e-mail, and move the number in $BASELINE_FILE in the same" >&2
    echo "       commit that brings them in." >&2
    exit 1
fi

if [ -z "$ours" ]; then
    n=$(git rev-list --count "${BASELINE}..HEAD")
    echo "[check-commit-trailers] OK: $n commit(s) since ${BASELINE:0:12}, none of ours carrying a machine-authorship line ($LEGACY known below the baseline, $FOREIGN foreign above it)"
    exit 0
fi
echo "[check-commit-trailers] REJECTED: machine-authorship line(s) since ${BASELINE:0:12}" >&2
printf '%s\n' "$ours" | while IFS=$'\t' read -r sha _ no text; do
    printf '    %s  line %s: %s\n' "${sha:0:12}" "$no" "$text" >&2
done
explain
exit 1
