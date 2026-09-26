#!/usr/bin/env bash
# Publishes dist/*.deb + *.sha256 to the rolling `nightly` Codeberg
# release. Called from .woodpecker/nightly.yml. Stable download URL:
#   https://codeberg.org/${CI_REPO}/releases/download/nightly/<filename>
#
# The release is rolling: same tag every night, assets overwritten.
# Version info for each build is embedded in the binaries themselves
# (lnsd --version) and in the release body.
#
# Authentication uses a Codeberg API token with `write:repository`
# scope, exposed to the publish step via the Woodpecker secret
# `codeberg_token`. CI_NETRC_PASSWORD (Woodpecker's OAuth-derived
# token) is NOT visible outside the clone step, so a manual token is
# required.
#
# Required env (set by Woodpecker):
#   CI_REPO         — e.g. "Lew_Palm/leviculum"
#   CI_COMMIT_SHA   — current commit
#   CODEBERG_TOKEN  — Codeberg API token (Woodpecker secret)
#   LEVICULUM_BUILD_ID (optional, for release body)

set -euo pipefail

: "${CI_REPO:?CI_REPO not set}"
: "${CI_COMMIT_SHA:?CI_COMMIT_SHA not set}"
: "${CODEBERG_TOKEN:?CODEBERG_TOKEN not set}"
# The tag push at the end reads the token from the environment via a git
# credential helper; Woodpecker exports it, a manual caller might not.
export CODEBERG_TOKEN

TAG="nightly"
API="https://codeberg.org/api/v1"
AUTH_HEADER="Authorization: token ${CODEBERG_TOKEN}"
BUILD_ID="${LEVICULUM_BUILD_ID:-unknown}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist"
[ -d "$DIST" ] || { echo "dist/ not found — run collect-nightly-debs.sh first"; exit 1; }

