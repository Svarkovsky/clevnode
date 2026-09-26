# Structured event logs

Test-harness scaffolding (Codeberg #39 piece 1, Stage 6) for capturing
mesh-protocol events as parseable lines so multi-node failures can be
diagnosed from a single merged log instead of N hand-correlated
process traces.

## Format

Each emitted event renders to a single line:

```text
EVENT_NAME node=<n> key1=val1 key2=val2 ... t=<rel-ms>
```

Rules:

- `EVENT_NAME` first.  Comes from the literal string passed as the
  `event` field in a `tracing::debug!` call.
- `node=` second.  Value comes from `LEVICULUM_EVENT_NODE`
  environment variable; defaults to `local`.
- All other keys appear alphabetically sorted between `node=` and
  `t=`.
- `t=` last.  Millisecond offset from layer registration time.

Records that don't carry an `event = "..."` field are silently
dropped, so the legacy printf-style `tracing::debug!("[FOO] ...")`
sites stay valid alongside the converted ones.

## Per-packet journey contract

The packet-level events `PKT_TX`, `PKT_RX`, `PKT_FORWARD`, `PKT_DROP`
and `DEDUP_DROP` form the journey contract an external collector uses
to stitch one packet's path across nodes:

- They are emitted on the dedicated tracing target
  `leviculum_core::pkt` (DEBUG), so a collector can enable exactly this
  stream via `RUST_LOG=leviculum_core::pkt=debug` without the rest of
  the transport noise.  The event-log layer sees every record
  regardless of target.
- Each carries `ph`, the first 16 hex chars of the dedup packet hash
  (SHA-256 over the hashable part, which strips `hops` and
  `transport_id`).  `ph` is therefore stable across hops and across
  Type1/Type2 header conversion: the same value appears in the
  sender's `PKT_TX`, every relay's `PKT_RX`/`PKT_FORWARD` and the
  receiver's `PKT_RX`, or in the `PKT_DROP`/`DEDUP_DROP` where the
  packet died.
- `PKT_DROP` renders its `reason` as the kebab-case `DropReason`
  (`no-path`, `plain-group-multihop`, `forward-max-hops`, ...).
  `unknown-context` is the one reason that says nothing about the
  packet's validity: it means the packet was addressed to US and
  carries a context byte this build assigns no meaning to, so nothing
  above transport could interpret it.  The same packet addressed to
  someone else is relayed normally and never reaches this counter —
  the context byte is semantic, not routing information.  A rising
  `unknown-context` on a node that is also an endpoint means a peer
  speaks a dialect (newer RNS, third implementation) we do not.
- `no-such-interface` is the one `PKT_DROP` on the OUTBOUND path, and
  the one with a different field set: the action was routed to an
  interface the driver's dispatch slice does not contain, so it never
  became a received packet and carries `iface_out` and `len` instead of
  `dst`, `type` and `iface_in`.  Non-zero means the driver and the core
  disagree about interface numbering — a configuration fault in the
  driver, not a mesh condition, which is why it does not share a counter
  with `no-path`.
- A relay whose outbound path points back out of the arrival interface
  forwards there — same-interface relay on a shared medium is a normal
  hop, not a drop (see
  [Python-RNS Compatibility](concepts/python-rns-compatibility.md#same-interface-relay-on-shared-media)).
  Its `PKT_FORWARD` carries `iface_out` equal to `iface_in`.
- `PKT_TX` on a `Broadcast` action reports `iface=bcast`: the sans-I/O
  core does not know the concrete interface set the driver expands the
  broadcast to; journeys stitch by `ph`.
- `PKT_TX` and `PKT_RX` both carry `hops`, but they are counted at
  different points and the difference is the contract:
  - `PKT_TX hops` is the hop count of the packet **as transmitted** —
    the byte that goes on the wire, read from the packed buffer being
    handed to the driver.
  - `PKT_RX hops` is the hop count **after receipt**, i.e. after the
    receiver's increment (`Transport::incoming_hop_count`, mirroring
    Python `Transport.py:1457`).

  So for one `ph` crossing one radio hop, `rx hops = tx hops + 1`.  A
  collector reads a journey's DIRECTION from exactly that relation: the
  node observing the packet at the lower hop count transmitted it, the
  node at one more heard that transmission.  Without `hops` on
  `PKT_TX`, a node that ORIGINATES a packet (a firmware node, or a
  daemon's own announces, path requests and link proofs) contributes no
  hop count at all and drops out of that relation.

  The relation is deliberately +1 only for a real medium crossing.  Over
  the local-IPC hop — a `LocalClient` interface, or the uplink to a
  shared instance — `incoming_hop_count` undoes its own increment, so
  there `rx hops = tx hops`.  That is Python's behaviour (`Transport.py`
  :1481-1484) and it is correct: the IPC hop is not a network hop.  A
  collector pairing on `+1` therefore ignores IPC hops, which is what it
  should do.
- The hash is never computed twice for one packet: emission sites
  reuse the dedup/cache hash where it exists and otherwise hash only
  while the `leviculum_core::pkt` target is enabled.  With the target
  disabled the whole contract is zero-cost.
- Deliberate exclusions: the high-volume overheard drop
  (`overheard-transport-id`) and IFAC drops stay counter-only
  (`PKT_DROP_SUMMARY`); announce-pipeline drops (replay, rate-limit,
  ingress-burst, over-max-hops, blackhole) are covered by the
  announce event family and the summary counters.

## Architecture

**All test threads, including tokio multi-thread workers, route
through the same global subscriber registered once via Layer
composition.**

Specifically: `tracing_setup::init_tracing_with_event_log()` builds a
`Registry::default().with(fmt_layer).with(event_log_layer)` chain
and installs it via `set_global_default` once per process (Once-
guarded).  Every thread, every spawned future, every tokio worker
inherits this global subscriber.  This is the load-bearing
architectural choice that lets a `#[tokio::test(multi_thread)]` mvr
see events emitted from worker threads.

Per-test buffer isolation is built on top: `init_event_log()`
returns an `EventLogHandle` whose `Arc<Mutex<Vec<String>>>` buffer
is registered in the layer's active-handles list.  The layer's
`on_event` iterates the active list and pushes the formatted line
to every active buffer.  When the test's handle drops, it removes
itself from the list.

Concurrency consequence: every active buffer receives every event
the layer sees, regardless of which test emitted it.  Tests that
assert on buffer contents must filter by event name to avoid
cross-test pollution.  Use disjoint event names per test
(`EV_BASIC`, `EV_VIOLATION`, …); mvr tests already enforce
`--test-threads=1` so this only affects unit tests.

## How to wire a test

Inside any test, before the test body runs:

```rust
let _evlog = leviculum_std::test_support::event_log::init_event_log();
```

The handle is RAII: when the binding goes out of scope it removes
itself from the layer's active-handles list.  If the test thread
is panicking at drop-time, the buffer dumps to stderr with a
`=== EVENT LOG DUMP …` banner that `cargo test` surfaces in the
failure listing.

Use `init_event_log_to_file(path)` instead of `init_event_log()`
when the test wants to assert on the dumped content directly
(`std::fs::read_to_string(path)`).

To make the test fail loud on undocumented schema gaps, end the
test body with:

```rust
leviculum_std::assert_no_schema_violations!(_evlog);
```

It panics if any `EVENT_SCHEMA_VIOLATION` line appears in the
buffer.

## How to add an event

Two steps, both in the same commit:

1. **Convert the call site.**  Replace the printf-style
   `tracing::debug!` with structured fields:

   ```rust
   tracing::debug!(
       event = "FOO",
       iface = %iface_name,
       dst   = %HexShort(&dst_hash),
       hops  = packet.hops,
       len   = bytes.len(),
   );
   ```

   `%` for Display, `?` for Debug.  Values must be ASCII without
   whitespace, `=`, or non-printable characters — otherwise the
   field-value validator fires (see below).  For Rust keywords
   like `type`, use the raw identifier `r#type`.

2. **Add a catalogue entry** in
   `leviculum-std/src/test_support/event_log.rs`'s `EVENT_CATALOG`:

   ```rust
   EventSchema {
       name: "FOO",
       required_keys: &["iface", "dst", "hops", "len"],
   },
   ```

   `required_keys` should list every field the call site sets.
   The subscriber checks that every catalogued event's emission
   includes all required keys; missing keys produce a
   `EVENT_SCHEMA_VIOLATION` line in the dumped buffer alongside
   the original event.

Catalogue entries without a live emitting site are explicitly
discouraged: the runtime-validation layer can't detect them, so
they silently rot.  Only add entries you have a corresponding
emit for.

## Firmware-side events

The nRF firmware emits the same line grammar, but not through this
machinery.  `leviculum-nrf` is `no_std` and cannot depend on
`leviculum-std`, so there is no `tracing` subscriber, no `node=`
field, and no runtime schema validation; the line goes into the
debug-CDC ring buffer behind the module prefix that the rest of the
firmware log uses:

```text
[BLE ] BLE_TX_DROP kind=packet len=312 frag=1 of=2 sent=1 reason=stalled code=0 waits=0 dropped=3 t=48210
```

The prefix does not disturb the grammar — the event name is still
one whitespace-delimited token and `grep BLE_TX_DROP` over a
captured debug-port log still works — but the event deliberately
does **not** appear in `EVENT_CATALOG`.  A catalogue entry the
subscriber can never see emitted is exactly the silent rot the rule
above forbids.  Firmware events are documented here and at their
call site instead.

### Who writes `t=` on a firmware line

Nobody at the call site.  Since #344 the firmware's log formatter
(`leviculum-nrf/src/log.rs`, shape in `leviculum-log-line`) appends
` t=<uptime-ms>` to **every** line it emits — event lines, plain
`[LORA]`/`[INFO]` lines, boot banners, `tracing` records alike.  A
call site that also writes its own `t=` renders the field twice;
`BLE_TX_DROP` did, and stopped.

The stamp is taken when the line is *formatted*, not when it is
drained: the ring is emptied in 64-byte USB packets on a 100 ms
loop, so host arrival times measure that loop and nothing else.

**A line may still legitimately carry two `t=` fields.** The boot
replay of the persistent tail wraps a line from the previous boot,
its stamp included, inside a line of this boot:

```text
[INFO!] [PERSISTENT_LOG] [LORA] RX 41 bytes t=91422 t=137
```

Both are true.  A line's own stamp is always its **last** `t=` —
the same rule `merge_event_logs` already applies, so the two agree.

**Epochs do not.**  A firmware `t=` is milliseconds of board uptime;
a host-side `t=` is milliseconds since that subscriber's init.
Merging a debug-port capture into a host event log with
`merge_event_logs` therefore orders each stream correctly within
itself and says nothing across the two.

Current firmware events:

| Event | Emitted by | Meaning |
|-------|-----------|---------|
| `BLE_TX_DROP` |  `leviculum-nrf/src/ble/notify.rs` | A BLE packet or keepalive was abandoned part-way through its fragments.  `frag=` is the fragment that failed, `sent=` how many did go out, `reason=` one of `stalled` (no HVN-TX-COMPLETE within the bound), `disconnected`, `budget`, `sd_error` (with the raw code in `code=`), `internal`.  `conn=` is the SoftDevice connection handle the TX targeted — the fan-out sends one copy per live link, and without this key the desk log of 2026-09-08 could not say WHICH link ate an `sd_error` (#365).  `dropped=` is the cumulative counter, so one line states both the incident and the running total. |
| `BLE_TX_PKT` | `leviculum-nrf/src/ble/notify.rs` (notify pump) and `leviculum-nrf/src/ble/columba.rs` (central write loop); line rendered by `leviculum-ble-tx`'s `TxPktLine`, verbatim-pinned by its host test | **One line per multi-fragment packet handed to a link, whatever became of it (#373).**  `conn=` the SoftDevice connection handle, `len=` the whole packet, `frags=` how many fragments it split into, `sent=` how many the stack accepted.  A healthy hand-over reads `sent==frags`; `sent<frags` is a loss on this node and is always accompanied by a `BLE_TX_DROP` naming the reason.  The desk log that motivated it had two of five relayed two-fragment packets vanish on the BLE hop with **no** line anywhere — `BLE_TX_DROP` only fires on an abandoned packet, so a fully-accepted packet that still never arrived left nothing to grep.  Single-fragment packets and keepalives stay unlogged (they are the bulk of the traffic and the failure mode needs ≥2 fragments). |
| `BLE_RX_ABANDON` | `leviculum-nrf/src/ble/columba.rs` (both GATT roles) | A link's reassembly was discarded before completion (#373): a new START over an unfinished head, a `total` contradicting the reassembly in progress, or the hard reset after a garbage frame.  Each is one or more whole Reticulum packets this receiver lost — previously a silent state reset.  `slot=` the link's drain slot, `lost=` what this frame cost, `total=` the link's running count (`BleDefragmenter::abandoned_count`).  lnsd's Columba interface emits the same event name identity-keyed (see the host-side section below), so a merged bench timeline carries both receivers. |
| `BLE_TX_RESYNC` | `leviculum-nrf/src/ble/notify.rs` | The connection was dropped deliberately after a torn fragment stream, because the wire protocol has no abort marker and a reconnect is the only in-band reset of the peer's reassembler (#255). |
| `BLE_TX_GAP` | `leviculum-nrf/src/ble/columba.rs` (both pumps) | The per-link inter-packet gap deferred a packet (#376): `conn=` the connection handle, `waited_ms=` how long the packet's first fragment was held back after the previous packet's last.  The gap defaults to 100 ms (`leviculum-ble-tx`'s `DEFAULT_TX_GAP_MS`, the measured desk value); `lnflash --set-ble-tx-gap` overrides it for measurement, `0` disables.  On a paced link with real traffic this line is the expected signature; its absence under back-to-back traffic means the knob was set to 0.  lnsd emits the same event name on its notify pipe (`link=notify`) and central links (`addr=`). |
| `BLE_TX_HELD` | `leviculum-nrf/src/ble/columba.rs` (peripheral pump) | The drain held a packet because the peer cannot receive yet (#376): `conn=`, `reason=not-subscribed`.  Once per connection, however many packets wait.  Background: a notify before the central writes the TX CCCD fails with `sd_error code=13313` (`BLE_ERROR_GATTS_SYS_ATTR_MISSING`) and the packet dies — the field T114 lost the first packet of a fresh connection this way at 11:31:18 on 2026-09-09.  Held packets wait in the link's queue and drain after the subscription (or the handshake, whichever is later); policy host-tested in `leviculum-ble-tx`'s `hold` module. |
| `[ANNOUNCE]` | `leviculum-nrf/src/announce.rs`, and the `TYPE_ANNOUNCE` arm of each binary | **Frozen shape — the desk recipe reads this.** When the board announced its `lxmf.delivery` destination, and why (#376). `[ANNOUNCE] sent dst=<hex8> reason=<r>` with `reason=peer-up peer=<hex8>` (a BLE peer finished its identity handshake; the announce goes on that peer's link alone and appears as `BLE_TX_ROUTE` beside it, never as `BLE_TX_FLOOD`), `reason=periodic` (the timer, every 30 minutes, on every interface, so it is a `BLE_TX_FLOOD`) or `reason=host` (`lnflash --announce`). The telemetry path's own pre-report announce is not named here; it is the broadcast that precedes a `[TELEMETRY] send` line. `[ANNOUNCE] withheld reason=no-clock` is the clock gate — an announce stamped from uptime can never replace a path at the receiver, so a board without a plausible wall clock says why it is silent instead of poisoning path tables; written once per change, not once per retry. `reason=rate-limited peer=<hex8>` under the `[BLE ]` prefix is the per-identity 15-minute limit, which is why a phone rotating its BLE address every minute does not buy an announce every minute. `lnsd` emits the same two occasions as the trace events `ANNOUNCE_TX reason=peer-up peer= iface= count=` and `ANNOUNCE_WITHHELD reason= peer= iface=`. |
| `BLE_TX_ROUTE` / `BLE_TX_FLOOD` / `BLE_TX_ROUTE_MISS` | `leviculum-nrf/src/ble/mod.rs` (the fan-out task) | What the core's per-packet delivery hint made of one outbound packet (#376).  Exactly one of the three per packet, so a capture accounts for everything the interface was handed.  `BLE_TX_ROUTE peer=<hex8> conn=<h> slot=<n> len=<n>` — the packet was addressed at one peer and went on that peer's link alone.  The core takes the peer from the path it routed over (`via_peer`, Codeberg #365), or, for a proof, from the arrival it is answering: the ingress peer travels with the deferred `ProofRequested` event, so a probe's proof leaves as `BLE_TX_ROUTE` and not as a flood.  This is what stops a report for the phone from also travelling to the board beside it, which used to forward it back and give the phone two copies.  `BLE_TX_FLOOD links=<n> len=<n>` — no hint, so a broadcast: announces and path requests still reach every live link.  `BLE_TX_ROUTE_MISS peer=<hex8> len=<n>` — the addressed peer holds no live link here and the packet is dropped, NOT flooded (flooding would spend the other links' airtime on a packet they cannot deliver and rebuild the relayed duplicate); its running total is `route_miss=` on `BLE_COUNTERS`, and a rising value means the path table outlived a link the #365 cull should have taken.  lnsd emits the same three names with `iface=` and, on `BLE_TX_ROUTE`, `conn=central\|peripheral` instead of a SoftDevice handle. |
| `BLE: RX` | `leviculum-nrf/src/ble/columba.rs` (both GATT roles) | One line per reassembled inbound packet: `BLE: RX <n>B conn=<h> frags=<k>` — `conn=` tells the phone's link from the neighbour board's, `frags=` how the PEER fragmented the packet (`BleDefragmenter::last_completed_fragments`), which is the only place a peer's real fragment size is visible (#376: Columba as peripheral claimed "MTU 20 bytes" while our central negotiated a large ATT MTU).  The binaries' former `BLE RX <n> bytes` line was dropped for it: one reception, one line. |
| `BLE_DRAIN_TABLE_FULL` | `leviculum-nrf/src/ble/columba.rs` | A connection could not claim a per-connection HVN drain slot; `slots=` is the table size.  Expected never: it means more live connections than `ble::MAX_LINKS`. |
| `[MEDIA]` | `leviculum-nrf/src/media.rs` | **Frozen shape — assertions read this.** Which carriers this node meshes over: `lora=on\|off ble=on\|off src=default\|flash`.  The two carrier fields are what the board is **running** (a carrier configured on but not started this boot reads `off` here, which is the honest answer), and `src=` says whether the profile came off the flash page or from the both-on default.  Emitted once per boot after both spawn decisions, then re-emitted with `[FW_BUILD]` every 5 s so a capture attached after the boot window still reads the carriers off the board.  A carrier held down by the profile also emits `carrier=<lora\|ble> state=down reason=profile` under the same prefix at the point its bring-up would have been, and packets dropped because their medium is off emit `MEDIA_TX_DROP iface=<name> packets=<n> bytes=<n> reason=carrier-off` — not one line per packet but on the first drop of a run and at each decade after it (the 1st, 10th, 100th …), because every log line also writes the 2 KiB post-crash tail and a carrier that is off drops one packet per announce; `MEDIA_TX_RESUMED iface=<name> packets=<n> bytes=<n>` closes the run with its exact totals when the carrier takes a packet again (host tests in `leviculum-nrf/media-state`).  See `docs/src/concepts/media-profiles.md`. |
| `[NAME ]` | `leviculum-nrf/src/name.rs` | What this board is called on each of its two display surfaces: `mesh=<name> ble=<name> src=derived\|flash`.  `mesh=` is the display name the LXMF announces carry (what Columba lists), `ble=` the GAP/advertised name a scanner sees, and `src=` whether both come off the flash page or from the names derived from the identity (`LNode-<hex8>` and `LN-<hex8>`, which are different strings, not a truncation of one another).  The two differ when an operator's name is longer than the BLE bound of 11 bytes — visibly, which is the point.  Emitted once per boot beside the `[MEDIA]` banner, then re-emitted with `[FW_BUILD]`, so a capture says under which names the board is visible without an operator having to remember what they set.  Set over the control envelope with `lnflash --set-name` (`docs/src/firmware/usb-control-envelope.md`, NODE_NAME 0x0C). |
| `SD_RAM_FLOOR` | `leviculum-nrf/src/ble/mod.rs` | One line per boot, before `Softdevice::enable`.  `wanted=` is the app RAM base the S140 says this BLE configuration needs, `floor=` is `ORIGIN(RETAINED)` from `memory.x` (the SoftDevice's ceiling — the retained cross-boot records and then the flip-link stack sit above it), `margin=` their signed difference.  `fits=0` never appears — the boot panics instead. |
| `ADV` | `leviculum-nrf/src/ble/columba.rs` | One line per boot when the advertising payloads are built.  `adv_bytes=`/`scan_bytes=` are the built PDU sizes against `cap=31`, `peripheral_only=` is the v0.3.0 capability bit, and `periph_links=` the number of incoming link slots the payloads serve (#372).  Emitted on the critical log path (like `SD_RAM_FLOOR`): it fires before the host's DTR-assert opens the runtime drain, and the gated path would silently drop it. |
| `BLE_SCAN_DECISION` | `leviculum-nrf/src/ble/columba.rs` | The scanner saw a Columba peer and applied the v2.2 address sort with the v0.3.0 capability override.  `addr=` is the peer's current address as 12 hex digits, `caps_record=` whether a readable capability record was present (`caps=` is meaningless when 0), `rule=` the decision rule that fired, `initiate=` whether this side dials.  Emitted once per (address, decision) change, not per PDU. |
| `BLE_CENTRAL_*`, `BLE_LINK_SELF`, `BLE_LINK_DUP` | `leviculum-nrf/src/ble/columba.rs` | Central-role connection lifecycle: `BLE_CENTRAL_ADDR`/`CONNECT`/`FAIL`/`UP`/`DOWN`, plus the two admission decisions.  `BLE_LINK_SELF addr=<a> action=disconnect` — the peer presented our own identity.  `BLE_LINK_EXPIRE role=peripheral\|central slot=<n> conn=<h> silence_ms=<n>` — the board's link expiry (#382): this link delivered nothing at all, payload AND keepalives, for `LINK_TIMEOUT_MS` (45 s), and the session disconnected it.  It is the board's counterpart of lnsd's `BLE_LINK_DOWN … reason=timeout` and the one mechanism that clears a link nobody dials; `silence_ms` says how far past the bound it ran.  `BLE_LINK_DUP peer=<hex8> addr=<a> action=refuse\|displace origin=incoming\|outgoing old_conn=<h> new_conn=<h> old_silence_ms=<n>` — that identity already holds a live link (#376, #382).  `origin=` is the whole decision, and no clock enters either branch: an `incoming` duplicate (the peer dialled us) always displaces, because a peer that opens a second connection has by its own one-link rule given up on the first; an `outgoing` one (our own dial) is always refused, because our dial is evidence of nothing about the peer.  `old_silence_ms` is how long ago the OLD link last delivered ANY frame, payload or keepalive; it is reported on both branches and consulted on neither — it is the measurement that would show the rule wrong.  A link that has really stopped answering is cleared by `BLE_LINK_EXPIRE` instead, never by a displacement.  A displacement adds `moved=`/`dropped=`, the packets carried over from the old link's queue.  Emitted in BOTH roles, so a phone dialling in and a board dialling out read the same.  A refusal also emits `BLE_DIAL_DEAD_END addr=<a> reason=dup_refused ttl_s=<n>`: the address is backed off, or the scanner re-offers it within seconds.  Running totals are `refused=` and `displaced=` on `BLE_COUNTERS`; beside a Columba phone both climb — `refused=` per fallback dial that finds the phone's rotated address, `displaced=` once per reconnect the phone itself initiates. |
| `[LORA] RX` | `leviculum-nrf/src/lora.rs` | One line per reassembled reception: `RX <n> bytes rssi=<dBm> snr=<dB> flags=0x<hh> dst=<hex8>`.  `flags=` is the packet's Reticulum header flags byte (its low two bits are the packet type) and `dst=` the first 8 hex digits of the destination hash — the two keys `lnflash --summarize` classifies announce, data, path request and proof from.  A packet too short to carry the header its flags claim keeps the bare `RX <n> bytes rssi= snr=` shape.  lnsd's RNode interface emits the same `rssi=`/`snr=` keys on its `LORA_RX iface=… len=…` trace event and its `RX … bytes from radio` line, paired to the data frame the way the reference interface pairs them: the firmware indicates `CMD_STAT_RSSI`/`CMD_STAT_SNR` immediately before each data frame, so the last-seen stat values are that frame's own report (keys absent until the first stat frame). |
| `[TELEMETRY] send` | `leviculum-nrf/src/telemetry.rs` | **Frozen shape — the #365 proofs read this.** One line per telemetry report attempt, emitted at the moment of the routing decision: `send dst=<hex8> via=<iface-name> next_hop=<hex8\|direct> online=<y\|n>`.  "Sent to a live carrier" is this line (`online=y`) followed by the `report target=…` line once the dispatch settles; "not sent" is `report withheld target=<hex8> reason=<r>` instead, where `reason=no-path` means no path entry at all and `reason=iface-offline` means an entry exists but its interface is offline (carrier off, or a peerless BLE domain) — both are followed by a path request on the carriers still online.  lnsd states the same decision for **every** originated packet as the host events `OUTBOUND_ROUTE dst= iface= next_hop= online=` and `OUTBOUND_WITHHELD dst= iface= next_hop= reason=` (schema-validated via `EVENT_CATALOG`, asserted in `leviculum-std/tests/obs_outbound_route_events.rs`). |
| `[TELEMETRY] proof` / `retry` / `gave up` | `leviculum-nrf/src/telemetry.rs` | **Frozen shape — the #373 proofs read these.** The proof wait of a sent report (state machine host-tested in `leviculum-nrf/telemetry-policy`): the transport tracks a receipt per single packet, and a report that gets no proof within the receipt timeout (`leviculum-core/src/transport.rs` `compute_receipt_timeout`) is retransmitted ONCE with the identical payload — same position, same time — then given up.  `[TELEMETRY] proof pkt=<hex8> after=<ms>` on a verified proof (`after=` measured from the first send, whether the first send or the retransmission landed); `[TELEMETRY] retry pkt=<hex8> reason=no-proof` at the moment the retransmission goes out; `[TELEMETRY] gave up pkt=<hex8> after=<ms>` after the second loss, and the next scheduled report carries on.  `pkt=` is always the FIRST send's packet hash, so the two or three lines of one report correlate on one id.  The retry goes through the ordinary send path (an offline path is re-resolved, the interface applies its airtime rules) and moves no cadence: `min_interval_ms` for the next report still counts from the original attempt.  Firmware only — lnsd has no telemetry sender. |
| `BOOT_TRACE` | `leviculum-nrf/src/boot_trace.rs` (shape host-tested in `leviculum-nrf/boot-trace`) | One line per boot, first thing in the boot banner: what the PREVIOUS boot's breadcrumb record in retained RAM (`.retained`, a `memory.x` region the Adafruit bootloader provably never touches — its original `.uninit` home sat under the bootloader's stack and was wiped on every reset) says.  `BOOT_TRACE prev_magic=ok\|absent prev_phase=<milestone> prev_boot=<n> reset_reason=<hex>`.  `prev_phase` is the last boot milestone the previous boot COMPLETED (`enter-main`, `persist-read`, `usb-up`, `lora-task`/`lora-skipped`, `sd-enabled`, `ble-task`, `main-loop`) — a healthy reboot reads `main-loop`; anything earlier says the previous boot HUNG right after that milestone, which is the whole point: a boot that dies before USB enumerates is otherwise invisible (ledger local-pocket-dark-d66209e).  `prev_magic=absent` (with `prev_phase=absent prev_boot=0`) is the honest first-boot-after-power-loss answer, never a fabricated phase; `prev_phase=unknown-0x<byte>` marks a record left by an image with a different phase table.  `reset_reason` is raw `POWER.RESETREAS` at entry to `main` (cleared after the read so each boot reports only its own cause); the decoded per-bit view follows on the `[RESET_REASON]` line, now on both boards. |

### Host-side BLE events

lnsd's Columba BLE interface (`leviculum-std/src/interfaces/ble/`)
emits `BLE_SCAN_DECISION` **with the same fields as the firmware's
line of the same name** — `addr=`, `caps_record=`, `caps=`, `rule=`,
`initiate=` — so a merged rig timeline shows both sides of one
mutual sighting deciding, and the two `rule=` values must be
complementary (one `initiate…`, one `wait…`).  Its link lifecycle is
`BLE_LINK_UP` / `BLE_LINK_DOWN` (with `role=central|peripheral` and
`peer=<hex8>`, the same hex the firmware logs and the `LN-<hex8>`
name carries), the admission decisions are the firmware's
`BLE_LINK_SELF` / `BLE_LINK_DUP` names — lnsd's `BLE_LINK_DUP` carries
`action=refuse|displace`, `origin=incoming|outgoing` and
`old_silence_ms=` exactly as the firmware's does (#382), plus
`old_role=` on a displacement — and a fan-out drop on a congested link
is `BLE_TX_FANOUT_DROP`.  The delivery-hint decision
is the firmware's `BLE_TX_ROUTE` / `BLE_TX_FLOOD` /
`BLE_TX_ROUTE_MISS` (#376), with `conn=central|peripheral` naming the
role of the link rather than a SoftDevice handle; routing at a
peripheral-role peer still reaches the other subscribed centrals,
because BlueZ fans one notification out to every subscriber and the
Columba service has a single notify characteristic.  A reassembly discarded
before completion is `BLE_RX_ABANDON` (#373) with `iface=`,
`peer=<hex8>`, `lost=` (packets this frame cost) and `total=` (the
link's running count) — the firmware's line of the same name is
slot-keyed instead of identity-keyed, everything else matches.
These are host events, so
unlike the firmware's they do appear in `EVENT_CATALOG` and are
schema-validated.

### Reading "was the radio listening at instant X"

`[SX_RX_ARM]` is emitted at the `SetRx` that arms the SX1262, once
per receive window:

```text
[SX_RX_ARM] site=idle timeout_ms=0 dark_ms=3 t=123456
```

- `t=` is the instant the receiver went live.  It is *not* derivable
  from the window's completion line: `[T114_LORA_LOOP] op=rx_*
  duration_ms=` brackets the whole `receive()` call, IRQ setup and
  buffer readout included, so `t - duration_ms` lands before the
  arming, not on it.
- `dark_ms` is the gap back to the previous window's end, computed on
  the board.  The boot arm has no previous window and says
  `dark_ms=first` rather than a digit.
- `site` is which of the loop's six listening windows this is —
  `idle`, `ack`, `csma`, `jitter`, `hold`, `yield` — because their
  timeouts overlap and the length alone does not identify them.

The two together close the span: window *n* was listening from its
own `t=` until `t(n+1) - dark_ms(n+1)`.  Everything outside those
spans is standby, including CAD and TX.  So the last window in a
capture has no closing instant — its successor is what supplies it.

The line is written through `log_fmt`, so a board with nothing
attached to the debug CDC (`RUNTIME_DRAIN_OPEN == false`) never
formats it.

## Validation behaviour

Two violation classes, both non-blocking — the original event
line is never suppressed.

### Schema violation (per-handle)

`EVENT_SCHEMA_VIOLATION event=<NAME> missing=[a,b] caller=file:line t=<ms>`

Emitted when a catalogued event misses required keys at emission.
Each active handle's catalogue lookup chains the production
`EVENT_CATALOG` with the handle's own `extra_schemas`, so
test-only schemas don't pollute the production catalogue.

### Field-value violation (per-event)

`EVENT_FIELD_VIOLATION event=<NAME> field=<key> value_problem=<kind> caller=file:line t=<ms>`

Emitted when a field's stringified value contains ASCII
whitespace, `=`, or non-printable characters.  Such values break
the whitespace-tokenised parser used by Stage-7's
`jl --filter <key>=<value>` filter.  `<kind>` is one of
`whitespace`, `equals`, `non_printable`.

The fix at the call site is to pick a value form that doesn't
need escaping — substitute `_` for spaces, drop `=` from value
strings, etc.  The original event line is still emitted; the
tester sees the violation alongside, treats it as a bug.

## Multi-process workflow

Spawned subprocesses (e.g. an `lnsd` child of an integration
test) emit to a per-process file when given two env vars:

```sh
LEVICULUM_EVENT_LOG=/tmp/leviculum-events-<pid>.log \
LEVICULUM_EVENT_NODE=node-a \
    ./lnsd ...
```

- `LEVICULUM_EVENT_LOG=<path>` — child appends each event line
  (and any field-violations) to `<path>` as it emits.  When
  unset, the subscriber writes only to the in-memory buffer used
  for panic-dump.
- `LEVICULUM_EVENT_NODE=<name>` — supplies the `node=` value.

After the children exit, the parent merges all per-process files:

```rust
use leviculum_std::test_support::event_log::merge_event_logs;
let merged: Vec<String> = merge_event_logs(&[
    PathBuf::from("/tmp/leviculum-events-12345.log"),
    PathBuf::from("/tmp/leviculum-events-12346.log"),
]);
```

`merge_event_logs` reads every input, parses the trailing `t=<n>`
token of each line, and returns the union sorted by `t` (stable
on tie).  Lines without parseable `t=` sort to the end with
their relative order preserved.

Per-process clock note: `t=` values are millisecond offsets from
each subscriber's local init time, not a shared wall clock.
Merged ordering is monotone across the union but doesn't directly
say which real-world event came first across hosts.  For
wall-clock correlation add a per-emission timestamp field to the
catalogue (`ts=<unix-ms>`) and sort on that instead.

### Production-daemon integration

`lnsd` honours both env vars at startup via
`leviculum_std::test_support::event_log::install_global_subscriber()`.
When `LEVICULUM_EVENT_LOG` is unset, the install path is
functionally equivalent to the previous
`tracing_subscriber::fmt().init()` call — no event-log layer is
built, so runtime overhead is whatever the fmt layer would
otherwise impose.

`rnsd` is Python-side (reference/Reticulum); structured event
capture for it is out of scope for the current Rust-side work.

## See also

- Codeberg #39 piece 1 (this document's spec).
- `leviculum-std/src/test_support/event_log.rs` (implementation +
  catalogue).
- `leviculum-std/src/test_support/tracing_setup.rs` (Registry
  composition + Once-guard).
- `leviculum-std/tests/event_log_subscriber.rs` (unit tests).
- `leviculum-std/tests/event_log_multiprocess.rs` (multi-process
  merge integration test).
- Stage 7: `jl` / `jldiff` filter tools that consume this format.
