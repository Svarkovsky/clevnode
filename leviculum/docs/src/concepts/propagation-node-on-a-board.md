# An LXMF propagation node on a board with 1 to 2 MB of flash

Both our boards carry a QSPI NOR flash we have never driven: 1 MB on
the Pocket V2 (IS25LP080D) and 2 MB on the T114 (MX25R1635F). Codeberg
#384 asks the obvious question: should that flash hold an LXMF
propagation node, so the mesh has a store when the recipient is not
reachable? The walk that prompted it had a link built from one phone to
another across two of our nodes and a hill, which is exactly the
topology where a store matters.

This page establishes what the role obliges us to, measures what it
would cost on these two parts, sets out the options, and recommends
one. It is a design document. Nothing here is a status page; what is
open belongs on the tracker.

The framing binds the whole argument. The reference is a source of
ideas, never a blueprint. What binds us is wire and semantic
compatibility: **a Python or Sideband peer must be able to use our node
as a propagation node without knowing it is small.** Everything else is
ours to design, and a board in a pocket is not a server in a basement.

## 1. What the role obliges us to

Established against `reference/LXMF` at 1.1.0.

### The destinations, and the verbs on them

A propagation node owns one inbound SINGLE destination,
`lxmf.propagation`, created from the router identity
(`propagation_destination`, `LXMRouter.py:190`). Two request handlers
hang off it, and they are the entire public protocol:

| Verb | Path | Who calls it | Handler |
|---|---|---|---|
| offer | `/offer` | another propagation node | `offer_request` (`LXMRouter.py:2266`) |
| get | `/get` | a client (Sideband, `lnmsg`, `rnsd`) | `message_get_request` (`LXMRouter.py:1482`) |

The paths are constants on the peer
(`OFFER_REQUEST_PATH`, `LXMPeer.py:14`; `MESSAGE_GET_PATH`,
`LXMPeer.py:15`). A third destination, `lxmf.propagation.control`,
carries the operator verbs `/pn/get/stats`, `/pn/peer/sync` and
`/pn/peer/unpeer` (`STATS_GET_PATH`, `LXMRouter.py:89`;
`SYNC_REQUEST_PATH`, `LXMRouter.py:90`; `UNPEER_REQUEST_PATH`,
`LXMRouter.py:91`) and is behind an allow-list, so it is not part of
what a stranger can drive.

Messages move in three shapes, and only three:

1. **A client uploads one message.** A single link packet carrying
   `[timestamp, [lxmf_data || stamp]]` (`propagation_packet`,
   `LXMRouter.py:2234`). No peering key is needed for a single
   message. The node proves the packet — and it proves it *after*
   storing, not before (`packet.prove`, `LXMRouter.py:2255`).
2. **A peer offers a batch.** `/offer` carries
   `[peering_key, [transient_id, …]]`; the node answers `True` (want
   all), `False` (want none), or the sublist it wants
   (`offer_request`, `LXMRouter.py:2266`). The bodies then follow as
   one Reticulum Resource.
3. **A client drains its mailbox.** `/get` with both fields `None`
   returns the list of transient IDs held for that client's delivery
   destination; a second `/get` with `[wants, haves, limit]` returns
   the bodies and *deletes* everything in `haves`
   (`message_get_request`, `LXMRouter.py:1482`). The client sends the
   purge only after it has taken local delivery
   (`message_get_response`, `LXMRouter.py:1607`).

A transient ID is `SHA-256(lxmf_data)` where `lxmf_data` is
`destination_hash || destination-encrypted payload`. We already
implement the client half of this exchange (`MESSAGE_GET_PATH`,
`leviculum-lxmf/src/propagation.rs:25`).

### What is protocol, and what is that implementation's bookkeeping

Per message the reference keeps seven fields
(`propagation_entries`, `LXMRouter.py:2518`): destination hash,
file path, receive timestamp, size, handled peers, unhandled peers,
stamp value.

Of those, **three are protocol**: the destination hash (it decides who
may `/get` the message), the transient ID (the key of every exchange),
and the bytes themselves. The stamp value is protocol-adjacent — a
peer drops messages whose stamp value is below its requirement
(`sync`, `LXMPeer.py:267`) — but a node that requires nothing needs
only to remember zero. The receive timestamp is local policy: it feeds
expiry and the cull weight (`clean_message_store`, `LXMRouter.py:1144`),
and no peer ever sees it. The file path and the two peer lists are pure
bookkeeping of *that* design.

