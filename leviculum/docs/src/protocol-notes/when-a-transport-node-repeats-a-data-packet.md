# When a transport node repeats a data packet

Established from `reference/Reticulum` while fixing #383, where a node that
was neither sender nor destination repeated all 30 probes it overheard on a
shared medium and buried 24 of the 30 answers.

## 1. Which data packets the reference forwards at all

`Transport.inbound` reaches its path-table forwarding branch only through this
condition (`RNS/Transport.py:1559-1560`):

```python
if packet.transport_id != None and packet.packet_type != RNS.Packet.ANNOUNCE:
    if packet.transport_id == Transport.identity.hash:
        if packet.destination_hash in Transport.path_table:
```

Three gates in order: the packet must carry a transport id, that id must be
this node's own identity hash, and a path to the final destination must be
known. The complementary case is handled earlier, in
`Transport.packet_filter` (`Transport.py:1341-1344`): a non-announce packet
whose transport id names a *different* instance is rejected before any of this
runs. So a transport node acts on exactly one class of overheard traffic, the
class addressed to it by name.

Once inside, the hop count only selects the header rewrite
(`Transport.py:1567-1581`): `remaining_hops > 1` keeps HEADER_2 and swaps in
the next hop, `remaining_hops == 1` strips back to HEADER_1, `remaining_hops
== 0` just bumps the count. All three transmit.

**A packet addressed directly to a destination one hop away is never handed to
a transport node.** The sender decides this, in `Transport.outbound`
(`Transport.py:1134-1166`): a path table entry with `hops > 1`, or `hops == 1`
while the sender is behind a shared instance, gets the HEADER_2 transport
header with the next hop written into it. Anything else falls through to

```python
# If none of the above applies, we know the destination is
# directly reachable, and also on which interface, so we
# simply transmit the packet directly on that one.
```

which puts HEADER_1 on the air with no transport id. That packet fails the
very first gate at `Transport.py:1559` on every node that hears it, so a
neighbour holding its own path to the destination repeats nothing. Holding a
path is not an invitation to forward; being named is.

The one exception, and it is not really an exception: if the destination sits
behind a local client of a shared instance, the previous hop stripped the
transport id (clients are made to look directly reachable), so the instance
synthesizes it back before the gate (`Transport.py:1543-1548`):

```python
if packet.transport_id == None and for_local_client:
    packet.transport_id = Transport.identity.hash
```

`for_local_client` is a path table entry at `hops == 0` (`Transport.py:1513`).

## 2. What the reference does repeat without being named

Two mechanisms, and a fix to the path-table gate must leave both alone.

**Link table** (`Transport.py:1648-1686`). Packets addressed to an established
link's id carry no transport id at all, and the relay repeats them purely off
its `link_table` entry. This is not overhearing: the entry exists only because
this node forwarded the LINKREQUEST earlier, as its designated next hop
(`Transport.py:1625`). The same-interface case is explicit about repeating
back onto the medium the packet arrived on:

```python
# If receiving and outbound interface is
# the same for this link, direction doesn't
# matter, and we simply repeat the packet.
```

gated on the taken hop count matching one of the two frozen counts, which is
what stops the relay-to-relay echo on one channel.

**Announces.** Rebroadcast is transport-id-independent by design; the
`packet_filter` exemption at `Transport.py:1342` exists for it.

## 3. Shared versus point-to-point

Nothing in any of this inspects whether the interface is a shared medium. The
gate is a property of the packet, not of the carrier, and it produces the
right behaviour on both: on a point-to-point link the only node that hears the
packet is the one it was sent to, so the gate never fires; on a shared medium
it is the only thing standing between one probe and N repeats.

The one interface comparison in the area, `link_entry[IDX_LT_NH_IF] ==
link_entry[IDX_LT_RCVD_IF]`, compares two stored interface indices of one link
entry. It asks whether a relayed link happens to enter and leave by the same
interface, not whether that interface is shared.

## Consequences for us

`leviculum-core/src/transport.rs`, `handle_data`, now gates the path-table
forward on `transport_id == Some(own hash)`, with the `for_local` synthesis
arm. The LINKREQUEST path already carried the same gate (`designated_hop`),
added for the LRPROOF echo storm; data never got it.

`Path::needs_relay()` (`storage_types.rs:60`, `hops > 1 && next_hop.is_some()`)
is not the predicate for this and never was. It answers how to rewrite the
header of a packet already accepted for forwarding, mirroring the
`remaining_hops` split above. Gating acceptance on it would kill the last hop
of every chain: in A-B-C where A cannot hear C, B is correctly the designated
hop and B's path onward to C is exactly one hop, so `needs_relay()` is false
for the very packet B must repeat.

Pinned by `leviculum-core/src/node/mvr_overheard_direct_data.rs`, whose
control 2 is that chain packet.
