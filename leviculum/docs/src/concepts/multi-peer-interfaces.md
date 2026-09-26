# An interface that holds several peers

A TCP listener, an AutoInterface, a BLE radio and an I2P endpoint all
have the same shape: one configured section, many peers behind it. Each
one has to answer the same question, and the answer decides how much
the rest of the stack has to know about the carrier:

> When the transport wants these bytes to reach **one** of the peers
> behind this interface, how does it say which one?

This page records the answers we run today, the answer the references
run, what one more interface actually costs measured on the firmware,
and which model we take forward. It is a design document, not a status
page: what is open belongs on the tracker.

The rule it has to live under is
[Interface isolation](interface-isolation.md) — only the interface
knows its medium. Nothing below weakens that; the whole argument is
about *where the peer-to-link map lives*, and in every option it lives
on the interface's side of the boundary.

## What we do today, and it is not one thing

### TCP server: a child interface per connection

The listener binds and accepts; every accepted connection becomes its
own `InterfaceHandle` with its own id, drawn from the shared counter
(`spawn_tcp_server`, `leviculum-std/src/interfaces/tcp.rs:361`). The
child is built from the already-connected stream
(`spawn_tcp_interface_from_stream`,
`leviculum-std/src/interfaces/tcp.rs:428`), inherits IFAC, mode and
ingress control from the listener, and is handed to the event loop,
which registers it in the routing map like any other interface
(`registry`, `leviculum-std/src/driver/mod.rs:4327`).

The listener itself is deliberately **not** in that map. It carries no
packets, so it would be a send target that cannot send; it is recorded
in the reporting inventory instead, where the child registers its
display identity and its parent link (`add_spawned`,
`leviculum-std/src/interfaces/tcp.rs:415`; `listener_id`,
`leviculum-std/src/interfaces/tcp.rs:423`). The split between the
routing map and the reporting inventory is the point of that module
(`interface_names`, `leviculum-std/src/interfaces/inventory.rs:7`).

Teardown runs through the ordinary disconnect path: the event loop
notices the channel closed, calls `handle_interface_down`
(`leviculum-std/src/driver/mod.rs:4017`) to cull the routing entries,
and the child's byte counters are folded into its parent's departed
totals so the listener's reported traffic does not shrink when a client
leaves (`remove_spawned`, `leviculum-std/src/driver/mod.rs:3600`).

### AutoInterface, I2P, shared instance: the same shape

- Every discovered AutoInterface peer becomes a separate handle
  (`spawn_auto_interface`,
  `leviculum-std/src/interfaces/auto_interface/orchestrator.rs:118`;
  the per-peer handle at
  `InterfaceHandle`,
  `leviculum-std/src/interfaces/auto_interface/orchestrator.rs:642`).
- Every accepted I2P stream becomes a handle
  (`new_interface_tx`, `leviculum-std/src/interfaces/i2p/mod.rs:420`).
- Every accepted shared-instance IPC client becomes a handle
  (`add_spawned`, `leviculum-std/src/interfaces/local.rs:289`).

Four multi-peer carriers, one model: a child interface per peer.

### BLE: one interface, many links, a peer hint

The fifth is different. One configured `BLEInterface` section is one
Reticulum interface and one broadcast domain (`BLEInterface`,
`leviculum-std/src/interfaces/ble/mod.rs:5`). Every outbound packet
goes through one planner that decides which links get a copy
(`plan_tx_to`, `leviculum-std/src/interfaces/ble/links.rs:534`, driven
from `send_packet`,
`leviculum-std/src/interfaces/ble/mod.rs:788`). Since Codeberg #376 the
core supplies the addressee: a path entry carries the identity it was
learned from (`via_peer`, `leviculum-core/src/storage_types.rs:48`) and
that identity rides out with the packet. With a hint the planner picks
one link; without one (an announce, a path request) it floods, which is
what a broadcast domain owes its peers.

The firmware runs the same shape with fixed ids: serial 0, LoRa 1, BLE
2, set once at startup (`set_interface_name`,
`leviculum-nrf/src/bin/t114.rs:200`) and hardcoded in the interface
itself (`BleInterface`, `leviculum-nrf/src/ble/mod.rs:537`), with the
announce gate naming the same constant (`BLE_IFACE`,
`leviculum-nrf/src/announce.rs:58`). The fan-out is a task that maps
the hint onto a per-link queue (`tx_fanout_task`,
`leviculum-nrf/src/ble/mod.rs:404`; `LINK_OUT`,
`leviculum-nrf/src/ble/mod.rs:368`).