Per peer, `to_bytes` (`LXMPeer.py:138`) persists twenty-odd fields.
Only four of them are visible on the wire in any form: the peer's
destination hash, its peering key, its announced limits, and its
announced costs. Everything else — link establishment rate, sync
transfer rate, rx/tx byte counters, offered/outgoing/incoming counts,
backoff state — is statistics. **And two of them are the problem:**
`handled_ids` and `unhandled_ids`, a pair of 32-byte-per-message sets
*per peer*, because every message the node accepts is enqueued into
every other peer's unhandled set
(`flush_peer_distribution_queue`, `LXMRouter.py:2472`).

That distinction decides what we may drop. We may drop all of the
statistics and both peer sets. We may not drop the destination hash,
the transient ID, or the bytes.

### What it advertises, and whether a peer believes it

The propagation announce is a seven-element msgpack list
(`get_propagation_node_app_data`, `LXMRouter.py:324`):

| # | Field | Reference default |
|---|---|---|
| 0 | legacy PN support | `False` |
| 1 | node timebase | now |
| 2 | propagation node state | `True` |
| 3 | per-transfer limit, kilobytes | 256 (`PROPAGATION_LIMIT`, `LXMRouter.py:55`) |
| 4 | per-sync limit, kilobytes | 10240 (`SYNC_LIMIT`, `LXMRouter.py:59`) |
| 5 | `[stamp cost, flexibility, peering cost]` | `[16, 3, 18]` (`PROPAGATION_COST`, `LXMRouter.py:54`; `PEERING_COST`, `LXMRouter.py:50`) |
| 6 | metadata dict | name |

**A node can honestly announce a small capacity, and peers respect
it.** Field 3 is enforced by the *offering* peer: a message larger than
our advertised transfer limit is dropped from its queue for us and
marked handled, so it is never retried (`sync`, `LXMPeer.py:267`).
Field 4 is enforced by us: an inbound resource larger than the
advertised sync limit is refused before it transfers
(`propagation_resource_advertised`, `LXMRouter.py:2206`). Field 5 is
read by both, and a client mines its stamp to the cost we name.

Two caveats, and they matter.

- **For a client, field 3 is advisory.** Nothing on the client side
  checks a node's transfer limit before uploading; the only
  enforcement is our refusal of the resource, which the client reports
  as a failed sync rather than as "too big".
- **We would be the first to advertise cheap.** The reference clamps
  its own configured cost up to `PROPAGATION_COST_MIN`
  (`LXMRouter.py:52`, applied at `LXMRouter.py:137`), so a Python node
  never announces below 13. Announcing 0 is wire-legal and semantically
  honoured — a peer's accepted cost is `max(0, our_cost − flexibility)`
  — but it is a policy nobody else in the mesh runs, and it hands away
  the only spam brake the protocol has.

The announce also has a switch: field 2 false makes every Python router
*unpeer* us on the next announce (`LXMFPropagationAnnounceHandler`,
`Handlers.py:35`). That is the clean way to leave the role, and it is
also the reason a node that serves only static peers is invisible to
clients: the reference computes field 2 as "propagation node **and
not** static-only".

### What a peer expects when a node forgets. This is the crux

**There is no verb for it.** Say it plainly, because the design has to
be built around the absence.

The error space is rich and none of it means "I dropped it":
`ERROR_NO_IDENTITY`, `ERROR_NO_ACCESS`, `ERROR_INVALID_KEY`,
`ERROR_INVALID_DATA`, `ERROR_INVALID_STAMP`, `ERROR_THROTTLED`
(`LXMPeer.py:29`), `ERROR_NOT_FOUND` (`LXMPeer.py:30`),
`ERROR_TIMEOUT`. `ERROR_NOT_FOUND` is defined and never returned by
either request handler.

What actually happens when a message is gone:

- On `/get` **list**, it is simply not in the returned list. The client
  cannot tell "never arrived" from "arrived and was dropped".
