# Announce dedup and path replacement (Python-RNS reference facts)

The reference facts behind the #376 desk measurement (a board's announce
reaches Columba only through the other board, never on the direct BLE
link). Four questions, answered strictly from the vendored
`reference/Reticulum` tree (Python-RNS 1.3.5), with citations. This page
states what the reference does; it decides nothing.

Sibling pages: [Hop counting](../architecture-hop-counting.md),
[Broadcast Python-RNS parity](../architecture-broadcast-python-parity.md).

## 1. Direct vs. forwarded copy: same packet hash

**Question.** An announce received directly (header type 1, wire hops 0)
versus the same announce forwarded by a transport node (header type 2,
wire hops 1, transport id inserted): same packet hash?

**Answer: yes, the hash is identical.** The hashable part is built by
`get_hashable_part` (`reference/Reticulum/RNS/Packet.py:355`):

```python
def get_hashable_part(self):
    hashable_part = bytes([self.raw[0] & 0b00001111])
    if self.header_type == Packet.HEADER_2:
        hashable_part += self.raw[(RNS.Identity.TRUNCATED_HASHLENGTH//8)+2:]
    else:
        hashable_part += self.raw[2:]
    return hashable_part
```

Three exclusions make the two copies hash the same:

* **The hop count is excluded entirely.** It is header byte 1 — `hops`
  (`reference/Reticulum/RNS/Packet.py:245`) — and both branches above
  start at byte 2 or later.
* **The transport id is excluded.** For header type 2 the slice starts
  after the 16-byte transport id (`TRUNCATED_HASHLENGTH//8 + 2` = 18).
* **The header-type and transport-type bits are masked off.** Byte 0 is
  packed as `packed_flags` (`reference/Reticulum/RNS/Packet.py:171`) —
  `header_type << 6 | context_flag << 5 | transport_type << 4 |
  destination.type << 2 | packet_type` — and the `& 0b00001111` mask
  keeps only destination type and packet type. Header type (bit 6),
  transport type (bit 4) and the on-air IFAC flag (bit 7) all vanish.

So a relay changing hops, inserting its transport id and flipping the
header to type 2 does not change the packet hash: the two copies are the
same announce to every dedup structure keyed on `packet_hash`.

## 2. The second copy in `Transport.inbound`: order, and the announce exemption

**Order.** The dedup check runs **first**, the path-table update later,
and both copies pass the dedup:

1. `packet.hops` (`reference/Reticulum/RNS/Transport.py:1457`) is
   incremented for every inbound packet, right after unpack.
2. The filter runs — `packet_filter`
   (`reference/Reticulum/RNS/Transport.py:1486`) gates all further
   processing.
3. On acceptance the hash is remembered at once — `add_packet_hash`
   (`reference/Reticulum/RNS/Transport.py:1506`) — before any announce
   processing (deferred only for link-table traffic and LR proofs).
4. Announce processing, including the path-table update, comes much
   later in the same call — `validate_announce`
   (`reference/Reticulum/RNS/Transport.py:1691`).

**The exemption.** Inside `packet_filter`
(`reference/Reticulum/RNS/Transport.py:1336`), a packet whose hash is
already in `packet_hashlist`
(`reference/Reticulum/RNS/Transport.py:1376`) is dropped — **except an
announce for a SINGLE destination, which is accepted anyway**:

```python
if not packet.packet_hash in Transport.packet_hashlist and ...: return True
else:
    if packet.packet_type == RNS.Packet.ANNOUNCE:
        if packet.destination_type == RNS.Destination.SINGLE:
            return True
```

So the second copy of the same announce is **not** discarded by the
hashlist. It runs the full announce path again, and what it may change
is decided there, by the random-blob replay check and the hops
comparison of §3 — not by dedup. For #376 this matters in both
directions: hashlist dedup cannot explain a missing direct announce, and
hearing the relayed copy first does not inoculate the node against the
direct copy.