**The receive side is already peer-aware on both stacks.** The board
reports which peer a packet came from and when a peer appears or
disappears (`handle_packet_from_peer`,
`leviculum-nrf/src/bin/t114.rs:850`;
`handle_interface_peer_lost`, `leviculum-nrf/src/bin/t114.rs:877`;
`handle_interface_peer_up`, `leviculum-nrf/src/bin/t114.rs:889`; the
same three in `handle_packet_from_peer`,
`leviculum-nrf/src/bin/rak4631.rs:844`), the core stamps the peer onto
the path entry it installs, and a peer loss culls exactly the paths
through it (`drop_paths_via_peer`,
`leviculum-core/src/transport.rs:3893`). So the identity-shaped
addressing already exists end to end; the only open question is whether
the *send* side spends an interface object on it.

### How far the two stacks already drift under one model

Both stacks run the peer-hint model for BLE today, so what follows is
not the cost of two models — it is the baseline drift between two
implementations of *one* model, which is the floor any split model
would build on top of. lnsd's central task always reports `CentralGone`
when it ends,
including when the dial never connected at all (`CentralGone`,
`leviculum-std/src/interfaces/ble/bluez.rs:283`), and the orchestrator
restarts the strict scan phase on that event (`CentralGone`,
`leviculum-std/src/interfaces/ble/mod.rs:618`, into `note_reset`,
`leviculum-std/src/interfaces/ble/links.rs:798`). The firmware restarts
its strict phase only at a real connection event or teardown
(`note_strict_reset`, `leviculum-nrf/src/ble/columba.rs:977`); a dial
that timed out records at most a dead end and leaves the clock running
(`note_dead_end`, `leviculum-nrf/src/ble/columba.rs:1381`).

Same protocol, same shared constant, different behaviour after a failed
dial: lnsd owes another full 30 s strict bound, the board does not.
Neither is obviously wrong. The point is that nobody decided it — it
fell out of the two stacks having different event vocabularies
(`CentralGone` fires for a dial that never connected; `conn_link_down`
cannot). That happens under one shared model. Option B below would give
the two stacks different *structures* as well, and the drift rate is
what it would multiply.

## The reference, as a source of ideas

Python-RNS spawns a child interface per connection in five places:
`spawned_interfaces` (`TCPInterface.py:632`),
`spawned_interfaces` (`AutoInterface.py:590`),
`spawned_interfaces` (`I2PInterface.py:998`),
`spawned_interfaces` (`BackboneInterface.py:129`) and
`spawned_interfaces` (`WeaveInterface.py:990`). The child is appended
to `RNS.Transport.interfaces` and is from then on an ordinary send
target.

What that buys:

- The transport addresses an interface. There is no hint, no second
  addressing concept, no interface that means different things
  depending on an extra argument.
- Per-peer statistics fall out: each child has its own `rxb`/`txb`, and
  `rnstatus` shows a row per peer.
- Teardown is one code path — detach the child, and everything keyed on
  its id goes with it.

What it costs: an interface object, its state, and its registration per
connection. Python has no BLE interface at all, so on the carrier this
page is actually about, the reference offers no precedent.

The nearest thing that does is the `ble-reticulum` package Columba
uses, which is our wire counterpart. It spawns one child per peer
(`BLEPeerInterface`,
`ble-reticulum/src/ble_reticulum/BLEInterface.py:2380`; at commit
`07d9413`, 2026-01-18, in the sibling checkout). Two things about that
child are worth more than the precedent itself:

1. **The child is a shim, not an interface.** Its `process_outgoing`
   (`ble-reticulum/src/ble_reticulum/BLEInterface.py:2437`) fetches the
   fragmenter from its *parent*, fragments, and hands each fragment
   back to the parent's shared driver keyed by address
   (`ble-reticulum/src/ble_reticulum/BLEInterface.py:2474`). Every
   piece of per-medium machinery stays on the parent. The child holds
   an address and two counters.
2. **On the peripheral side the addressing is not real.** The GATT
   server's `send_notification`
   (`ble-reticulum/src/ble_reticulum/BLEGATTServer.py:537`) takes a
   `central_address`, and then writes the value to the one TX
   characteristic (`set_value`,
   `ble-reticulum/src/ble_reticulum/BLEGATTServer.py:575`), which
   notifies **every subscribed central**. The address argument only
   selects which counter to increment.

