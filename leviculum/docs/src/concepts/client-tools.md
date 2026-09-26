# Client Tools

Leviculum ships command-line tools next to the daemon, and every tool
we will ever ship is governed by the same small set of rules. This page
records them for whoever adds the next tool: the counterpart rule, the
drop-in contract, the explicit permission to exceed the reference, the
case where a tool with no Python equivalent is the right deliverable,
the one situation in which our own tool must *not* be used, and the
marking rule for tools that can forge traffic.

The daemon-level half of this story — shared-instance IPC and config
compatibility, and why drop-in is a design goal rather than an accident
— is in [Python-RNS Compatibility](python-rns-compatibility.md). This
page extends the same property from the daemon to the clients.

## A counterpart for every reference tool

The reference stack ships a family of utilities under
`reference/Reticulum/RNS/Utilities/`. The rule is one `ln*` counterpart
per reference tool. The honest current state, as of 2026-08:

| Reference tool | Counterpart | State |
|---|---|---|
| `rnsd` | `lnsd` | Shipped. Drop-in at IPC and config level; see [Python-RNS Compatibility](python-rns-compatibility.md). |
| `rnstatus` | `lnstatus` | Shipped. Local-mode output is byte-parity-pinned against the reference by the 2×2 matrix `status_parity_matrix_2x2` (`status_parity_tests.rs:1392`), the reported inventory by `status_inventory_parity_across_daemons` (`status_parity_tests.rs:2087`); Periculum wiring is Codeberg #174. |
| `rncp` | `lncp` | Shipped (send, fetch, listen). |
| `rnprobe` | `lnprobe` | Shipped. Same command line, output, and exit codes; proven against both `lnsd` and `rnsd` over the shared instance. |
| `rnpath` | — | Missing; filed as Codeberg #173. |
| `rnid` | — | Missing, not yet filed. |
| `rnx` | — | Missing, not yet filed. |
| `rnsh` | — | Missing, not yet filed. |
| `rnir` | — | Missing, not yet filed. |
| `rnpkg` | — | Missing, not yet filed. |
| `rnodeconf` | — | Missing, not yet filed. (`just flash`/`just flash-rnode` cover our own rig's flashing needs, but they are build tooling, not a counterpart.) |

The vendored 1.3.5 tree also carries `rngit` as a utility subpackage;
the counterpart rule covers it the same way, and it is likewise
missing and unfiled. "Not yet filed" rows are gaps in the issue
tracker, not decisions to skip the tool — file the issue when work on
one begins.

Alongside the counterparts we ship tools with no reference equivalent:
`lnstest` (test and diagnostics driver, see below), `lnomad` (NomadNet
browser; its reference counterpart is the NomadNet application rather
than an RNS utility), and `lblogd` (blog daemon). The counterpart rule
does not restrict these; the drop-in rule below does not apply to them
because there is no reference surface to be compatible with.

## Drop-in first

**With the reference tool's arguments, our tool behaves like the
reference tool.** Same flags mean the same thing, same exit codes,
and the output is something a script written for the Python tool can
still parse. This is the daemon-level drop-in property extended to
the clients, and it is what lets a comparison harness point one
driver at either stack.

The worked example is `lnstatus`: the renderer consumes the same
`interface_stats` dict a Python `rnsd` exposes, and feeding an
identical stats dict into `lnstatus` and `rnstatus` yields
byte-identical output (`lnstatus_render.rs:1-15`, pinned by the 2×2
parity matrix, which drives both clients against both daemons after
byte-identical controlled traffic, `status_parity_tests.rs:1-30`).
`lnstatus` mirrors the reference flag surface — `-a`, `-A`, `-P`,
`-l`, `-j/--json`, `-m/--monitor`, `-s/--sort`, the name filter — with
the reference meanings, because those flags and formats are the
compatible surface a user's muscle memory and a user's scripts depend
on.

Drop-in is judged against the vendored reference
(`reference/Reticulum`), which is the same source of truth the
protocol work measures against. When the reference tool's own output
changes between versions, the bridge parsers in Periculum absorb it —
compatibility of meaning, not a frozen byte format, per
[Wire Field Semantics](wire-field-semantics.md).

## Drop-in is about the answer, not just the query

A client tool asks a daemon a question. Drop-in means the answer describes
the same world, not merely that the query succeeded — and an answer about a
different world is the hardest kind of difference to notice, because nothing
fails.

Codeberg #177 was exactly that. `rnstatus` against `rnsd` listed three
interfaces; against `lnsd` it listed none, and the query returned cleanly
both times. The cause was structural: our `interface_stats` was assembled
from `Transport`'s routing map — the interfaces the core can *send packets
on* — while a Python `rnsd` reports `RNS.Transport.interfaces`, everything
`Reticulum` runs (`Reticulum.py:1334`). Listeners carry no packets, so they
were in no collection at all on our side, and their absence was the symptom.
The reporting inventory now lives in the driver
(`leviculum-std/src/interfaces/inventory.rs`) and `interface_stats` reports
the union: transport's routable interfaces plus the listeners the daemon
runs. Transport stays free of listener rows, which it would otherwise try
to send on.

Codeberg #190 was the same shape one field down. A radio row's `bitrate`
answered with the per-medium `BITRATE_GUESS` — 10 Mbit/s, TCP's — because the
key was filled only from a configured `bitrate`, and a radio configures none.
Python fills it from the interface itself, which for an RNode is the on-air
rate derived from the radio settings (`RNodeInterface.updateBitrate`,
RNodeInterface.py:693-696). The precedence is now Python's, in Python's order:
a configured `bitrate` first (`if configured_bitrate: interface.bitrate =
configured_bitrate`, Reticulum.py:887), else the interface's own rate for its
medium, else the guess. Nothing failed while it was wrong; a client asking how
fast the air is simply got an answer about a different medium.

### What appears, and under what name

The name is the interface's identity to a script, so each row reproduces the
reference `__str__` exactly:

| Row | Name | `short_name` | `type` | Reference |
|---|---|---|---|---|
| shared-instance server | `Shared Instance[rns/<instance>]` | `Reticulum` | `LocalServerInterface` | LocalInterface.py:391, 496-498 |
| an accepted IPC client | `LocalInterface[rns/<instance>]` | `<n>@\0rns/<instance>` | `LocalClientInterface` | LocalInterface.py:372-374, 441 |
| a TCP listener | `TCPServerInterface[<section>/<ip>:<port>]` | `<section>` | `TCPServerInterface` | TCPInterface.py:666-672 |
| a connection it accepted | `TCPInterface[Client on <section>/<ip>:<port>]` | `Client on <section>` | `TCPClientInterface` | TCPInterface.py:443-449, 577 |

`<section>` is the config section name (`[[My TCP Server]]`), which is
Python's `interface.name`; it is carried on `InterfaceConfig::name` because
flattening the parsed config used to drop it. A spawned row also carries
`parent_interface_name` / `parent_interface_hash` pointing at its listener
(`Reticulum.py:1342-1344`), and `hash` is the full 32-byte
`Identity.full_hash(str(interface))` on both stacks, so a script may key
interfaces by hash across daemons. A listener reports `clients` (its live
spawned count) and its children's byte totals, including those of children
that have since disconnected — the reference gets the latter for free by
incrementing the parent counter alongside the child's
(`TCPInterface.py:306-308`).

### Pinned deviations

Each is a decision, not an accident, and each has a test that fails if it
drifts:

- **A listener's frequency fields are the sum of its live children's**, where
  the reference keeps a deque on the listener itself
  (`TCPInterface.py:634-644`). Identical at rest (both read exactly 0, which
  is what the frozen comparisons assert) and equal in aggregate under load;
  they can differ while a child that contributed samples has already
  disconnected.
- **Interfaces other than the four rows above still report our internal
  name** (`tcp_client_0`, `rnode_0`, `auto/eth0/…`) rather than the
  reference's `TCPInterface[<section>/<host>:<port>]` family. A script that
  keys on those names still sees an unfamiliar identity; the drop-in gap is
  narrowed, not closed.
- **Config interfaces are ordered by section name**, not config-file order:
  the parsed config is a map keyed by section name, so file order is not
  recoverable. Deterministic run to run, which HashMap iteration was not.
- **Extra and missing keys**: ours adds `announce_queue` and `peers`, the
  reference adds `autoconnect_source`. Pinned exactly by
  `assert_daemon_stats_parity`.
- **`tx_jitter_max`** (Codeberg #190): the ceiling, in seconds, of the
  randomised pre-TX delay an interface draws against before a frame goes on
  the air. The reference has no equivalent — Python's `RNodeInterface` leaves
  medium access to the RNode firmware's CSMA and holds no such attribute, so
  nothing in `get_interface_stats` (Reticulum.py:1326-1470) reports it. Purely
  additive: `rnstatus` reads every field by name (`ifstat["name"]`,
  rnstatus.py:391; `if "<key>" in ifstat` for the optional ones) and never
  enumerates an interface dict, so an unknown key is not read. Emitted only
  where the concept applies, the way Python gates `airtime_short` and friends
  on `hasattr`, which is why it does not appear in the TCP-only rows the
  parity matrix compares.
- **An IPC client's `short_name` index** mirrors the reference's live-client
  count at accept time (`LocalInterface.py:441/355`), so the labels of two
  daemons agree only when their clients connected and left in the same order.

## Exceeding is allowed and wanted

Additive flags and richer output are welcome. The constraint is only
that the **reference-compatible surface stays reference-compatible**:
an extra flag must not change what an existing flag does, and extra
output must not break what a Python-tool script would parse.

Current examples: `lnstatus --instance_name`
(`leviculum-cli/src/lnstatus.rs`) selects a shared instance
by name where the reference tool only reads it from the config file,
and `lnstatus --tables` (the second half of Codeberg #174) exposes
internal tables `rnstatus` cannot show at all.
Both are additive: run `lnstatus` with exactly `rnstatus`'s
arguments and you get `rnstatus`'s behaviour.

### The shape an additive dump takes

`--tables` is the worked example, and three of its decisions generalise
to the next one.

**Additive key, not an envelope.** The tables go into the `-j` object
under one new key rather than wrapping it, so the stats dict stays the
top-level object. Everything that parses `lnstatus -j` today — Periculum's
`parse_status` scans for the line whose object carries `interfaces` —
keeps working untouched, and `-j` without the flag is what it always was.
Wrapping would have been tidier and would have broken every existing
reader.

**Reference names where the reference has one, its own vocabulary where
it does not.** Python serves exactly one of these tables over RPC
(`get_path_table`, Reticulum.py:1516-1538), so those six keys and their
units are taken verbatim and our one addition sits beside them. The other
tables Python holds but never exposes; it names their fields only by list
index (`IDX_RT_*`, `IDX_LT_*`, `IDX_AT_*`, `IDX_TT_*`,
Transport.py:3556-3586), so the string keys are ours, spelled after those
constants. The additive keys are safe against a Python reader for the same
reason `tx_jitter_max` is: every Python consumer of an RPC response reads
it by name and none enumerates it.

Naming collides once, and it is worth knowing about: `link_table` in the
dump is `Transport.link_table`, the links this node *relays*. The
pre-existing `link_table` RPC (`lnstest diag`) is the links this node
*terminates*, and appears in the dump as `local_links`. The reference name
won the contested word because the reference has a table by that name; the
inventory the reference has no table for at all took the qualified one.

**Absent is not empty.** A daemon that implements the query answers with
the key present and its tables possibly empty. A daemon that does not — a
Python `rnsd`, or an `lnsd` from before the flag — makes the client omit
the key, print why on stderr, and exit 0. Presence therefore distinguishes
"cannot answer" from "nothing there". Nulling the key, or defaulting it to
empty lists, would have made every assertion about an empty table pass
silently against a daemon that cannot answer it — the read-side tolerance
question of Codeberg #183, one layer up. Python's `rpc_loop` matches no arm
for an unknown command and falls through to `conn.close()`
(Reticulum.py:1213-1260), so the absence surfaces as a fast transport error
rather than a hang; that is pinned against a real `rnsd` in
`reverse_rpc_interop_tests`.

One honesty note, because it is easy to get wrong in the other
direction: `-j/--json`, `-m/--monitor` and the announce/path-request/
link statistics flags are **not** exceedances — the reference
`rnstatus` has all of them (`rnstatus.py:685-706`), and ours mirror
them under the drop-in rule. Claiming reference-mirrored features as
our extensions would misstate where the compatible surface ends;
check the reference before calling a flag additive.

## New tools where testing gains from them

`lnstest` exists because Periculum needed a driver the Python tool
set does not offer — deterministic selftest phases (delivery,
ratchet, link) with machine-parseable summary lines
(`leviculum-cli/src/lnstest.rs:1-4`). That is the precedent: **when a
test cannot be written because no tool can express it, the tool is
the deliverable.**

The currently open examples — not a closed list — are:

- Codeberg #175: wire-level tools, a packet injector and a decoder
  with no Python equivalent.
- Codeberg #176: a structured event tap, so Periculum can assert on
  events instead of scraping container logs.

A new tool of this kind has no reference surface, so the drop-in rule
does not bind it; the [evidence rules](evidence-and-honesty.md) do. In
particular a test tool's output is a diagnostic indicator: it must
measure the production path and be observed in both states before its
green is believed.

## The rule a comparison must not break

A stack comparison drives the **same client against both daemons**.
That is the whole point of the drop-in property, and it cuts both
ways: our own client may not replace the reference tool in the very
tests that measure against the reference. Substituting "our better
tool" on one side smuggles config, cadence and timeout differences
into a result that claims to be about the stacks — the parallel-driver
failure described in
[Evidence and Honesty](evidence-and-honesty.md#reference-first-for-compatibility-bound-behaviour).

So: `lnstest selftest` pointed at either daemon is a valid A/B.
`lnstatus` against `lnsd` compared with `rnstatus` against `rnsd` is
not a stack comparison — it varies two things at once. The 2×2 parity
matrix (`status_parity_tests.rs:5-30`) is the shape that untangles
this: each client against each daemon, so client-render parity and
daemon-stats parity are separated instead of conflated. This is the
one place where "use our better tool" is wrong.

## A tool that can forge is marked as such

Anything that emits crafted frames — the planned packet injector of
Codeberg #175 first among them — refuses to run without an explicit
flag acknowledging that it forges traffic, and names itself in its
output so a capture containing forged frames is attributable. A
crafted frame in a mesh is indistinguishable from a real one by
design; the honesty has to live in the tool. No shipped tool forges
today; this rule binds the first one that does.

## Adding the next tool: the checklist

1. **Name and scope.** `ln*` counterpart of one reference tool, or a
   new testing tool per the precedent above. Check the tracker first
   (#173 covers probe and path query).
2. **Drop-in surface.** Implement the reference tool's flags with the
   reference tool's meanings and exit codes. Divergence from the
   reference's *internals* is fine under the
   [deviation rule](python-rns-compatibility.md#the-deviation-rule);
   divergence of the compatible surface is not.
3. **Parity evidence.** Pin the drop-in claim with a test that could
   fail — the 2×2 matrix of `status_parity_tests.rs` is the model.
4. **Periculum wiring.** Add a client manifest
   (`periculum/periculum/adapters/clients/*.toml`) and an output
   parser in the bridge (`periculum/periculum/src/bridge.rs`), so
   scenarios can drive the tool and assert on its output.
5. **Docs.** Guide page, man page, and both in `docs/src/SUMMARY.md`.
6. **Forgery marking**, if the tool can emit crafted frames.

## See also

- [Python-RNS Compatibility](python-rns-compatibility.md) — the
  daemon-level drop-in property this page extends, and the deviation
  rule.
- [Evidence and Honesty in Testing](evidence-and-honesty.md) — the
  A/B discipline the comparison rule enforces.
- [Wire Field Semantics](wire-field-semantics.md) — compatibility as
  meaning, not bytes.