- On `/get` **fetch**, a wanted ID that is no longer in the store is
  skipped silently and the response is shorter than the request
  (`message_get_request`, `LXMRouter.py:1482`). No error, no gap
  marker.
- On the peer side the same thing happens in reverse: the offering
  peer discovers on its next sync that an ID it had queued is gone
  from its own store and quietly drops it (`sync`, `LXMPeer.py:267`).

And the reference already forgets, routinely and silently: messages
expire after 30 days (`MESSAGE_EXPIRY`, `LXMRouter.py:38`) and, when
the store exceeds its configured limit, entries are culled by a weight
of `age × size` until enough bytes are free (`clean_message_store`,
`LXMRouter.py:1144`). Peers vanish after 14 days unreachable
(`MAX_UNREACHABLE`, `LXMPeer.py:39`).

Two conclusions follow, and they point in opposite directions.

**Forgetting is normal, so a small node forgets faster, not
differently.** There is no promise in the protocol that we would be
breaking. Sideband's own retry behaviour already has to cope with a
node that dropped something.

**But acceptance is proven and retention is not.** The node proves the
upload packet (`packet.prove`, `LXMRouter.py:2255`), so the sender is
told "accepted" and is never told "and then discarded". A store that
accepts more than it can plausibly hold converts a proof of acceptance
into a lie by omission. **So our design must avoid promising**: accept
less rather than accept and drop, and make the advertised limits small
enough that the acceptance is honest.

The one honest back-pressure verb that does exist is
`ERROR_THROTTLED`, and a peer handles it correctly by deferring its
next sync (`LXMPeer.py:421`). "Not now" is expressible. "Not ever" is
not.

## 2. The numbers

### What our messages actually weigh

Measured, not assumed. Source: the two field logs from the 2026-09-09
walk, `/home/lew/rig-run/feld-archiv/pocket-lauf10.log` and
`t114-lauf10.log`, 12.69 h of wall clock each (10:52 to 23:33).

Method: for each `[TELEMETRY] report target=` line, take the packet the
node emitted within the next 14 log lines, excluding the two lengths
that are the nodes' own `lxmf.delivery` announces (181 and 183). 284
messages, which agrees with the 288 `report` lines to within the four
that straddle a log boundary.

| On-wire packet (Type 1) | Count |
|---|---|
| 227 B | 6 |
| 259 B | 80 |
| 275 B | 198 |

Median 275 B, worst case 275 B, minimum 227 B. A Type 1 header is 19 B
(`HEADER_MINSIZE`, `leviculum-core/src/constants.rs:69`), and the
propagation form re-prepends the 16-byte destination hash, so
`lxmf_data` is the packet length minus 3: **median 272 B, range 224 to
272 B.** With the 32-byte propagation stamp appended, the **stored
object is 304 B median, 256 to 304 B over the run.**

Cross-check against the relayed form: the same message crossing a hop
was logged as `[LORA] TX split 291 bytes (254+37)` with a Type 2
header, and 291 − 35 = 256 = 275 − 19. The two framings agree.

**No text messages were present.** The walk carried telemetry only, so
this distribution is a telemetry distribution and nothing else. A
Sideband text message is `LXMF_OVERHEAD` = 112 B
(`LXMF_OVERHEAD`, `LXMessage.py:63`) plus the RNS encryption overhead
plus the text, so a one-line message lands in the same 250 to 350 B
band; anything with an image or an audio field is one to two orders of
magnitude larger and is exactly what the advertised transfer limit
exists to refuse.

### How many fit

The store's own structure, stated rather than waved at. A
log-structured record store on 4 KB erase sectors, appended in place,
reclaimed a whole sector at a time. Per record:

| Field | Bytes |
|---|---|
| body length | 2 |
| transient ID | 32 |
| receive timestamp | 4 |
| stamp value | 1 |
| flags (live / purged) | 1 |
| CRC-16 | 2 |
| **header total** | **42** |

The destination hash is not duplicated: it is the first 16 bytes of the
body, as it is in the reference, which reads it back from the head of
the file (`LXMRouter.py:581`).

At the measured median body of 304 B a record is 346 B. Records do not
straddle a sector, so 11 fit in a 4 KB sector with 290 B of tail
(7.1 %). Reserving 8 sectors for the superblock pair and spares:

