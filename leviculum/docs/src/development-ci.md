# CI Pipeline

A self-hosted CI pipeline runs entirely on the developer's machine.
Four tiers with different time budgets and triggers automate the
test discipline mandated by `CLAUDE.md` — no GitHub Actions, no
external runners.

## Tiers

| Tier | Name | Trigger | Budget | Test scope |
|------|------|---------|--------|------------|
| 0 | `fast` | pre-push hook | ~3 min | fmt + clippy (host + nrf firmware workspace, both BSPs) + the firmware stack-frame gate + rustdoc gate + the [third-party notice guard](concepts/licensing-and-notices.md) + workspace lib tests |
| 1 | `standard` | on demand: `just standard`, once per batch | ~15 min (first run: 20-40 min cold compile) | Tier 0 + core/tests + ffi + proxy + rnsd_interop + TCP-hub endurance smoke soak (see [Soak and endurance](soak-and-endurance.md)) + the `status_parity` two-daemon suite + the ignored-test census |
| 2 | `extensive` | on demand: `systemctl --user start leviculum-ci-tier2.service` | ~30-90 min | Tier 1 + the periculum `conformance/` and `regression/` corpora (docker) |
| 3 | `nightly` | systemd timer 02:00 daily | ~2-6h | Tier 2 + LNode flash-from-HEAD + the periculum `hardware/` corpus |

Each tier runs everything from the lower tiers as well, so a green
nightly proves the entire stack.

## Installation

One command, idempotent:

```
just install-ci
```

It installs `git` hooks (via `core.hooksPath = .githooks`), runner
scripts, systemd user units, the separate cargo target dir, and the
state dir. Re-running is safe.

The installer detects the worktree it was run from and patches the
systemd-unit `ExecStart` paths to match — so a `git worktree`-based
second checkout (see "VM-mode install" below) installs its own units
that fire against itself.

### VM-mode install (CI worktree on a long-running host)

For schneckenschreck or any other dedicated CI machine where the
nightly Tier-3 runs land, install with `--vm-mode`:

```
git worktree add ~/coding/libreticulum-ci master
cd ~/coding/libreticulum-ci
bash scripts/install-ci.sh --vm-mode
```

`--vm-mode` differs from the default install in two ways:

1. The git-hook wiring (`core.hooksPath = .githooks`) is **skipped**.
   The VM never commits or pushes; hooks would never fire.
2. A worktree-scoped marker file
   (`.git/worktrees/<name>/leviculum-ci-vm-mode-marker`) is created.
   `run-tier2.sh` and `run-tier3-hw.sh` check this marker at the
   head of every run and, if present, invoke `_repo-sync.sh` to do
   `git fetch + git checkout --force origin/master + git submodule
   update --recursive`.

The marker is per-worktree, not per-user: a manual invocation of
`run-tier2.sh` from the developer's primary checkout will **not**
trigger the destructive `--force` checkout against the wrong tree.

The synced commit hash is appended to `last-results.txt` as
`<timestamp> tier2 sync HEAD=<short-hash>` (or `tier3-hw` for the
nightly), so you can correlate scheduled runs with the master commit
they tested.

## The firmware stack-frame gate

`just nrf-stack-frames` builds both firmware binaries and reads the
frame-allocating `sub sp` immediates out of the linked ELF. Any frame
above 16 KB fails the gate.

The T114 stack is 128 008 B and grows down into the SoftDevice's RAM
floor. An overflow past `_stack_end` does not fault: it overwrites SD
state, and the board dies later in an SD internal assertion with a
useless PC. So a single oversized frame is both fatal and invisible,
which is why this is checked statically on every push rather than
observed at runtime.

The frame it was written for: `Box::new(builder.build(..))` materialised
a by-value `NodeCore` — over 40 KB once `EmbeddedStorage`'s inline
collections are counted — twice in `main`'s poll frame. 94 720 B, 74 % of
the stack, ~13 KB of margin left for the whole call tree.
`NodeCoreBuilder::build_boxed` allocates first and configures through the
box, which drops that frame to 12 672 B.

The gate prefers `arm-none-eabi-objdump` and falls back to the rustup
`llvm-tools` `llvm-objdump`; `install-ci.sh` installs the latter.

## Manual operation

```
just fast        # Tier 0
just standard    # Tier 1
just extensive   # Tier 2
just nightly     # Tier 3
just status      # show recent runs across all tiers
```

## First-run expectation

Tier 1 runs in a separate `CARGO_TARGET_DIR` (`~/.cache/leviculum-ci-
target/`) so it doesn't fight your IDE's `target/` for inkremental
caches. The first run after `install-ci.sh` compiles the whole
workspace and all test binaries from scratch — **plan for 20-40
minutes**. Subsequent runs are incremental, ~5-15 minutes.

## Notifications