So the reference implementation of a per-peer interface, on this exact
carrier, does not actually address one peer where the carrier cannot.
That is not a criticism of it — the Columba wire spec has one notify
characteristic, and no software layer can conjure a second one. It is
the reason "the reference spawns children, so we should" is not an
argument here. Our own planner is explicit about the same limit
(`plan_tx_to`, `leviculum-std/src/interfaces/ble/links.rs:534`).

The firmware is the exception, and it cuts the other way: the
SoftDevice's notification takes a connection handle, so a board *can*
address one peripheral-role link. lnsd, on BlueZ, cannot.

## The numbers

### Per-interface state in `Transport` and `NodeCore`

`NodeCore` holds no interface-keyed collection of its own; every
per-interface field lives in `Transport`, and there are 24 of them
(`interface_announce_caps`, `leviculum-core/src/transport.rs:1761`
through `own_tunnel_ids`,
`leviculum-core/src/transport.rs:2001` — the `BTreeMap<usize, _>` and
`BTreeSet<usize>` fields in that block).

**Method, and why not `size_of`.** Summing `size_of` over those 24
value types would be the wrong number by a wide margin in both
directions: a `BTreeMap` allocates in nodes of up to 11 entries, so the
first interface pays for a whole node in every map and the next ten pay
nothing, and several of the values are themselves growable
(`interface_held_announces`, `leviculum-core/src/transport.rs:1905`, is
a map of maps). What the 96 KiB firmware pool actually sees is
allocator traffic, so that is what was measured: a counting
`GlobalAlloc` around `System` — the harness already in the tree as
`CountingAlloc` (`leviculum-core/tests/heap_leak.rs:56`) — reporting
net live bytes (allocated minus freed), sampled around building one
`Transport` with *N* interfaces registered through the setter sequence
the firmware bins run, then driving 40 rounds of announce RX across all
of them.

Positive control in every run: the transport must hold 3 of 3 peer
paths at the end, or the measurement is discarded as vacuous. That
control earned itself immediately — the first version of this harness
reported a flat 0 B for every *N*, because it drove `NodeCore` with
`NoStorage` and no announce was ever accepted.

The harness itself is deliberately **not** committed. It would have to
live in `leviculum-core/tests/`, and `fast` runs `cargo test --workspace
--lib`, so nothing would ever execute it — a test that runs nowhere is
the exact Guarantee-B failure
[Checks that are actually checks](checks-and-citations.md) is about. It
is ~180 lines and the recipe above is enough to rebuild it: the
allocator from `heap_leak.rs`, `Transport::new` with `enable_transport:
true` and a long `path_expiry_secs`, the seven setters, and
`clear_packet_hashes` per round so the dedup cache does not mask the
per-interface growth.

Run on `i686-unknown-linux-musl`, not the host default: the board is a
32-bit-pointer target and every one of those maps is pointer-heavy, so
x86-64 overstates it. How much depends on what is being counted — a
third on the pointer-dominated first registration (1 363 B vs 891 B),
about 5 % on the warm 3 → 4 step (1 411 B vs 1 347 B), 10 % on an
11-interface transport (22 228 B vs 20 136 B). The 32-bit column is the
one quoted below.

| Step | Live-heap delta (i686) |
|---|---|
| registration only, 1st interface into an empty node | 891 B |
| registration only, each further interface | 3 B (the name `String`) |
| 3 → 4 interfaces, warm | 1 347 B |
| 4 → 5, 5 → 6, 6 → 7, warm | 579 B each |

Registration is nearly free; the cost appears when the interface
carries traffic and the lazily-created maps get their entry. **Take
1 347 B for the first extra interface and ~600 B for each one after.**
Reproduced identically across two runs, with the one exception of a
single 6 → 7 step where a node split landed differently (939 B) —
which is the `BTreeMap` node granularity showing, not noise in the
method.

### What the firmware can afford

Three measured budgets, all from the T114 on the rig, all post-#372:

| Budget | Measured | Headroom |
|---|---|---|
| Heap, 96 KiB pool (`HEAP_SIZE`, `leviculum-nrf/src/lib.rs:220`) | worst watermark 65 044 B of 98 304 (`rig-run/proof-372-t114.log`, 2026-09-08); typical 56 000-57 000 | 33 260 B at the worst point |
| Stack, flip-link region below `.data` | `min_free=72 280` of a 104 464 B region, `peak_used=32 184` (`rig-run/proof-dup-t114.log`, 2026-09-10) | ~70 KiB never touched |
| SoftDevice RAM ceiling | 928 B of margin (`leviculum-nrf/memory.x:88`) | **not the relevant budget, see below** |