| Board | Part | Sectors | Usable | Messages | Message bytes |
|---|---|---|---|---|---|
| T114 | MX25R1635F, 2 MB | 512 | 504 | **5 544** | 1 646 KiB |
| Pocket V2 | IS25LP080D, 1 MB | 256 | 248 | **2 728** | 810 KiB |

Allowing records to straddle sectors buys about 7 % (5 966 and 2 935)
at the cost of a harder recovery scan. Not worth it.

For scale: one message at the reference's default per-transfer limit of
256 KB would occupy 79 % of the T114's usable store and would not fit
on the Pocket at all once the reserve is taken. That is the argument
for announcing a small field 3, not a preference.

### What an index costs in the heap

Measured heap, from the same field run, on the Pocket at the end of
12.69 h:

```
[HEAP] used=58612 free=39692 watermark=58996 size=98304
```

96 KiB of heap, 58 996 B at the high-water mark, so **39 308 B of free
heap in the worst observed moment.**

A reference-shaped in-RAM index costs, per message, 32 B of transient
ID as the key plus 16 B destination hash, 4 B offset, 2 B size, 4 B
timestamp and 1 B stamp value: 59 B, before any map overhead.

| Board | At capacity | Full RAM index | One peer's unhandled set |
|---|---|---|---|
| T114 | 5 544 messages | 319 KiB | 173 KiB |
| Pocket | 2 728 messages | 157 KiB | 85 KiB |

**The full index does not fit — it is eight times the whole heap on the
T114, and the per-peer sets are worse, because there is one pair of
them per peer and the reference peers with up to 20**
(`MAX_PEERS`, `LXMRouter.py:43`). 39 308 B holds 666 full entries; a
defensible 8 KiB budget holds 138.

What does fit, in order of preference:

1. **No RAM index at all: an on-flash directory, scanned.** The record
   headers *are* the directory. A `/get` list request scans the store
   for records whose body begins with the caller's delivery
   destination hash. Cost is a sequential read of the part, and that
   is affordable (below).
2. **A bounded RAM cache of the newest N.** 138 entries in 8 KiB
   answers the common case — a phone that syncs every few minutes
   wants the recent tail — and falls back to the scan for the rest.
3. **A Bloom filter over transient IDs**, to answer "do I already have
   this?" on the accept path without a scan. 5 544 entries at 1 % false
   positive is about 6.6 KiB, and a false positive costs one scan, not
   a wrong answer.

Not on the list: per-peer handled/unhandled sets. They cannot be made
to fit and they are bookkeeping, not protocol.

### Endurance

Datasheet figures, both parts, both cited.

| | IS25LP080D (Pocket) | MX25R1635F (T114) |
|---|---|---|
| Density | 8 Mbit / 1 MB | 16 Mbit / 2 MB |
| Endurance | 100 000 cycles min (JEDEC A117) | 100 000 cycles min |
| Retention | 20 years | 20 years |
| Sector erase, 4 KB | 70 ms typ / 300 ms max | 58 ms typ / 240 ms max |
| Block erase, 32 KB | 0.1 s / 0.5 s | 1 s / 3 s |
| Block erase, 64 KB | 0.15 s / 1.0 s | 0.8 s / 3.5 s |
| Chip erase | 2 s / 6 s | 30 s / 60 s |
| Page program, 256 B | 0.2 ms / 0.8 ms | 3.2 ms / 10 ms |
| Standby current | 8 µA typ | 5 µA typ (ultra-low-power mode) |

Sources: ISSI *IS25LP080D / IS25WP080D/040D/020D* data sheet, Rev. B4,
2018-02-15, §9.9 Program/Erase Performance and §9.10 Reliability
Characteristics; Macronix *MX25R1635F* data sheet, Rev. 1.6,
2018-12-12, key-features list and the Ultra Low Power Mode AC
characteristics table. Both parts erase in 4 KB sectors and 32/64 KB
blocks.

Note the asymmetry: the Macronix part is the low-power one and pays for
it in write time. Programming a page costs 16× what it costs on the
ISSI part, and a chip erase costs 15×.