**Read this as history, not as behaviour.** `scripts/run-tier3.sh` calls
`notify-send` on its verdict — `-u critical` for RED (sticky until
dismissed), `-u normal` for GREEN and for a lock-held skip. It is the only
tier runner that ever did. It is also not the script the nightly starts:
`leviculum-ci-nightly.service` runs `scripts/run-tier3-hw.sh`, which
writes the ledger and notifies nobody. So no tier notifies today. Results
are pull-only — `just status`, or
`~/.local/state/leviculum-ci/last-results.txt`.

**Prerequisite:** `notify-send` needs `DBUS_SESSION_BUS_ADDRESS` and
`XDG_RUNTIME_DIR` in the user systemd manager environment, which
exists only when you have a logged-in graphical session. On a
headless server, notifications are silently dropped — inspect
`~/.local/state/leviculum-ci/last-results.txt` instead.

## Stale-block on push (removed 2026-08-07)

`pre-push` used to block the push when the last `tier2 GREEN` line in
`last-results.txt` was ≥ 10 commits or ≥ 24 hours old. It was removed,
not repaired. Only `scripts/run-tier2.sh` writes that line, nothing has
started it since the Tier 2 timer was retired on 2026-06-12, and the
remedy the block printed (`just extensive`) does not write it either —
so the block could not be cleared by doing what it said. It was
unsatisfiable for 46 days, and the 502 commits that landed in that
window all used `git push --no-verify`, which switches off the lint,
Tier 0, mvr and the trailer guard along with it.

`scripts/ci-status.sh` still reports how long it has been since a Tier 2
run was recorded. It states the age and blocks nothing.

## Tier 1 after every commit (removed 2026-08-07)

`.githooks/post-commit` detached `scripts/run-tier1.sh` — `just standard`
under docker, 15 minutes warm and 20-40 cold — after every commit that was
not part of a rebase. It was removed, and the rule it failed is in
[Checks That Are Actually Checks](concepts/checks-and-citations.md).

The short form: a commit is not a unit anybody wants tested. WIP commits,
amends and commits mid-refactor all started a forty-minute run, which is
why the runner needed a dirty-flag loop to coalesce them — it was
repairing a granularity that was wrong to begin with. It was also
invisible: batches were separately instructed to start `just standard`
under `nohup`, so the same tier ran twice per batch for a week before
anyone noticed the hook existed. And it ran docker in the background,
which tears down containers whatever else is on the box was using — the
standing rule against starting the full integ suite behind someone's back
exists for that collision, and this hook was doing it after every commit.

Tier 1 is now started explicitly, once per batch, by typing
`just standard`.

## Logs

Location: `~/.local/state/leviculum-ci/`

| File | Contents |
|------|----------|
| `last-results.txt` | one-line tally per run (`<iso-timestamp> <tier> GREEN/RED <log-path>`) |
| `tier1-YYYYMMDD-HHMMSS-PID.log` | full Tier 1 output (one file per run) |
| `tier2-YYYYMMDD-HHMMSS-PID.log` | full Tier 2 output |
| `nightly-YYYYMMDD-HHMMSS-PID.log` | full Tier 3 output |
| `tier1.lock` | flock for Tier 1 concurrency control |
| `tier1.dirty` | marker that Tier 1 needs to (re-)run |

Rotation: tier 1/2 logs are deleted after 14 days; nightly logs after
60 days. Done at the start of each runner script.

Each script run gets its own log file (timestamp + PID suffix). No
run ever overwrites another run's log — this is intentional so a
failure trace cannot vanish under a successful re-run. The path of
the specific log goes into `last-results.txt` so `just status` can
point at exactly the right file.

## The scenario suites live in periculum

