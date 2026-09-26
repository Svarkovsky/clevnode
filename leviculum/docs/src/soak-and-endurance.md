# Soak and endurance

Two independent lines of evidence back the claim that `lnsd` runs as a stable,
long-lived transport node: an in-repo soak test that runs as a CI gate, and a
permanent node in the public Reticulum mesh. A third measure, poison-tolerant
locking, keeps a task panic under a lock from crashing the whole daemon.

## In-repo soak: the TCP-hub endurance test

`leviculum-std/tests/rnsd_interop/loadtest_tcp_hub_tests.rs` (Codeberg #101)
boots the real `lnsd` binary as an internet-facing transport hub, drives
sustained load plus connection churn against it, and samples the hub process's
`/proc/<pid>` RSS and open-fd count throughout. Because the hub is a separate
process, those samples are meaningful.

### Topology

```text
  N raw TCP clients ─┐            ┌─ sink (Single dest, TCP client)
  churn connections ─┼─▶  lnsd  ──▶┘
                     ┘  (transport hub)
```

A pool of steady TCP clients plus a set of churn workers (connections opened,
used, and closed in a tight loop) push sequence-numbered, encrypted single
packets through the hub to a sink daemon. The sink decrypts and folds each
`(client, seq)` into a per-source set, so delivery is verified exactly, not
sampled.

### What it asserts

On every run `report_and_assert` enforces:

- **100% delivery.** TCP is lossless, so every packet a client sends must arrive
  at the sink, contiguous and without duplicates. Any shortfall is a real hub
  bug, never noise. Connection-refused-under-load counts as zero-delivery, so
  backpressure failures cannot hide.
- **RSS plateau (no per-connection leak).** A steady population of connections
  legitimately costs memory, so growth from idle baseline to steady is expected.
  The leak signal is a *continuous climb* across the steady+churn phase, where
  thousands of connections are churned: the test compares the first vs second
  half of the steady-phase RSS samples and fails only when both a proportional
  and an absolute floor are exceeded, so a plateau with jitter never trips. A
  separate absolute ceiling over baseline is a runaway backstop.
- **fd bounded under churn, released after teardown.** Peak fd count must stay
  under `baseline + steady_conns + churn_workers + margin`, and after the
  clients close and drain, the count must fall back near baseline. A
  per-connection fd leak would blow past the ceiling and leave the end count
  elevated.
- **Clean hub log.** The hub's log is scanned for fatal/bad lines; expected
  churn-teardown lines are allow-listed, anything else fails the run.

### Two variants

| Test | Default load | Runtime | When |
|------|--------------|---------|------|
| `loadtest_tcp_hub_smoke` | 24 conns / 5 s | ~15 s | Tier 1 CI gate, every commit |
| `loadtest_tcp_hub_soak`  | 200 conns / 60 s | minutes | on demand / heavier validation |

Both are `#[ignore]`d because they spawn the `lnsd` binary, which the
`leviculum-std` test build does not itself produce — the binary is built first.

### Running it

Use the entrypoint, which builds `lnsd` (release) so the test's `locate_lnsd()`
finds it, then runs the right variant:

```sh
bash scripts/run-soak.sh          # smoke (~15 s + build)
bash scripts/run-soak.sh --full   # heavy soak (minutes)
```

The script honours the ambient `CARGO_TARGET_DIR` so the binary lands where the
test looks, prints the effective parameters, and exits non-zero on failure. A
passing run ends with a `PASS:` block plus the `rss plateau:` and `fds:` lines.

### Tuning

The soak reads these environment variables (defaults shown are the heavy-soak
values; the smoke variant uses smaller ones):

| env | default | meaning |
|-----|---------|---------|
| `LOADTEST_CONNS` | 200 | steady concurrent TCP client connections |
| `LOADTEST_SECS` | 60 | steady + churn duration (seconds) |
| `LOADTEST_PKT_MS` | 50 | per-connection inter-packet interval (ms) |
| `LOADTEST_CHURN_WORKERS` | 16 | connections repeatedly opened/closed |
| `LOADTEST_CHURN_PKTS` | 4 | packets per churn connection before close |
| `LOADTEST_MAX_RSS_GROWTH_PCT` | 40 | max steady-phase RSS growth |
| `LOADTEST_MAX_RSS_ABS_MIB` | 300 | absolute RSS ceiling over baseline |
| `LOADTEST_DRAIN_SECS` | 20 | post-load drain window for the fd check |
| `LOADTEST_LNSD_BIN` | auto | explicit path to the `lnsd` binary |

### Where it runs regularly

The smoke soak is wired into the Tier 1 `standard` CI target (`Justfile`), which
is run once per batch of work, and again by `just extensive` and `just nightly`,
which depend on it — so a green soak is produced regularly and left on record.
Until 2026-08-07 a post-commit hook also ran it after every commit; that hook is
gone and Tier 1 is now started explicitly. See
[CI Pipeline](development-ci.md). The heavier `--full` soak is run on demand.

## Real-world production soak: the `miauhaus` node

A permanent `lnsd` transport node (`miauhaus`) runs continuously in the public
Reticulum mesh, not in a lab harness. It has operated multi-day continuous as a
routing transport node, carrying real announce and path traffic, and has
survived a host reboot with no crash or self-reset observed. This is operational
endurance evidence alongside the synthetic soak: the daemon holds up under real,
unscripted mesh traffic over long uptimes.

Figures are kept deliberately conservative — multi-day continuous operation with
no crash is what is directly observed and defensible; no precise uptime hours are
claimed here.

## Crash containment: poison-tolerant locking

Endurance is not only about not leaking; it is about a fault in one path not
taking down the whole process. The shared-state `std::sync::Mutex` sites in
`leviculum-std` were previously locked with `.lock().unwrap()`, so one task
panicking while holding a lock poisoned it and crashed every later locker —
turning an isolated task panic into a whole-daemon crash.

`MutexRecover::lock_recover()` (`leviculum-std/src/sync_ext.rs`) locks with
`lock().unwrap_or_else(PoisonError::into_inner)` and is applied uniformly to the
non-test std-mutex locks across the driver, interfaces, RPC, and event-log
paths. This is "continue degraded, do not crash," not "isolate one interface":
the dominant lock is the node-wide `core` mutex, held on every RPC, connect, and
dispatch, so recovery continues node-wide state that may be mid-mutation from the
task that panicked. The guarantee is that one task panic no longer cascades into
a whole-daemon crash that drops every peer the node routes for; the first such
recovery logs a `tracing::warn` so the degraded state is visible in the logs.