**The write pattern a log-structured store produces** is: append 346 B,
which touches one or two 256-byte pages; erase one 4 KB sector when the
allocator wraps onto it. Reclaim is round-robin over the whole part, so
wear is level by construction — that is the wear levelling, and it is
free, provided nothing is ever written to a *fixed* location.

Duty, measured: 284 messages from two moving trackers over 12.69 h =
**22.4 messages/hour**.

| Duty | Messages/year | Sector erases/year | T114 life | Pocket life | One fixed index sector |
|---|---|---|---|---|---|
| measured (×1) | 196 000 | 16 561 | 3 092 years | 1 546 years | **6.1 months** |
| ×10 (a busy 20-node mesh) | 1 960 000 | 165 606 | 309 years | 155 years | **18 days** |
| ×100 | 19 600 000 | 1 656 063 | 31 years | 15 years | **3 days** |

Read the last column twice. **The message data is not the endurance
risk; a fixed metadata sector is.** A store that keeps its head pointer,
its index, or its sequence counter in one sector and rewrites it on
every accepted message spends its whole 100 000-cycle budget in six
months at the duty we actually measured in the field. A store that
writes only forward and reclaims round-robin outlives the board by
three orders of magnitude.

This is the single most important engineering constraint on the page,
and it is a constraint on the store, not on LXMF.

### Time

**Flash.** Filling the part once, from the table above:

- T114: 512 sector erases × 58 ms = 29.7 s, plus 8 192 page programs ×
  3.2 ms = 26.2 s. **56 s.**
- Pocket: 256 × 70 ms = 17.9 s, plus 4 096 × 0.2 ms = 0.8 s.
  **19 s.**

Per accepted message: one or two page programs (6.4 ms worst case on
the T114, 0.4 ms on the Pocket) and, once every eleven messages, one
sector erase (58 / 70 ms). Both parts can suspend an erase, so the
erase does not have to block the radio; but the simpler answer is that
58 ms of flash-busy time every eleven messages is 0.5 % of the airtime
those eleven messages cost.

**Reading.** The nRF52840 QSPI runs to 32 MHz and embassy-nrf exposes
it (`Frequency`, `embassy-nrf-0.9.0/src/qspi.rs`). The Macronix part in
its default ultra-low-power mode caps quad reads at 8 MHz — 4 MB/s —
and the ISSI part allows 133 MHz, so 32 MHz is the controller's limit
there: 16 MB/s. **A full-store scan is 0.5 s on the T114 and 0.07 s on
the Pocket.** That is what makes the on-flash directory viable: a
`/get` list request that costs half a second of QSPI is not a problem;
a 319 KiB RAM index is.

**LoRa.** Measured, from the field log: a telemetry message crossing a
hop is 291 bytes on the wire, split into 254 + 37 byte frames, and the
firmware reported `op=tx duration_ms=903..905` for 13 of the 15 such
transmissions in the run (SF8, BW 125 kHz, CR 4:5, 18-symbol preamble).
Our airtime model agrees: computed 543 ms for the 184-byte frame the
same log reports as `airtime_ms=544` (`airtime_ms_with_preamble`,
`leviculum-core/src/rnode.rs:909`; preamble from
`derive_preamble_symbols`, `leviculum-core/src/rnode.rs:839`, which
floors at 18, `LORA_PREAMBLE_SYMBOLS_MIN`,
`leviculum-core/src/rnode.rs:776`).

At 904 ms per message and the 10 % duty-cycle cap the firmware enforces
(`[LORA_AIRTIME_LOCK] lt=1000 lt_cap=10.00%` in the same run):

| Board | Full store | Pure airtime | Wall clock at 10 % duty |
|---|---|---|---|
| T114 | 5 544 messages | 1.39 h | **13.9 h** |
| Pocket | 2 728 messages | 0.68 h | **6.8 h** |

with zero retransmissions, zero link setup and no other traffic on the
channel. **A store nobody can drain in a reasonable time is a museum,
and over LoRa a full store is a museum.** Only the delta between two
meeting nodes is ever transferable in a walk-past.