The multi-node scenarios that used to be `reticulum-integ` are now the
sibling [periculum](https://codeberg.org/Lew_Palm/periculum) checkout,
which leviculum expects at `../periculum` (override with
`PERICULUM_ROOT`, or the binary with `PERICULUM_BIN`). They are TOML
files, not `#[test]` functions, so the tier separation is a matter of
which directory a tier runs rather than of `#[ignore]`:

| Corpus | Binds hardware | Run by |
|---|---|---|
| `conformance/` | no | Tier 2 |
| `regression/` | no | Tier 2 |
| `hardware/` | yes | Tier 3 |

The split is machine-checked in periculum
(`periculum/tests/corpus_admission.rs`), so a scenario cannot drift into
the wrong tier by convention alone. A `hardware/` scenario whose boards
this bench does not hold reports `SKIPPED_INFRA` naming what was
missing — never RED.

Run one scenario by hand:

```
periculum run ../periculum/hardware/lora_link_rust.toml
```

## Concurrent test protection

Two scenario runs on the same machine fight over Docker container names
and USB serial handles. To prevent that, periculum acquires a
process-wide file lock on `~/.local/state/leviculum-ci/test.lock` before
bringing any node up.

Single invocation: transparent. No extra output.

Two simultaneous invocations: the second exits within a second with
a multi-line `[leviculum]` message naming the current holder —
pid, started time, cwd, optionally the test-name filter. Example:

```
[leviculum] Another integration test is already running.
[leviculum] Current holder:
[leviculum]   pid=12345
[leviculum]   started=2026-04-14T02:01:33
[leviculum]   pkg=periculum
[leviculum]   binary=periculum
[leviculum]   cwd=/path/to/leviculum
[leviculum] Wait for it to finish or stop that process, then retry.
```

On-demand Tier 2 / scheduled Tier 3 runs that collide with a manual test
drop a marker file at `~/.local/state/leviculum-ci/lock-contention`;
the runner scripts observe the marker, classify the run as SKIPPED
(not RED), send a `normal` (not `critical`) notification, and delete
the marker. No false-alarm pages.

### Inspecting the lock

```
cat ~/.local/state/leviculum-ci/test.lock     # current (or last) holder
ls  ~/.local/state/leviculum-ci/lock-contention  # marker if present
```

### Force-release

Not applicable. The kernel releases the flock the moment the holding
process closes its fd — on clean exit, panic, SIGINT, SIGKILL, and
even host reboot. There is no TTL, no heartbeat, no manual cleanup
path. A stale `test.lock` file on disk after a reboot is self-
healing: the next invocation opens it, flock succeeds immediately
(kernel state is empty post-reboot), and the stale content is
overwritten.

### Scope

The lock protects only scenario runs. Unit tests in `leviculum-core`,
`leviculum-std`, `leviculum-ffi`, `leviculum-proxy`, and
`leviculum-cli` do not acquire it — they parallelise freely with an
in-progress scenario run. `periculum validate` and `periculum list`
do not acquire it either: they read scenario files and touch no node,
container or radio.

### Filesystem requirement

Local filesystem only. `flock` semantics over NFS / sshfs are
implementation-defined. If your `$HOME` is on a network filesystem,
the lock behaviour is not guaranteed. This is a single-developer
dev-box tool; not an issue in practice.

## Hardware test profiles (Tier 3)

Tier 3 runs the periculum `hardware/` corpus over USB-attached
embedded devices. Different scenarios need different subsets of the
attached boards; the rest must not transmit, so their RF activity does
not contaminate the run.

**No USB-hub power switching.** Every board stays permanently powered
and passed through to the VM. RF isolation of non-participating
firmware nodes is done in software: the runner pushes `radio_silent`
over serial to every discovered board it did not bind. Per-port power
cycling correlated with hamster hardware-watchdog freezes (proven
2026-06-15) and was removed, together with the `usbhub-helper` and its
libvirt-passthrough caveats.

Which individual boards exist on this bench is site data and lives in
periculum's `rig.toml` (override with `$PERICULUM_RIG`). What *kind* of
board each is — how it is recognised over USB, which port carries which
role, what it can be asked to do — lives in `periculum/devices/*.toml`
and is the same everywhere. A scenario names the set of boards it needs:

```toml
profile = "rnode_lnode_pair"
```

which is resolved against the rig file. A scenario needing more boards
than the bench holds is `SKIPPED_INFRA` with a reason naming what was
missing — never RED. An absent board is not a protocol result.

### Firmware identity

Before any hardware scenario runs, `scripts/flash-lnodes-from-head.sh`
flashes every attached LNode from the current commit and reads its
`[FW_BUILD]` banner back over the debug serial to confirm the board
really runs that commit. A board whose firmware cannot be confirmed
makes the tier RED and is named in the verdict
(`firmware_unverified=<vid:pid>`): a run against unknown firmware must
never be silently trusted. This step is leviculum's, not periculum's —
periculum tests whatever firmware it finds and leaves board preparation
out of scope on purpose.

### Device-vanish watchdog

`scripts/run-tier3-hw.sh` polls `lsusb` once a second for the whole run
and latches the first drop below each USB id's baseline count. Under
VFIO controller passthrough the host cannot inject a phantom VM-side
disconnect, so a board that vanishes mid-run is always a real
device/firmware failure (suspected self-reset under load, Codeberg
#65), never an infra artefact. It forces RED with the board named
(`board_vanish=<vid:pid> firmware_self_reset_suspected`), and every
scenario verdict from the vanish onwards is untrusted.

## Troubleshooting

| Symptom | Action |
|---------|--------|
| Tier 1 never seems to run | It doesn't run itself. Type `just standard`. Nothing has started it automatically since the post-commit hook went (2026-08-07). |
| Notification never arrived | Expected: nothing that currently runs calls `notify-send` (see Notifications above). Check `last-results.txt`. |
| Tier 1 spuriously red | Check log; if Docker is involved, ensure no leftover containers (`docker ps -a`) |
| Timer didn't fire | `systemctl --user list-timers`, then `journalctl --user -u leviculum-ci-nightly.timer`. The nightly is the only timer this installer enables; Tier 2 has no timer. |
| Tier 2 looks like it never runs | It doesn't, unless started: `systemctl --user start leviculum-ci-tier2.service`. `scripts/ci-status.sh` prints how long it has been. |
| Disk filling up | Logs auto-rotate (14d/60d), but `~/.cache/leviculum-ci-target/` can grow large — clear with `cargo clean --target-dir ~/.cache/leviculum-ci-target` |