RELEASE_BODY=$(cat <<EOF
Rolling nightly build. The assets under this release are **replaced on every CI run** — this tag always points at the latest nightly.

**Debian / Ubuntu packages** (statically linked musl, runs on Debian 9+ / Ubuntu 16.04+, no extra packages needed):

\`\`\`
https://codeberg.org/${CI_REPO}/releases/download/nightly/leviculum-nightly-amd64.deb
https://codeberg.org/${CI_REPO}/releases/download/nightly/leviculum-nightly-arm64.deb
\`\`\`

\`sudo apt install ./leviculum-nightly-amd64.deb\` installs \`lnsd\` as a systemd service and sets up \`/etc/reticulum\` for Python-RNS client drop-in compatibility.

**Userspace tarball** (just the binaries plus README/LICENSE/CHANGELOG, no service, no root needed):

\`\`\`
https://codeberg.org/${CI_REPO}/releases/download/nightly/leviculum-nightly-amd64.tar.gz
https://codeberg.org/${CI_REPO}/releases/download/nightly/leviculum-nightly-arm64.tar.gz
\`\`\`

\`tar xzf leviculum-nightly-amd64.tar.gz && ./leviculum-nightly-amd64/bin/lnsd --version\` runs without installing anything system-wide.

**lnomad — Nomadnet terminal browser** (separate package, does not install or start the lnsd service):

\`\`\`
https://codeberg.org/${CI_REPO}/releases/download/nightly/lnomad-nightly-amd64.deb
https://codeberg.org/${CI_REPO}/releases/download/nightly/lnomad-nightly-arm64.deb
https://codeberg.org/${CI_REPO}/releases/download/nightly/lnomad-nightly-amd64.tar.gz
https://codeberg.org/${CI_REPO}/releases/download/nightly/lnomad-nightly-arm64.tar.gz
\`\`\`

\`sudo apt install ./lnomad-nightly-amd64.deb\` — the browser needs a running RNS instance (leviculum's \`lnsd\` or Python \`rnsd\`).

**lblogd — dev-blog server** (separate package; serves Markdown posts as a NomadNet page node and on the clearnet):

\`\`\`
https://codeberg.org/${CI_REPO}/releases/download/nightly/lblogd-nightly-amd64.deb
https://codeberg.org/${CI_REPO}/releases/download/nightly/lblogd-nightly-arm64.deb
https://codeberg.org/${CI_REPO}/releases/download/nightly/lblogd-nightly-amd64.tar.gz
https://codeberg.org/${CI_REPO}/releases/download/nightly/lblogd-nightly-arm64.tar.gz
\`\`\`

\`sudo apt install ./lblogd-nightly-amd64.deb\` installs and starts \`lblogd\` as a systemd service. As shipped it serves on \`http://127.0.0.1:8180/\`; \`/etc/lblogd/config.toml\` explains how to put it on a public domain with automatic HTTPS.

\`lnomad\` and \`lblogd\` carry their own version numbers, independent of the \`leviculum\` packages above.

**lnflash — firmware flasher for LNode boards** (self-contained bundle: the flasher, the T114 firmware image, and Nordic's S140 SoftDevice with its licence):

\`\`\`
https://codeberg.org/${CI_REPO}/releases/download/nightly/lnflash-nightly-amd64.tar.gz
\`\`\`

\`tar xzf lnflash-nightly-amd64.tar.gz && cd lnflash-* && sudo ./lnflash\` — nothing is downloaded and nothing is installed; everything it writes to the board is in the directory. The flasher binary is amd64; the firmware image and SoftDevice inside are not architecture-specific.

**Source tarball** (tracked files at the same commit as the .debs above, no submodules):

\`\`\`
https://codeberg.org/${CI_REPO}/releases/download/nightly/leviculum-nightly-source.tar.gz
\`\`\`

Each asset is published with a matching \`.sha256\` next to it.

Current build: \`${BUILD_ID}\` (commit \`${CI_COMMIT_SHA}\`)

Verify with \`lnsd --version\` after install.
EOF
)

# Find existing release
echo "[publish] looking up release tag=${TAG}"
release_json=$(curl -sS -H "$AUTH_HEADER" "$API/repos/$CI_REPO/releases/tags/$TAG" || echo '{}')
release_id=$(echo "$release_json" | jq -r '.id // empty')

if [ -z "$release_id" ]; then
    echo "[publish] no existing release, creating"
    # target_commitish must be a branch name when the tag doesn't
    # yet exist — Forgejo rejected both the bare SHA (build #40) and
    # an omitted field (build #41) with "The target couldn't be
    # found." Use the default branch from Woodpecker, which is
    # 'master' here. The exact build SHA still appears in the body.
    BRANCH="${CI_REPO_DEFAULT_BRANCH:-master}"
    release_json=$(jq -n \
        --arg tag "$TAG" \
        --arg target "$BRANCH" \
        --arg body "$RELEASE_BODY" \
        '{tag_name:$tag, target_commitish:$target, name:"Nightly Builds", body:$body, draft:false, prerelease:true}' \
        | curl -sS -X POST -H "$AUTH_HEADER" -H "Content-Type: application/json" \
            "$API/repos/$CI_REPO/releases" -d @-)
    release_id=$(echo "$release_json" | jq -r '.id')
    if [ -z "$release_id" ] || [ "$release_id" = "null" ]; then
        echo "[publish] create failed: $release_json"; exit 1
    fi
    echo "[publish] created release id=${release_id}"
else
    echo "[publish] found release id=${release_id}, refreshing body"
    jq -n \
        --arg body "$RELEASE_BODY" \
        '{body:$body}' \
        | curl -sS -X PATCH -H "$AUTH_HEADER" -H "Content-Type: application/json" \
            "$API/repos/$CI_REPO/releases/$release_id" -d @- >/dev/null

    echo "[publish] deleting existing assets"
    # Forgejo's asset-delete endpoint is
    # /repos/{owner}/{repo}/releases/{release_id}/assets/{attachment_id}.
    # The release_id segment is mandatory — omitting it yields a silent
    # 404 with -sS, which is exactly what happened before this fix and
    # caused assets to accumulate across runs (12 stale entries on the
    # nightly tag pointing at three different builds).
    echo "$release_json" | jq -r '.assets[].id' | while read -r asset_id; do
        [ -n "$asset_id" ] || continue
        http_code=$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE \
            -H "$AUTH_HEADER" \
            "$API/repos/$CI_REPO/releases/$release_id/assets/$asset_id")
        echo "[publish]   delete asset $asset_id → HTTP $http_code"
    done
fi

echo "[publish] uploading new assets"
shopt -s nullglob
for f in "$DIST"/*.deb "$DIST"/*.tar.gz "$DIST"/*.sha256; do
    name=$(basename "$f")
    echo "[publish]   → $name"
    curl -sS -X POST -H "$AUTH_HEADER" \
        -F "attachment=@${f}" \
        "$API/repos/$CI_REPO/releases/$release_id/assets?name=${name}" >/dev/null
done

# The release rolls, so the git tag must roll with it. Forgejo points the
# tag at a commit only when the release is CREATED (the branch above);
# refreshing an existing release never moves the ref. That left `nightly`
# frozen at its creation commit (05a17675, 2026-05-06) while the assets
# beside it moved on nightly — the release page's "Source code" links
# served months-old source next to current binaries. Force-push the tag to
# the commit this run actually built, after the assets are up so a failed
# upload never moves it. The commit is already on the remote (CI builds
# pushed commits), so this transfers no objects, only the ref.
#
# The push authenticates through a one-shot credential helper, never a
# token-in-URL remote: when a push fails, git prints the full remote URL
# into the error message, and this log is public. The helper string is
# single-quoted on purpose — git expands the variable when it invokes
# the helper, reading the environment (exported above), so the token
# appears in no URL and in no process argument. The empty helper first
# clears any inherited helpers so ours is the only one consulted.
# Everything else in this script authenticates via header for the same
# no-token-in-URL reason.
echo "[publish] pointing tag ${TAG} at ${CI_COMMIT_SHA}"
# shellcheck disable=SC2016
git -C "$ROOT" \
    -c credential.helper= \
    -c credential.helper='!f() { echo "username=oauth2"; echo "password=${CODEBERG_TOKEN}"; }; f' \
    push "https://codeberg.org/${CI_REPO}.git" \
    "+${CI_COMMIT_SHA}:refs/tags/${TAG}"

echo "[publish] done"
echo "[publish] latest: https://codeberg.org/${CI_REPO}/releases/tag/${TAG}"