**BLE.** The negotiated MTU is bounded by measurement rather than
assumed: the SoftDevice is configured with an ATT MTU ceiling of 256
(`CONN_GATT`, `leviculum-nrf/src/ble/mod.rs:640`), but the field log
shows a 183-byte packet fragmenting into 2 and a 275-byte packet also
into 2, which brackets the payload per fragment to 138 to 182 bytes and
the MTU to 146 to 190 — consistent with the 185 default
(`DEFAULT_MTU`, `leviculum-core/src/framing/ble.rs:89`;
`payload_per_fragment`, `leviculum-core/src/framing/ble.rs:107`). At
177 bytes per fragment a 304-byte message is 2 notifications, so a full
store is 11 088 notifications on the T114 and 5 456 on the Pocket.

**The sustained notification rate is not measured and this page will
not invent it.** The field run carried sparse traffic — the tightest
observed spacing is two packets in the same millisecond, which is a
burst, not a rate. What can be said is the shape: at 10 notifications/s
a full T114 store is 18 minutes and at 100/s it is under two minutes,
so BLE is not the binding constraint, and the measurement is owed
rather than critical. It is named in §5.

### What accepting a message costs in CPU

A propagation stamp is validated by expanding a 1 000-round workblock
from the transient ID and hashing it with the stamp
(`WORKBLOCK_EXPAND_ROUNDS_PN`, `LXStamper.py:13`; `stamp_workblock`,
`LXStamper.py:49`; `validate_pn_stamp`, `LXStamper.py:84`). Each round
is one SHA-256 over the salt input plus one HKDF-SHA256 producing 256
bytes: one extract HMAC and eight expand HMACs, four SHA-256
compressions each. **37 compressions per round, 37 000 for the
workblock, plus 4 000 for the final digest over the 250 KiB workblock:
41 000 SHA-256 compressions, 2.62 MB hashed, per message.**

Two things follow.

**The reference materialises the 250 KiB workblock in RAM. We do not
have to, and already do not.** `workblock_hasher`
(`leviculum-lxmf/src/stamp.rs:143`) streams the HKDF blocks straight
into the digest and keeps one 256-byte block. The RAM objection to
stamp validation is already solved in our tree; only the CPU cost
remains.

**At an advertised cost of 0 the cost is not incurred at all.** Our
validator short-circuits before the workblock when the cost is zero
(`validate_stamp`, `leviculum-lxmf/src/stamp.rs:198`), and the firmware
already runs this way for delivery stamps: the LXMF dependency is
pulled with default features off, so the node "advertises a zero stamp
cost and mines nothing" (`leviculum-nrf/Cargo.toml:31`).

The expensive case is *peering out* to a Python node, which requires
mining a key at that node's advertised peering cost, default 18, over a
25-round workblock (`WORKBLOCK_EXPAND_ROUNDS_PEERING`,
`LXStamper.py:14`; `generate_peering_key`, `LXMPeer.py:242`). With the
precomputed-digest-state trick our miner already uses, that is 925
compressions for the workblock plus about two per trial over 2^18
expected trials: **525 000 compressions, 33.6 MB hashed, once per peer,
and the result is persistable.** The reference's own miner rehashes the
6.4 KB workblock every trial and so hashes 1.7 GB for the same key;
this is a legitimate deviation under the deviation rule, since the
stamp produced is byte-identical.

Converting compressions to seconds needs a SHA-256 throughput on the
nRF52840 at 64 MHz that **we have not measured**. For orientation only,
at 20 / 40 / 60 cycles per byte the stamp validation is 0.8 / 1.6 /
2.5 s and the peering key is 10 / 21 / 32 s. The measurement is owed
(§5); the conclusion that survives any plausible value is that
per-message stamp validation at a nonzero cost is seconds of the only
core we have, and a peering key is a one-off we can afford.

## 3. The options

Four, and the fourth is doing nothing on the board.

