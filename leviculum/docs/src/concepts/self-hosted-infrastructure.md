# Self-hosted infrastructure

A plan to give Leviculum and Periculum a second home on our own server,
`workhorse.de` (public name `leviculum.network`), reachable over both the
clearweb and the Reticulum network, running **permanently in parallel**
with Codeberg rather than replacing it.

This is a design record for later implementation, not a description of
something already built. Nothing here is provisioned yet.

## Motivation

Today the project is single-homed: code, release artifacts and the issue
tracker all live with one hosting provider. Any single provider is a
single point of failure — an outage, an account dispute, or a change in a
service's terms can remove all three at once. The response is to stop
being single-homed: stand up an independent home we fully control, keep
both in sync indefinitely, and treat the loss of either as a non-event
because the other is already live and complete. As a bonus, a
self-hosted node is reachable over Reticulum, which clearweb forges are
not — a natural fit for a Reticulum stack.

## Principles

1. **Simplest thing that works.** No CI framework, no forge platform. A
   bare git repo, a static web server, a shell script, and `rngit`.
2. **Both networks, one source.** Everything a user needs — code,
   issues, releases — is reachable over clearweb *and* Reticulum.
   Wherever possible this is achieved by the data being *in the git
   repo*, which is clonable over both, not by running a service twice.
3. **No single point of failure at the code.** The authoritative copy is
   an ordinary bare git repo. `rngit` — young, "not tested extensively
   in the wild" per the RNS manual — sits *beside* it as a Reticulum
   remote, never underneath it.
4. **AI-friendly by construction.** An agent with no prior knowledge
   reads one `AGENTS.md` and knows how to clone, file an issue, and cut a
   release, because the patterns are plain files and standard git, not a
   bespoke API.
5. **Reticulum-compatibility is Priority 1.** The Reticulum side uses
   Mark's own `rngit`, so we interoperate with the ecosystem by default.

## Architecture

```
                 ┌──────────────────────────────────────────┐
                 │  workhorse.de  (leviculum.network)         │
                 │                                            │
   clearweb ─────┤  nginx  ──► /srv/git/leviculum.git (bare)  │  ◄─── SSH push
   git clone     │         └─► /srv/www (static: issues,      │       (maintainers)
   downloads     │              release artifacts, git browse)│
                 │                                            │
   Reticulum ────┤  rngit  ──► same bare repo as rns:// remote │
   rns:// clone  │         ├─► rngit release  (signed nightly) │
   NomadNet page │         └─► serve_nomadnet = yes (browse)   │
   lncp/rncp     │                                            │
                 │  nightly.sh  (systemd timer): build+test,   │
                 │     on green → publish to both fronts       │
                 │  sync.sh     (timer): mirror to/from Codeberg│
                 └──────────────────────────────────────────┘

   The bare repo is the single source of truth for code. Issues live
   inside it as plain-text files, so they travel with every clone on
   either network. Codeberg is kept as a synchronised parallel home.
```

## Component 1 — Git upstream (dual-network, permanently mirrored)

The authoritative copy is `/srv/git/leviculum.git`, a bare repo.

- **Maintainer push:** over SSH (`git push origin master`). SSH key
  auth, no web push. The only write path to the code.
- **Clearweb read/clone:** nginx serving the bare repo, smart HTTP via
  `git-http-backend` (a CGI shipped with git — no extra software), or
  dumb-HTTP (`git update-server-info` in a post-receive hook + static
  serving) for zero CGI. Anonymous clone/pull only.
- **Reticulum read/clone:** `rngit` with `[repositories] public = /srv/git`
  exposes the same repo at `rns://<hash>/public/leviculum`.
- **Codeberg stays a mirror:** every maintainer pushes to both remotes
  (`git remote set-url --add --push origin`), so Codeberg and workhorse
  hold identical history. Because git commits are content-addressed, the
  two are byte-identical when in sync; there is no divergence to
  reconcile for code.

Periculum is a second repo in the same group, treated identically.

## Component 2 — Issues as plain text in the repo

Issues are Markdown files committed into the repo. They are part of every
clone, on both networks, with no running issue service and no API.

```
issues/
  open/    0042-lxmf-msgpack-unbounded-recursion.md
  closed/  0038-lrproof-hop-asymmetry.md
```

Each file is front-matter plus a Markdown body:

```markdown
---
id: 42
title: LXMF msgpack skip recurses without a depth budget
labels: [bug, compat, priority:low]
state: open
created: 2026-08-17
author: <name or Reticulum identity hash>
---
Body in Markdown, cite code as path:line. Comments are appended below a
`---` rule, each with an author + date line.
```

- **State** is the directory (`open/` vs `closed/`); `ls issues/open` is
  the backlog, a state change is one `git mv`, and it diffs cleanly.
- **Search** is `grep -r`. No index, no database.
- **Numbering:** `scripts/new-issue.sh "title"` picks the next free
  number by scanning `issues/`. Collisions are rare (nearly all issues
  originate from our own instances) and resolve at merge; switch to short
  hash IDs only if distributed creation ever makes that a real problem.
- **Attribution**, if wanted, comes from signed git commits, not a
  per-issue crypto layer.

`rngit work` was considered and rejected for the issue store: it keeps
work items as msgpack in a side directory, writable only with `rngit`
installed and not legible at rest. Plain files win on both-network reach,
AI-transparency, and zero-service — at the cost of built-in signing,
which git commit signing replaces.

## Component 3 — Nightly builds (a script, not a framework)

`nightly.sh`, run by a systemd timer:

```
1. fetch + checkout the tip of master into a clean build worktree
2. cargo build --release  (both stacks)
3. run the tests. If red -> stop, notify, publish NOTHING.
4. on green only:
   - stamp nightly-YYYYMMDD-<shortsha>
   - copy artifacts to /srv/www/releases/nightly/  (clearweb download)
   - repoint /srv/www/releases/nightly/latest  (symlink)
   - keep the previous N builds = last-known-good (add-then-swap, never
     destroy-then-upload)
   - rngit release <repo> create nightly-YYYYMMDD-<sha>:./dist  (signed)
5. regenerate the static clearweb views (Component 5)
```

This designs out three review findings at once: `PUB-0016` (nightly
shipped from an untested/red tree — green is now a precondition),
`PUB-0005` (destroy-before-upload with no failure detection — add-then-
swap keeps history), and `PUB-0011` (no rollback — retained builds plus
the `latest` symlink are exactly that).

## Component 4 — Downloads over both networks

- **Clearweb:** nginx serves `/srv/www/releases/` at stable URLs such as
  `https://leviculum.network/releases/nightly/latest/leviculum-amd64.deb`.
- **NomadNet page:** `rngit serve_nomadnet = yes` already exposes a
  release list, file browser, commit history and refs to any NomadNet
  client; its Micron templates live in `~/.rngit/templates/`.
- **Reticulum file pull:** `rngit release <repo> fetch` is primary (it
  verifies the Ed25519 manifest signature before writing any bytes);
  `lncp`/`rncp --fetch` remain available for raw file pulls.

## Component 5 — Clearweb read-only views

Two static views, regenerated by the nightly run and by a post-receive
hook, so nothing dynamic runs on the clearweb side:

- **Code browsing:** `stagit` renders static HTML into `/srv/www/code/`.
  No CGI, no daemon. (`cgit` only if dynamic browsing is later wanted.)
- **Issue browsing:** a small script renders `issues/**/*.md` to a static
  `/srv/www/issues/` index and per-issue pages. Read-only on clearweb;
  writing an issue is a git commit over either network.

## Component 6 — Keeping Codeberg and workhorse in sync

Code sync is trivial and symmetric: maintainers push the same commits to
both remotes, and content-addressing guarantees they match.

Issue sync is the one genuinely hard part, because Codeberg keeps issues
in its own database (web UI, external contributors) while our issues are
plain files in the repo. Rather than a true bidirectional merge (which
invites conflicts), pick one side as the source of truth and mirror the
other one-way; this decision is deferred but the two shapes are:

- **Repo as source of truth** (matches the long-term single-home design):
  plain-text issues are authoritative, a `sync.sh` job pushes
  creates/edits/closes to Codeberg via the Gitea API for visibility.
  External Codeberg-only comments are pulled back on a best-effort basis
  and appended to the file. Simplest to reason about; Codeberg becomes a
  read-mostly shopfront.
- **Codeberg as source of truth** (matches keeping Codeberg the primary
  day-to-day tracker): humans file and discuss on Codeberg's web UI, and
  `sync.sh` exports the issue set to plain files in the repo so Reticulum
  users can read (not write) them. Loses write-from-Reticulum until the
  repo is ever made authoritative.

The choice is a later detail decision. Whichever side is chosen, the sync
is a one-way export/import script on a timer, not a live service, and the
Gitea API access already used for issue curation is sufficient.

## AI-friendliness — the `AGENTS.md` contract

A single file at the repo root tells any agent everything in one screen:
how to clone over each network, that issues are Markdown under
`issues/open` and `issues/closed`, how to file one (`scripts/new-issue.sh`,
edit, commit, push), how to close one (`git mv` to `closed/` with a
note), and that releases are cut only by `scripts/nightly.sh`. Because
the substrate is plain files and standard git, an agent needs no
project-specific API knowledge — the patterns are the ones every agent
already knows.

## Rollout

1. **Stand up (no announcement).** Provision workhorse.de: bare repo,
   nginx, `rngit`, `nightly.sh` + timer, `AGENTS.md`, `stagit`, the issue
   renderer. Maintainer instances add workhorse as a second push remote.
2. **Parallel run, indefinitely.** Both homes live; every push goes to
   both. Import the current Codeberg issues into `issues/` once, then run
   `sync.sh` on a timer. Exercise `rngit` clone/pull/release over real
   Reticulum. This is the steady state, expected to continue indefinitely.
3. **If either home ever becomes unavailable:** the other already holds
   everything, so nothing is lost. If the self-hosted side is to become
   the sole home, point the README solely at `leviculum.network` and
   carry on — no scramble, because both have been live in parallel all
   along.

## What this resolves

- Single-provider dependency for code, releases and issues — the reason
  for a second, self-controlled home.
- Untested nightly builds, destroy-before-upload publishing, and no
  rollback — designed out by `nightly.sh` (Component 3).
- Build docs that point at a path the musl target never produces —
  folded into the new README and `AGENTS.md`.

## Open questions

1. **Server baseline:** current state of workhorse.de (OS, nginx, an
   existing Reticulum instance, public IP, TLS via certbot).
2. **Reticulum transport to the server:** TCP interface over the
   clearweb, or a real RF path — affects `rngit` reachability and
   announce cadence.
3. **Issue sync direction:** which side is source of truth (Component 6),
   and the field mapping for the one-time Codeberg import.
4. **`rngit` acceptance bar:** what track record over Reticulum is
   required before it is relied on even as a secondary path.
5. **Signing:** SSH vs GPG commit signing for attribution; whether
   release signing uses a dedicated identity.
