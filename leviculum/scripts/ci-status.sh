#!/bin/bash
RESULTS=~/.local/state/leviculum-ci/last-results.txt

if [ ! -f "$RESULTS" ]; then
    echo "No CI runs yet."
    exit 0
fi

for tier in tier0 tier1 tier2 tier3; do
    # `[ +]`, not a plain space: the pre-push hook writes its tier-0 line as
    # `tier0+mvr GREEN`, so a `" tier0 "` grep matched none of them and this
    # table read "(no runs yet)" for the one tier that runs on every push.
    LAST=$(grep -E " $tier[ +]" "$RESULTS" | tail -1)
    if [ -n "$LAST" ]; then
        printf "  %-6s  %s\n" "$tier" "$LAST"
    else
        printf "  %-6s  (no runs yet)\n" "$tier"
    fi
done

# Tier 2 is on demand (install-ci.sh step 9, Lew 2026-06-12): no timer
# starts it, so its row above can be arbitrarily old and still be correct.
# Say how old it is, in days, and leave the judgement to the reader.
#
# Until 2026-08-07 this printed a verdict word from check-tier2-staleness.sh
# instead — OK/WARN/STALE against a 24h threshold. That word read the same
# at 25 hours as at the 46 days it had actually been, and the pre-push gate
# that consumed it blocked every push for those 46 days with a remedy that
# could not clear it. Both the gate and the script are gone; the fact they
# were derived from is printed here directly.
echo ""
LAST_T2=$(grep -E " tier2[ +]" "$RESULTS" | tail -1 | awk '{print $1}')
echo "Tier 2 runs on demand: systemctl --user start leviculum-ci-tier2.service"
if [ -z "$LAST_T2" ]; then
    echo "  No tier-2 run has ever been recorded."
else
    DAYS=$(( ($(date +%s) - $(date -d "$LAST_T2" +%s)) / 86400 ))
    echo "  No tier-2 run has been recorded since $LAST_T2 ($DAYS days ago)."
fi