| | A. Full propagation node | B. Bounded node, honest limits | C. Courier for recently-seen peers | D. Nothing on the board; `lnsd` carries it |
|---|---|---|---|---|
| What Sideband sees | a normal propagation node | a normal propagation node with small limits | nothing; not a PN | the PC's node, if in range |
| Announces `lxmf.propagation` | yes | yes | no | n/a |
| RAM at capacity | 319 KiB index + 173 KiB per peer | on-flash directory + 8 KiB cache + 6.6 KiB filter | same as B, smaller | 0 |
| Peers | autopeer, up to 20 | autopeer, capped low | none | as configured |
| Stamp cost advertised | 16 | 0, or a low nonzero once measured | n/a | 16 |
| Two boards meet, no phone | works, if both can peer | **works** | works, but only between our own boards | **does not work** |
| Board switched off mid-transfer | client retries; nothing lost | client retries; nothing lost | our own protocol, our own problem | n/a |
| Verdict | impossible | viable | not compatible | insufficient |

**A, the full node, is dead on two independent counts.** The per-peer
handled/unhandled sets are 173 KiB *per peer* at capacity against
39 KiB of free heap, and there is no cap we control on who peers with
us: any Python router within four hops that hears our announce peers
automatically (`AUTOPEER_MAXDEPTH`, `LXMRouter.py:45`;
`LXMFPropagationAnnounceHandler`, `Handlers.py:35`). Worse, every
message we accept is enqueued for every peer
(`flush_peer_distribution_queue`, `LXMRouter.py:2472`), so a store we
filled from a phone over BLE would be re-offered over a 10 %-duty LoRa
link to everyone in range. That is not a tuning problem.

**B, the bounded node, is the only option that satisfies the framing.**
Everything it needs is already expressible in the announce: a small
field 3 and field 4 that peers and clients honour, a stamp cost we
choose, and a `max_peers` of our own. It costs the spam brake — a node
advertising cost 0 can be filled by anyone — which the small transfer
limit and the size cull bound but do not remove. It is the only option
where a Sideband user gets the thing they expect without knowing the
node is small.

**C, the courier, is out on compatibility, not on cost.** Holding
messages only for destinations we have recently seen is a good policy
and would fit the RAM budget comfortably. But there is no verb for it:
a node that does not announce `lxmf.propagation` is invisible to
Sideband, and a node that announces it and then behaves as a courier is
lying about field 2. C is a *policy inside B*, not an alternative to
it — and as a policy inside B it is exactly the right one for the
bounded case.

**D is what we do today and it is insufficient for the case that
prompted the issue.** Two boards on a hill with no PC in range have no
store between them.

### The case with no phone present

Explicitly, because it is the operator's case. Under B, two boards that
meet with no phone can exchange messages by two paths, and the cheap
one is worth naming:

- **Full peering.** Both announce as propagation nodes, autopeer within
  four hops, mine a peering key at each other's cost (which, since we
  choose our own, can be low between our own boards), and sync over a
  Link and a Resource. Correct, and bounded by the 10 % duty cycle: the
  delta, not the store.
- **Single-message client upload.** `propagation_packet`
  (`LXMRouter.py:2234`) accepts one message per link packet with no
  peering key at all. Two boards can hand each other one message at a
  time with no peering, no Resource, and no mining. For a walk-past on
  LoRa, where 904 ms of airtime per message is the real budget, this is
  the path that matches the medium.

Under A the same is true but the store re-offer makes it unusable.
Under C it works only between our own boards. Under D it does not work.

### Switched off mid-transfer

The protocol is safe against our disappearance at every point, and the
reason is worth recording because it constrains our implementation:

- **Mid-upload**, the client's packet is proven only after the message
  is stored (`packet.prove`, `LXMRouter.py:2255`). If we die first, the
  client gets no proof and retries. **This makes "persist before you
  prove" a rule, not a preference** — a proof written before the record
  is durable converts a power cut into a lost message.
- **Mid-`/get`**, the node deletes only on the client's explicit
  `haves` purge, and the client sends that purge only after local
  delivery (`message_get_response`, `LXMRouter.py:1607`). If we die
  during the transfer, nothing is deleted and the client repeats the
  exchange.
- **Mid-sync with a peer**, the offering peer marks nothing handled
  until the transfer concludes, and a failed request tears the link
  down and backs off (`request_failed`, `LXMPeer.py:395`).

The one thing that is *not* safe is a store whose own recovery is
unsound. A power cut in the middle of an append must leave a store that
reopens with every completed record and no partial one, which is what
the per-record CRC and the forward-only log are for.

## 4. The recommendation

