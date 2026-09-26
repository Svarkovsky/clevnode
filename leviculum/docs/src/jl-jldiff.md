# `jl` and `jldiff` — filtering and comparing structured event logs

Stage 7 / Codeberg #39 piece 4.  Two CLI tools that consume the
structured event-log format established by
[`structured-event-logs.md`](structured-event-logs.md):

- **`jl`** — filter and slice an event log.  Reads from stdin or
  one or more files, applies AND-combined filters, emits matching
  lines unchanged.
- **`jldiff`** — compare two event logs by an alignment-key tuple.
  Partitions events into LEFT_ONLY / RIGHT_ONLY / MATCHED_DIFFER /
  MATCHED_IDENTICAL buckets.

> **Examples below are verified by `tests/jl_jldiff_docs.rs`.**
> If you change a worked example here, update the test;
> if a worked example breaks, the doc is wrong, not the test.

## Test infrastructure

The tools are exercised by six test files in `leviculum-std/tests/`:

- `jl_filter.rs` — Phase A unit/integration tests for `jl` in isolation.
- `jldiff_compare.rs` — Phase B unit/integration tests for `jldiff` in isolation.
- `jl_jldiff_workflow.rs` — end-to-end Subscriber → binary tests.
- `jl_jldiff_fixtures.rs` — checked-in real-shape log files driving expected outputs.
- `jl_jldiff_edge_cases.rs` — boundary and adversarial inputs.
- `jl_jldiff_docs.rs` — every example below is mirrored here as a test.

When a Stage-6 format change drifts these tools, all six of those
files are likely to fail at once; that is the intended signal.

## Format recap

```
EVENT_NAME node=<name> k1=v1 k2=v2 ... kN=vN t=<rel-ms>
```

- `EVENT_NAME` first.
- `node=<name>` always second when present.
- Other fields alphabetically sorted.
- `t=<ms>` always last; integer ms relative to subscriber init.

Lines that do not fit the structured shape (banners, free text,
cargo-test output) pass through `jl` unchanged.  See
[`structured-event-logs.md`](structured-event-logs.md) for the
full format spec, the runtime catalogue, and the violation-line
synthesis rules.

---

## `jl` — filter binary

```text
jl [--filter <expr>]... [--node <name>] [--since-event <NAME>] [--until-event <NAME>] [INPUT...]
```

| Flag | Effect |
|------|--------|
| `--filter <expr>` | Filter expression.  Repeatable; AND-combined. |
| `--node <name>` | Shorthand for `--filter node=<name>`. |
| `--since-event <NAME>` | Drop everything before the first event whose `EVENT_NAME` is `<NAME>`.  The matching event is **included**.  At most one. |
| `--until-event <NAME>` | Drop everything at and after the first event whose `EVENT_NAME` is `<NAME>`.  The matching event is **excluded**.  At most one. |
| `INPUT...` | Optional file paths.  Without any, reads stdin.  Multiple files are read in order; output preserves order. |

Filter expression forms:

| Form | Meaning |
|------|---------|
| `key=value` | exact match |
| `key=*` | event has that key (any value) |
| `key=prefix*` | value starts with `prefix` |
| `t<N`, `t>N`, `t<=N`, `t>=N` | numeric `t` comparison |

The `event` key is special-cased: `event=PKT_RX` matches BOTH a
real `PKT_RX ...` line (where the EVENT_NAME first token is
`PKT_RX`) AND a synthetic violation line whose explicit `event=`
field is `PKT_RX`.  This makes the filter consistent across real
events and the `EVENT_SCHEMA_VIOLATION` / `EVENT_FIELD_VIOLATION`
lines that reference them.

### Example 1: filter to one event-name

Input:

```
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=21
PKT_RX node=alpha dst=abc2 hops=1 iface=lora0 len=64 type=Data t=80
```

Command:

```sh
jl --filter event=PKT_RX
```

Output:

```
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PKT_RX node=alpha dst=abc2 hops=1 iface=lora0 len=64 type=Data t=80
```

### Example 2: slice between two markers

Input:

```
PKT_LOCAL node=alpha dst=abc1 iface=lora0 matched=true t=10
PKT_LOCAL node=alpha dst=abc1 iface=lora0 matched=true t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=30
PKT_RX node=alpha dst=abc2 hops=0 iface=lora0 len=64 type=Data t=40
PKT_RX node=alpha dst=abc3 hops=0 iface=lora0 len=64 type=Data t=50
PKT_DROP node=alpha dst=abc4 hops=3 iface_in=lora0 reason=ttl_expired type=Data t=60
PKT_RX node=alpha dst=abc5 hops=0 iface=lora0 len=64 type=Data t=70
```

Command:

```sh
jl --since-event PATH_ADD --until-event PKT_DROP
```

Output:

```
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=30
PKT_RX node=alpha dst=abc2 hops=0 iface=lora0 len=64 type=Data t=40
PKT_RX node=alpha dst=abc3 hops=0 iface=lora0 len=64 type=Data t=50
```

### Example 3: time window

Input (same as Example 1), with one extra later event:

```
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=21
PKT_RX node=alpha dst=abc2 hops=1 iface=lora0 len=64 type=Data t=80
PKT_RX node=alpha dst=abc3 hops=2 iface=lora0 len=64 type=Data t=200
```

Command:

```sh
jl --filter t>=20 --filter t<100
```

Output:

```
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=21
PKT_RX node=alpha dst=abc2 hops=1 iface=lora0 len=64 type=Data t=80
```

---

## `jldiff` — compare binary

```text
jldiff --align-on <key>[,<key>...] LEFT_FILE RIGHT_FILE
```

The alignment-key tuple groups events on each side.  Each group's
events are paired by file order (1st left ↔ 1st right, …); surplus
events on either side go to `LEFT_ONLY` / `RIGHT_ONLY`.  Events
missing one of the align-keys are unalignable and surface in the
appropriate `_ONLY` bucket with an `[unalignable: missing key X]`
annotation.

Output format:

```
=== LEFT_ONLY (N events) ===
<event line>
...

=== RIGHT_ONLY (N events) ===
<event line>
...

=== MATCHED_DIFFER (N pairs) ===
L: <left event line>
R: <right event line>
   DIFF: key=lvalue|rvalue [key=lvalue|rvalue ...]

=== MATCHED_IDENTICAL (N pairs) ===
```

`MATCHED_IDENTICAL` is count-only — events with no field
differences are not re-listed.  The `t=` field is reported in DIFF
lines when it differs (which is normal — alignment keys are how
you say "same logical event"; t shifts naturally between runs).

### Example 4: compare two mvr-test runs

`a.log`:

```
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=11
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20
```

`b.log`:

```
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=15
PATH_ADD node=alpha dst=abc1 hops=2 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=16
```

Command:

```sh
jldiff --align-on event,dst a.log b.log
```

Output:

```
=== LEFT_ONLY (1) ===
ANN_RX node=alpha dst=abc1 hops=0 iface=lora0 path_response=false t=20

=== RIGHT_ONLY (0) ===

=== MATCHED_DIFFER (2 pairs) ===
L: PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
R: PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=15
   DIFF: t=10|15

L: PATH_ADD node=alpha dst=abc1 hops=0 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=11
R: PATH_ADD node=alpha dst=abc1 hops=2 iface=lora0 next_hop=alpha ok=true source=announce table_len=1 t=16
   DIFF: hops=0|2 t=11|16

=== MATCHED_IDENTICAL (0 pairs) ===
```

### Example 5: multi-key alignment (lnsd vs Python-RNS)

`a.log` (lnsd):

```
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
PKT_RX node=alpha dst=abc1 hops=0 iface=tcp1 len=64 type=Data t=20
```

`b.log` (Python-RNS):

```
PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=12
PKT_RX node=alpha dst=abc1 hops=0 iface=tcp1 len=64 type=Data t=22
```

Command:

```sh
jldiff --align-on event,dst,iface a.log b.log
```

Output:

```
=== LEFT_ONLY (0) ===

=== RIGHT_ONLY (0) ===

=== MATCHED_DIFFER (2 pairs) ===
L: PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=10
R: PKT_RX node=alpha dst=abc1 hops=0 iface=lora0 len=64 type=Data t=12
   DIFF: t=10|12

L: PKT_RX node=alpha dst=abc1 hops=0 iface=tcp1 len=64 type=Data t=20
R: PKT_RX node=alpha dst=abc1 hops=0 iface=tcp1 len=64 type=Data t=22
   DIFF: t=20|22

=== MATCHED_IDENTICAL (0 pairs) ===
```

The multi-key tuple `(event, dst, iface)` keeps the two interfaces
separate even though both have the same `event` and `dst` —
without `iface` in the key, jldiff would multi-occurrence-pair
them in file order, which is fine but obscures the per-interface
view.

---

## Workflow notes

When an mvr-test fails, the dump goes to stderr framed by
`=== EVENT LOG DUMP ... ===` banners.  Pipe it through `jl` to
narrow:

```sh
just mvr 2>&1 | jl --filter event=PATH_ADD
```

The banners and any free-text lines around the dump pass through
unchanged; only the structured events filter.

For an A/B comparison between two runs, capture each run's output
to a file and run `jldiff`:

```sh
# Run a baseline; capture only the structured events.
just mvr 2>&1 | jl > baseline.log

# Run again after a change.
just mvr 2>&1 | jl > candidate.log

# Diff aligned on the event identity.
jldiff --align-on event,dst,iface baseline.log candidate.log
```

For multi-process logs, the Stage-6
[`merge_event_logs`](structured-event-logs.md#multi-process-merge)
helper produces a t-ordered union; `jl` and `jldiff` then operate
on the merged file as if it came from a single subscriber.

---

## See also

- [`structured-event-logs.md`](structured-event-logs.md) — Stage-6
  format spec, subscriber architecture, runtime catalogue.
- [Codeberg #39](https://codeberg.org/Lew_Palm/leviculum/issues/39)
  — the test framework epic this batch closes.
