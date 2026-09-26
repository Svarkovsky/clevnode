#!/bin/bash
# _repo-sync.sh — sync the current worktree to origin/master.
#
# Used at the head of run-tier2.sh and run-tier3-hw.sh on the
# schneckenschreck CI worktree so that scheduled runs always pick
# up the latest master without requiring a manual `git pull`.
#
# Invoked only when the calling tier-runner finds the worktree-
# scoped marker file (see install-ci.sh --vm-mode) inside the
# worktree's git-dir.  Default-mode (developer) installs never
# write the marker, so this helper never runs against the
# developer's interactive checkout.
#
# `--force` is intentional: the worktree is owned by CI; any local
# state is stale and discardable.  `origin/master` (not `master`)
# yields a detached HEAD on each run, which avoids any conflict
# with `master` being checked out in the developer's primary
# worktree (a worktree may not have the same branch checked out
# in two places simultaneously).

set -e

git fetch --quiet origin master
git checkout --quiet --force origin/master
# Submodule update is best-effort: master can refer to a vendor
# commit that is unreachable from the configured submodule remote
# (private fork, force-push GC'd, etc.).  A failure here doesn't
# invalidate the sync — the subsequent build will surface any
# real dependency on the missing submodule state.
git submodule update --init --recursive --quiet \
    || echo "[_repo-sync] WARN: git submodule update failed; continuing"