**Option B, but not yet as LXMF: build the store first, and the role
second.** The numbers say the flash is ample (5 544 messages on the
T114, 2 728 on the Pocket, against a measured field duty of 22
messages/hour), the endurance is ample by three orders of magnitude
*provided* nothing is written to a fixed sector (a fixed index sector
dies in six months at exactly the duty we measured), and the binding
constraint is neither: it is the 39 KiB of free heap, which rules out
the reference's index shape and forces an on-flash directory that a
0.5 s scan makes perfectly affordable. Every one of those conclusions
is about the store and none of them is about LXMF, so the store is
what the first batch builds — and it is needed by lnmsg's mailbox and
by telemetry retention whatever we decide about propagation. The role
itself waits on two measurements we do not have and cannot fake: the
sustained BLE notification rate, which decides whether a phone can
drain the store in a usable time, and the SHA-256 throughput on the
board, which decides whether we can ever advertise a nonzero stamp cost
and keep the spam brake the protocol was designed around.

### The first batch, and its acceptance

**Batch: drive the QSPI flash and land a log-structured record store.
No LXMF, nothing announced on `lxmf.propagation`.**

1. A QSPI driver behind the existing storage trait shape, on both
   boards, at `Frequency::M8` on the T114 (its part's ultra-low-power
   quad-read ceiling) and `M32` on the Pocket. The pin aliases already
   exist and name the right parts (`QspiClk`,
   `leviculum-nrf/src/boards/t114.rs:103`; `QspiClk`,
   `leviculum-nrf/src/boards/rak4631.rs:100`); nothing else is wired
   to those pins on either board.
2. A forward-only record log: 4 KB sectors, 42-byte header as
   tabulated, CRC per record, round-robin sector reclaim, **no fixed
   metadata sector anywhere**.
3. An on-flash directory: lookup by destination-hash prefix is a
   sector scan, plus a bounded RAM cache of the newest entries sized
   against the measured free heap.

Acceptance, all four:

- **Host tests** over a simulated NOR device with real semantics
  (erase to `0xFF`, program-once bits, 4 KB granularity): append, scan,
  reclaim, and a power-cut injected at *every* byte offset of a record
  write, each of which must reopen with every completed record and no
  partial one.
- **A wear pin with its own negative control**: write enough records to
  wrap the part twice and assert that the per-sector erase counts
  differ by at most one. The negative control pins a fixed sector and
  asserts the check fails.
- **On the rig**: fill the part on both boards, power-cycle, read back,
  and report the record count and a byte-exact digest of the store.
- **Two measurements reported as numbers, not as pass/fail**:
  sustained BLE notification throughput to a phone, and SHA-256
  bytes/second on the nRF52840 at 64 MHz. These are the inputs to the
  decision about the role; a batch that lands the store without them
  has not finished.

### What would change this, and what to measure again

- **SHA-256 throughput on the board.** If it is at the fast end, a
  nonzero propagation stamp cost is affordable and B keeps the spam
  brake. If it is at the slow end, B has to advertise cost 0 and rely
  on the transfer limit and the cull, and that trade must be written
  down as a deviation with its reason.
- **Sustained BLE notification rate.** If a full store cannot be
  drained in minutes, the bounded capacity should be cut to what can
  be, and the number to cut it to comes from this measurement.
- **Message-size distribution beyond telemetry.** The 284 samples here
  are all telemetry. A run carrying real Sideband text and a phone that
  sends an image will move the median and, more importantly, will show
  how often the advertised transfer limit actually bites.
- **Whether autopeering can be bounded in practice.** The reference
  peers with anyone within four hops. B assumes a `max_peers` we
  enforce ourselves keeps that survivable; a mesh test with three or
  more Python routers in range would show whether it does.

### One correction this page owes

Codeberg #384 states that "a search of `leviculum-nrf` finds no QSPI".
It finds the pin aliases: both board files already declare the six
QSPI pins and name the correct part in a comment
(`QspiClk`, `leviculum-nrf/src/boards/t114.rs:103`; `QspiClk`,
`leviculum-nrf/src/boards/rak4631.rs:100`). What is absent is a driver,
a filesystem and any use. The conclusion of the issue is unaffected —
the flash is untouched — but the starting point is a little further
along than it says.