Three BLE children cost `1 347 + 2 × 579 = 2 505 B` of heap, 4 041 B if
every step happens to split a node. Against 33 260 B free at the worst
watermark ever observed that is 7-12 %, and against the ~42 000 B free
at the typical watermark it is 6-10 %. **The heap affords it.**

The 928 B SoftDevice margin does *not* bound this, and it is worth
being explicit because the number is small enough to look alarming.
That margin sizes the SoftDevice's own RAM requirement, which scales
with `conn_count`: 15 272 B at two connections, 23 968 B at four
(`leviculum-nrf/memory.x:88`), and #372 paid for that by moving the app
RAM floor up 8 576 B. A Reticulum interface object is application heap;
it does not appear in `sd_ble_enable`'s requirement at all. Spawning
three children over the same four BLE connections costs the SoftDevice
nothing. A fifth BLE *connection* would cost about another 4 300 B (the
measured two-to-four slope) and blow the 928 B margin — but that is
equally true today with one interface, and the boot assert catches it
either way.

Stack is likewise not per-interface: the send loop iterates, it does not
recurse. What does scale with the interface count is the broadcast
fan-out — an announce emits one action per entry in the routing map
(`interface_names`, `leviculum-core/src/transport.rs:9471`), each
carrying a cloned packet. With three BLE children an announce would
allocate three ~500 B action buffers where today it allocates one that
`tx_fanout_task` clones per link (`leviculum-nrf/src/ble/mod.rs:404`).
Same peak, moved one layer up.

### Which machinery is per medium, and which is per link

This is what decides whether a child can stand on its own or needs a
parent to lean on. Measured against the tree, not assumed:

| Machinery | Per | Where it belongs |
|---|---|---|
| Airtime credit bucket (`AirtimeCredit`, `leviculum-std/src/interfaces/airtime.rs:23`) | **medium** — one radio, one duty cycle | parent |
| Pre-TX jitter / CSMA deference (`compute_jitter_max_ms`, `leviculum-std/src/interfaces/rnode.rs:158`) | **medium** — contention is on the air | parent |
| Announce cap and egress slot (`interface_announce_caps`, `leviculum-core/src/transport.rs:1761`; `interface_next_slot_ms`, `leviculum-core/src/transport.rs:1942`) | **medium** — it rations a shared resource | parent (splitting it per link multiplies the budget by the link count) |
| Max-airtime backchannel (`interface_max_airtime_ms`, `leviculum-core/src/transport.rs:1950`) | **medium** | parent |
| Advertising and scanning (`reconcile_advertising`, `leviculum-std/src/interfaces/ble/mod.rs:734`; `ScanScheduler`, `leviculum-std/src/interfaces/ble/links.rs:754`) | **medium** — one adapter | parent |
| IFAC | **medium** — it is a property of the configured section | parent |
| BLE inter-packet gap (`LinkPacer`, `leviculum-std/src/interfaces/ble/links.rs:852`) | **link**, except on the shared notify pipe where one pacer serves every subscriber (`leviculum-std/src/interfaces/ble/mod.rs:301`) | child, mostly |
| Negotiated MTU and fragmentation state | **link** | child |
| Keepalive and expiry timers | **link** | child |
| Byte counters | **link** | child |

Six of the ten rows are per medium, and the four that are not are the
small ones. That is the finding: on a shared-carrier
medium almost everything that makes an interface an interface is per
medium. A child would own an MTU, a pacer, two timers and two counters,
and would have to reach the parent for everything else — which is
precisely the shape `ble-reticulum`'s child ended up in.

## The options

**A — children everywhere.** BLE spawns an interface per link on both
stacks, matching TCP, AutoInterface, I2P and the shared instance.

**B — children on lnsd, peer hint on the board.** The daemon can
afford interface objects; the firmware keeps three compile-time ids.

**C — peer hint everywhere.** One interface per medium; the transport
passes "for this peer"; the interface maps peer to link. This is
what Codeberg #365 and #376 built, and what runs today.

**D — peer hint everywhere, per-peer rows in the reporting
inventory.** C, plus the one thing A gives away for free: the
inventory already models a parent with spawned children and merges a
departed child's bytes into its parent (`add_spawned`,
`leviculum-std/src/interfaces/inventory.rs:184`; `remove_spawned`,
`leviculum-std/src/interfaces/inventory.rs:190`), and it is
driver-owned and deliberately outside the routing map. A BLE peer
appearing and disappearing already crosses the driver boundary as a
peer event, so it can create and retire an inventory row without ever
becoming a send target.