## 3. The path replacement rule

All of this sits under the hop cap and non-local condition —
`PATHFINDER_M` (`reference/Reticulum/RNS/Transport.py:1750`) — with
`announce_emitted` (`reference/Reticulum/RNS/Transport.py:1753`) the
emission timestamp read out of the announce's random blob (bytes 5..10;
`announce_emitted`, `reference/Reticulum/RNS/Transport.py:3191`), and
the table side aggregated as the maximum over the recorded blobs
(`timebase_from_random_blobs`,
`reference/Reticulum/RNS/Transport.py:3182`).

**Unknown destination:** added unconditionally — `should_add`
(`reference/Reticulum/RNS/Transport.py:1831`).

**Fewer or equal hops** than the table entry — `path_table`
(`reference/Reticulum/RNS/Transport.py:1765`):

```python
if packet.hops <= Transport.path_table[packet.destination_hash][IDX_PT_HOPS]:
    path_timebase = Transport.timebase_from_random_blobs(random_blobs)
    if not random_blob in random_blobs and announce_emitted > path_timebase:
        should_add = True
```

(`path_timebase`, `reference/Reticulum/RNS/Transport.py:1772`.) Two
conditions, **both** required:

* the random blob must be new — the *same* announce heard again, e.g.
  the direct copy after the relayed copy, has the same blob and does
  **not** replace the path, however many hops it saves;
* the emission timestamp must be **strictly newer** than the newest one
  recorded for the destination. A later announce whose clock is behind
  the recorded one loses even at fewer hops — which is why the #376
  announce instrument keeps the telemetry path's clock gate.

**More hops** than the table entry: ignored, unless one of three
escapes fires, in order —

* the path has expired: `path_expires`
  (`reference/Reticulum/RNS/Transport.py:1793`), still requiring an
  unseen blob;
* the emission is strictly newer: `path_announce_emitted`
  (`reference/Reticulum/RNS/Transport.py:1809`), still requiring an
  unseen blob;
* same emission, but the recorded path has been marked unresponsive:
  `path_is_unresponsive`
  (`reference/Reticulum/RNS/Transport.py:1822`).

## 4. No hops-0 drop rule

**Question.** Does Python drop or downgrade announces received with
hops 0 on any interface class — anything that could make Columba ignore
a direct board announce?

**Answer: no such rule; searched `Transport.inbound`.** There is no
condition anywhere in `Transport.inbound` that keys on `hops == 0` for
an announce or treats a directly received announce worse than a relayed
one. (`packet.hops`, `reference/Reticulum/RNS/Transport.py:1457`,
increments every inbound packet before any decision, so a direct
announce is processed at `hops == 1`; the only decrements are the two
shared-instance IPC cases — `is_local_client_interface`,
`reference/Reticulum/RNS/Transport.py:1482` — which are not radio
interfaces.)

The gates that *do* exist on the way to the path table, for any hops
value, are:

* **Signature validation** — `validate_announce`
  (`reference/Reticulum/RNS/Identity.py:532`): fails, and the announce
  is silently dropped.
* **Ingress limiting** — `should_ingress_limit`
  (`reference/Reticulum/RNS/Transport.py:1705`): on an interface with
  `ingress_control` enabled (`should_ingress_limit`,
  `reference/Reticulum/RNS/Interfaces/Interface.py:145`), an announce
  for an *unknown* destination can be held — `hold_announce`
  (`reference/Reticulum/RNS/Transport.py:1706`) — rather than
  processed; a pending path request bypasses the hold.
* **PLAIN/GROUP announces** are always invalid
  (`packet_filter`, `reference/Reticulum/RNS/Transport.py:1336`);
  `lxmf.delivery` announces are SINGLE and unaffected.
* **The §3 replacement conditions**, in particular the strict
  emission-timestamp comparison.

Recorded 2026-09-09 against `reference/Reticulum` as vendored (1.3.5).
