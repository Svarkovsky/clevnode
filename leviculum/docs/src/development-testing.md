# Testing — Developer Quick Reference

One-page orientation. See [CI Pipeline](development-ci.md) for the
automation details.

## TL;DR

- **Writing code**: run `cargo test -p <crate-you-touched>` as you go.
- **Committing**: nothing happens. No hook tests a commit.
- **Per batch of work**: run `just standard` (Tier 1) yourself. Nothing
  starts it for you.
- **Pushing**: Tier 0 runs automatically and blocks on fail.
- **Daily**: Tier 3 (02:00) runs via systemd. Tier 2 is on demand —
  nothing starts it for you.
- **Suite overview**: `just status`.

## Prerequisites (system tools)

The interop and integration tests shell out to real external tools, so these
system packages must be installed (all apt-installable on Debian):

- **docker** — the Tier 2 scenario corpora (run by periculum).
- **python3** + **Python RNS** (`rns`) — the `rnsd_interop` tests drive a real
  Python `rnsd`/`rnstatus` as the compatibility reference.
- **socat** — bridges a virtual serial pty pair so the serial-family interfaces
  (KISS, AX.25, Pipe) can be interop-tested against a real Python peer.
- **nomadnet** (optional) — the real NomadNet node used by the on-demand lnomad
  acceptance (`scripts/lnomad_nomadnet_acceptance.sh`, see [The lnomad
  acceptance](#the-lnomad-acceptance)). Not part of any tier; install with
  `pip install nomadnet`.
- **i2pd** (optional) — provides the SAM bridge on 127.0.0.1:7656 that the
  `I2PInterface` live tests use. The default suite covers `I2PInterface` with an
  in-process mock SAM bridge, so i2pd is not needed to go green; it only gates
  the `#[ignore]`d live tests (`cargo test -p leviculum-std i2pd_live --
  --ignored`). Enable the bridge with `sam.enabled = true` in
  `/etc/i2pd/i2pd.conf` and start the `i2pd` service.
- **cargo-fuzz** + **nightly** (optional) — drive the wire-format parser fuzz
  harness under `leviculum-core/fuzz` and `leviculum-std/fuzz` (see [Fuzzing
  the wire parsers](#fuzzing-the-wire-parsers)). Not part of any tier; install with
  `cargo install cargo-fuzz && rustup toolchain install nightly`.
- **just**, **cargo**, **flock**, **notify-send** — build/CI plumbing.

`scripts/install-ci.sh` checks for these at setup and prints the `sudo apt
install` hint for any that are missing. Whenever a test starts depending on a
new tool, add it BOTH here and to that check list so a fresh machine can be set
up from scratch.

## The four tiers

| Tier | When                              | Command         | Time      | Scope |
|------|-----------------------------------|-----------------|-----------|-------|
| 0    | on `git push` (hook)              | `just fast`     | ~3 min    | fmt + clippy + workspace lib tests |
| 1    | on demand, once per batch[^t1]     | `just standard` | ~15 min (40 min cold[^cold]) | Tier 0 + core/tests + ffi + proxy + rnsd_interop |
| 2    | on demand: `systemctl --user start leviculum-ci-tier2.service`[^t2] | `just extensive` | 30–90 min | Tier 1 + periculum `conformance/` + `regression/` |
| 3    | 02:00 daily (systemd timer)       | `just nightly`  | 2–6 h     | Tier 2 + LNode flash-from-HEAD + periculum `hardware/` |

Each tier includes every lower tier, so a green nightly proves the
whole stack.

[^t2]: Tier 2 had a 12:30/18:30 timer until 2026-06-12, when it was
retired in favour of on-demand runs (`scripts/install-ci.sh` step 9,
which also deletes any timer a previous install left behind). This page
went on advertising the timer, and `.githooks/pre-push` went on telling
people to "wait for the next scheduled run" until 2026-08-07 — see
[CI Pipeline](development-ci.md). `scripts/ci-status.sh` prints how long
it has been since a Tier 2 run was recorded.

[^t1]: A `post-commit` hook detached `scripts/run-tier1.sh` after every
commit until 2026-08-07. A commit is not a unit anybody wants tested — a
WIP commit, an amend and a commit mid-refactor each started the same
forty-minute docker run — and there is nothing an author can do about a
red gate that lands twenty minutes after the commit it judges. Removed;
see [CI Pipeline](development-ci.md) and the hook rule in
[Checks That Are Actually Checks](concepts/checks-and-citations.md).

[^cold]: `just standard` typed by hand builds in the repo's own
`target/`. The separate `CARGO_TARGET_DIR` at
`~/.cache/leviculum-ci-target` — so IDE builds and CI builds don't fight
over the same incremental cache — belongs to `scripts/run-tier1.sh`,
which nothing has started since the post-commit hook went. Either way the
first run against a cold target dir compiles the workspace from scratch
(~40 min); subsequent runs are incremental (~15 min).

Results go to `~/.local/state/leviculum-ci/last-results.txt`:
`GREEN` = passed, `RED` = failed, `SKIPPED` = deferred because another
test held the lock (see "Concurrent runs" below). No tier raises a
`notify-send` alarm today — read the ledger, or `just status`. See
[CI Pipeline](development-ci.md).

## While writing code

Fast feedback. Run only what you changed:

```sh
cargo test -p leviculum-core --lib   # touched core lib code
cargo test -p leviculum-std          # touched std
cargo clippy -p leviculum-core       # clippy for one crate
cargo fmt                             # apply formatter (not --check)
```

End of a batch of work:

```sh
just standard                        # Tier 1 (~15 min)
```

This is the "15-minute-budget" check that CLAUDE.md expects after
every task, and typing it is the only thing that runs it.

**Never** run a full scenario corpus casually:

```sh
# DON'T do this without a reason — the containers and the USB handles
# collide with anything else running scenarios on the box.
periculum run ../periculum/conformance
```

If you must, use `just extensive`, which builds the binaries the nodes
mount first and is lock-protected.

## Before pushing

Nothing to type. `git push` triggers `.githooks/pre-push`, which lints
the Woodpecker pipelines (`.githooks/pre-push:21`) and then runs
`just fast` (Tier 0, `.githooks/pre-push:24`). A red Tier 0 aborts the
push — fix, stage, and push again.

That is the whole hook. It used to also block on Tier 2 staleness, at
5 commits/8 h (warn) and 10 commits/24 h (block); the block was
unsatisfiable and was removed on 2026-08-07, along with the
`git push --no-verify` habit it taught. See
[CI Pipeline](development-ci.md).

## After committing

Nothing. There is no `post-commit` hook — deliberately, since
2026-08-07 (footnote [^t1] above). Tier 1 is `just standard`, typed once
per batch.

## Checking state

```sh
just status                                  # last result per tier
just logs                                    # tail most recent Tier 1 log
cat ~/.local/state/leviculum-ci/last-results.txt   # full history
```

## LoRa hardware tests

Tier 3 only. Requires two Heltec T114 boards + two RNode radios
connected via USB. Manual runs:

```sh
just flash                        # flashes ALL attached T114s; touch-free
                                  #   since the Bug #13 firmware change.
                                  #   Double-tap RESET only if the runner
                                  #   prompts you (crashed-firmware fallback).
just flash-one /dev/ttyACM3       # flash one specific T114 (A/B testing)
just nightly                      # full Tier 3 run
```

A single LoRa scenario in isolation:

```sh
periculum run ../periculum/hardware/<name>.toml
```

Hardware scenarios are not gated behind a flag: they live in
`hardware/`, and periculum decides from the scenario itself whether
this bench can serve it. One that binds a board the bench does not
hold reports `SKIPPED_INFRA` naming what was missing, never RED.

### Radio duty-cycle lock is OFF by default in tests

The harness writes `airtime_limit_long = 0` into every generated
radio interface (single RNode, multi-vport RNode, serial LNode), so
the firmware duty-cycle airtime lock never engages mid-run. Without
this, the driver's lawful-by-default ETSI cap (#55) silently stops a
saturating sender once its rolling-hour airtime hits 10 %: the modem
stops radiating while still accepting frames, which reads from above
as an intermittent resource stall. A test that itself exercises the
duty-cycle lock opts back in explicitly:

```toml
[radio]
frequency = 869463000
airtime_limit_long = 10   # percent; arms the 10 % ETSI cap
```

or per subinterface under `[[nodes.x.rnode_interfaces]]`, or for a
one-off run with `LORA_AIRTIME_LIMIT_LONG=10`.

## Concurrent runs

Only one scenario run can be in flight at a time — Docker names and
USB handles would otherwise collide. A second invocation exits in
under a second:

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

A scheduled Tier 2 / Tier 3 that hits this case logs `SKIPPED`,
not `RED`, and sends a normal (not critical) notification. **No
action needed** — the next scheduled slot runs normally. In
practice this means: if you're doing late-night hardware work and
the 02:00 nightly fires, it silently defers. You don't need to
stop it.

Unit tests in the leviculum crates (`leviculum-core`,
`leviculum-std`, `leviculum-ffi`, `leviculum-proxy`,
`leviculum-cli`) run in parallel with a held scenario lock — they
never touch containers or boards.

## Installing / updating the CI

```sh
just install-ci
```

Idempotent. Installs git hooks, systemd user units, state dirs,
separate cargo target dir. Safe to re-run after pulling.

## The lnomad acceptance

`lnomad` is the terminal NomadNet browser. Its end-to-end acceptance drives a
**real** NomadNet node rather than a mock: NomadNet runs as the shared Reticulum
instance and as a node server hosting a known `index.mu`; `lnomad --print`
fetches and renders that page over the shared-instance path, and the script
asserts the rendered output contains the known content.

```sh
../periculum/periculum/assets/scripts/lnomad_nomadnet_acceptance.sh
```

It prints `ACCEPT-PASS` / `LNOMAD-ACCEPT-COMPLETE` and exits 0 on success, or
`ACCEPT-FAIL: <reason>` and non-zero otherwise. It creates an isolated RNS +
NomadNet config under a temp dir and always cleans up (kills nomadnet, removes
the temp dir) on exit.

This is an **on-demand** acceptance — it is NOT wired into any tier, because it
needs `nomadnet` (and its Python `RNS`) installed and takes ~30 s of real
announce/link setup. Requirements and overrides:

- **nomadnet** on PATH (or point `NOMADNET` at the executable).
- **python3 + RNS** on PATH (or point `PY` at the interpreter).
- The musl `lnomad` release binary; the script builds it (`cargo build --release
  -p lnomad`) if it is missing. Override its location with `LNOMAD`, or the
  cargo target dir with `CARGO_TARGET_DIR`.
- Tune timing with `NN_SETTLE` (nomadnet startup, default 25 s) and
  `LNOMAD_TIMEOUT` (fetch timeout, default 40 s).

## Fuzzing the wire parsers

The functions that parse UNTRUSTED bytes off the wire (packet, resource
advertisement, discovery announce app-data, announce field-slicer, IFAC,
HDLC/KISS deframers, I2P SAM reply lines) have a coverage-guided fuzz harness
(cargo-fuzz / libFuzzer). A parser that panics, overflows, hangs, or OOMs on
malformed input is a remote DoS, so each target asserts graceful `Err`/`None`.

There are two detached fuzz crates, one per library crate that owns a parser:

- `leviculum-core/fuzz` — `packet_unpack`, `resource_advertisement_unpack`,
  `discovery_announce`, `announce_from_packet`, `ifac_verify`, `hdlc_deframe`,
  `kiss_deframe`.
- `leviculum-std/fuzz` — `sam_parse` (the I2P SAM reply-line parser and the
  base64/destination decoders it feeds).

`announce_from_packet` (the `ReceivedAnnounce::from_packet` field-slicer) and
`sam_parse` were added in Codeberg #108 alongside the #23 targets. Both reach
crate-internal parsers through a `#[cfg(fuzzing)]`-gated `fuzz` module in each
crate, so no fuzz-only surface leaks into the normal public API.

This is NOT part of any tier (`just standard` never runs it). It needs nightly
+ cargo-fuzz and the glibc host target (the workspace defaults to musl, which
ASan does not want):

```sh
cd leviculum-core
# 30 s smoke on one target (catches shallow crashes):
cargo +nightly fuzz run resource_advertisement_unpack \
    --target x86_64-unknown-linux-gnu \
    seeds/resource_advertisement_unpack -- -max_total_time=30 -max_len=8192
cargo +nightly fuzz list          # all targets

cd ../leviculum-std              # the SAM target lives in its own crate
cargo +nightly fuzz run sam_parse \
    --target x86_64-unknown-linux-gnu \
    fuzz/seeds/sam_parse -- -max_total_time=30 -max_len=8192
```

Any crash the fuzzer finds is fixed at the root AND pinned by a deterministic
regression unit test in the normal suite, so it stays fixed without the fuzzer.
Deep continuous fuzzing (hours per target) is a nightly/CI follow-up, not part
of the 15-minute budget. See `leviculum-core/fuzz/README.md` and
`leviculum-std/fuzz/README.md` for the target lists and exposure ranking.

## Golden rules

- Tests are never flaky. A failure is a real bug — diagnose and
  fix at the root, don't retry until green.
- Don't commit while tests are red.
- `#[ignore]` is only for hardware-dependent tests. For
  CPU-expensive non-hardware tests, use a Cargo feature flag.

## The ignored-test census

An `#[ignore]`d test is run by nothing. Codeberg #189 found one such
suite that had been broken for a month with no gate anywhere to say so,
so the size of that bucket is pinned per test unit in
`scripts/ignored-counts.txt` and checked by
`scripts/check-ignored-counts.py` at the end of `just standard`. The
census is exhaustive: every test executable in the workspace plus every
package's doc-tests, with units absent from the pin file expected to
have zero, so a new test binary is covered without an entry.

A new `#[ignore]` therefore fails Tier 1 until you either route the test
into a tier or raise its number in the pin file — a one-line diff, on
purpose, with the reason belonging in the commit message. `python3
scripts/check-ignored-counts.py --print` dumps the current census in
pin-file format.

Routing an ignored test by name (rather than lifting the ignore) is what
`scripts/run-status-parity.sh` does for the three `status_parity` tests,
which need to run serially. A test filter that matches nothing exits 0,
so any such script must also assert how many tests actually ran.