| | A | B | C | D |
|---|---|---|---|---|
| Correct addressing on a central-role link | yes | yes | yes | yes |
| Correct addressing on a BlueZ peripheral link | **no** — one notify characteristic, every subscriber gets it | no | no, and says so | no, and says so |
| Correct addressing on a SoftDevice peripheral link | yes | yes | yes (per-slot queues) | yes |
| Firmware heap, 3 children | +2.5 to 4.0 KiB | 0 | 0 | 0 |
| Per-medium policies | must be hoisted to a parent object or duplicated per link | hoisted on one stack only | untouched | untouched |
| Per-peer statistics | free | on lnsd only | absent today | yes, in the inventory |
| Adding a new multi-peer medium | write a parent + a child + the hoisting | pick a stack, then both | implement `plan_tx_to` | implement `plan_tx_to`, emit peer events |
| Model count for one protocol | 1 | **2** | 1 | 1 |

**A peer reachable on two links at once.** Under A there are two child
interfaces and therefore two path entries with different interface
indices; the transport picks by hop count and the loser is a live
standby, and losing one link culls only its own paths. That is the
cleanest behaviour of the four, and it is a real scenario: a rotated-
address reconnect holds two links to one identity for a moment. Under
C and D there is one interface and one path entry; the planner takes
the first link it finds for that identity
(`plan_tx_to`, `leviculum-std/src/interfaces/ble/links.rs:534`), and a
peer loss is reported only when the *last* link for that identity dies
(`knows_identity`, `leviculum-std/src/interfaces/ble/links.rs:323`).
The observable difference is which of two equally good links carries
the next packet — the transport cannot express a preference it has no
information to form. Under B, whichever of the two the stack in
question runs.

## The recommendation

**D: keep the peer hint on both stacks, and recover per-peer visibility
in the reporting inventory rather than in the routing map.** The
deciding argument is not the RAM — 2.5 KiB of a 33 KiB worst-case
margin is affordable, so option A is not blocked by the firmware and we
should stop saying it is. It is that a child interface on a shared
carrier is not an interface: six of the ten mechanisms above are
per medium, so every child would delegate straight back to a parent,
and the reference implementation of exactly this idea on exactly this
carrier ended up as a shim holding an address and two counters, with a
peripheral-side `central_address` that only picks a counter. We would
pay an object, a registration, a teardown path and a second addressing
concept to buy an addressing capability the carrier does not have. The
hint costs one `Option<[u8; 16]>` on an action we already emit, works
identically on both stacks, and is honest about the peripheral-side
limit instead of papering over it. What A genuinely buys — a row per
peer in `rnstatus` — is a reporting concern, and the inventory is
already the place where reporting rows live without being send targets.
Option B is rejected outright. The two stacks already drift under one
shared model — the failed-dial scan phase above is this month's
example — and giving them different structures as well buys nothing
the numbers ask for: the firmware is not the constrained party here,
which was B's whole premise.

### What would change this, and what to measure again

The recommendation is a reading of today's mechanisms, not a
permanent verdict. Three triggers, each with the measurement that
settles it:

1. **A carrier arrives where per-link addressing is real on both
   roles and per-link policy is genuine** — per-link airtime, per-link
   congestion control, per-link IFAC. Then a child owns something and
   A wins on its merits. Measure: how many of the ten rows above
   move from "medium" to "link" for that carrier.
2. **The transport gains a reason to prefer one of two links to the
   same peer** — a per-link quality or cost signal. Today it has none,
   which is why C's "first link found" is not a shortcoming. Measure:
   whether a per-link metric changes the chosen route on the rig at
   all.
3. **`rnstatus` parity against `rnsd` needs per-peer BLE rows.**
   That is D's second half, and it is an inventory change, not an
   architecture change. Measure: the `interface_stats` row set from
   both daemons on the same topology, which the drop-in property makes
   a single-driver comparison.

What would *not* change it is a future firmware with more RAM. The
argument above is about where the mechanisms live, and that is the same
on a board with 96 KiB of heap and on a server with 96 GB.

### One correction this page owes

[Bluetooth interfaces](bluetooth-interfaces.md) records an earlier
"Stage B" plan to model `ble-reticulum` as "one ordinary byte only
interface per connected peer (the Columba `BLEPeerInterface` shape)".
That is option A, written before #365 and #376 built the peer hint and
before anyone read what the Columba child actually does on the
peripheral side. This page supersedes that paragraph; the surrounding
argument there — that the broadcast plane and the link plane are two
interfaces and not one clever hybrid — is unaffected and still holds.
