# Interface Isolation

The single most important architectural rule in Leviculum:

> **Only the interface knows the quirks of its carrier medium. The
> core, the transport, and the daemon are media-agnostic.**

A packet is a packet. At the boundary where the core hands bytes to
an interface, there is no distinction between an announce, a link
request, a data packet, or a resource chunk. They are all just bytes.

## What "media-agnostic core" means

`leviculum-core` decides *what* to send, to *which* interface, and —
on an interface that carries several peers — for *which peer* (the
`peer` hint on `Action::SendPacket`, Codeberg #376: an identity, never
a link, a handle or an address, so the interface still owns the map
from peer to link). It never decides *when* to put a frame on the
wire, never spaces transmissions, and never reasons about
contention. The core processes
every packet with zero delay and emits an `Action::SendPacket` or
`Action::Broadcast` immediately (see [Architecture](../architecture.md)).

Because the core is the same code on a Linux daemon, an Android app,
and an nRF52 firmware image, it cannot afford to know whether the
medium underneath is a fibre-fast TCP socket or a half-duplex LoRa
radio whose airtime budget is measured in minutes. Medium awareness
lives entirely on the far side of the
[`Interface` trait](../architecture.md#interface-trait).

## What an interface is allowed to know

A LoRa interface knows it cannot transmit and receive at the same
time. It knows its `RadioSettings` (bandwidth, spreading factor,
coding rate) and therefore the airtime cost of any given frame. It
holds packets back, applies its own randomised pre-TX jitter on top of
the RNode firmware's CSMA, and refuses new frames when its airtime
budget is exhausted. Concretely:

- **Send-side jitter** — packets are queued, not sent immediately; the
  jitter window is sized from the radio parameters so two nodes do not
  re-collide (`leviculum-std/src/interfaces/rnode.rs:151`, the
  `compute_jitter_max_ms` doc comment, and the jitter queue at
  `:851`).
- **CSMA** — radio-level carrier sensing is handled by the RNode
  firmware; the interface defers collision avoidance to it rather than
  the core (`leviculum-std/src/interfaces/rnode.rs:587`).
- **Airtime backpressure** — a per-interface credit bucket charges
  every send by its airtime cost and signals `BufferFull` rather than
  flooding the serial queue
  (`leviculum-std/src/interfaces/airtime.rs:1`). This explicitly
  "never leaks into `leviculum-core`, so the `no_std` core stays free
  of host-side backpressure concerns" (same file).

A TCP interface has none of this. It just writes bytes
(`leviculum-std/src/interfaces/tcp.rs`).

## Why the rule is hard, not advisory

The rule binds anyone writing a fix. If a proposed fix for a
collision, contention, or duplex problem introduces an awareness flag
or counter in `transport.rs`, the `node/` modules, or the daemon
("is a link in flight?", "am I forwarding a link request?"), it is at
the *wrong layer*. Such a fix must be redirected into the interface.

Interface implementations are therefore free to diverge from
Python-Reticulum's thin serial-writer style — that divergence is
exactly where medium-specific intelligence belongs, and it satisfies
the project's [deviation rule](python-rns-compatibility.md#the-deviation-rule)
as long as wire and semantic compatibility are preserved.

## Consequences

- The same routing logic runs unchanged over LoRa, TCP, UDP, serial,
  and the in-process local socket. That includes relaying a packet
  back out of the interface it arrived on — the
  [same-interface relay](python-rns-compatibility.md#same-interface-relay-on-shared-media)
  decision is taken in the media-agnostic core; the interface is not
  involved.
- New media are added by implementing one trait, not by threading
  medium-specific cases through the protocol core.
- Collision-avoidance bugs are debugged in one place — the interface —
  instead of being smeared across six stack layers.

## The one place the medium is named

`InterfaceKind` (`leviculum-core/src/traits.rs`) names the carrier —
`Tcp`, `Rnode`, `Serial`, and so on — and that looks like an exception
to the rule. It is not: the kind is *reported*, never acted on. It
exists so `rnstatus` can print the Python-RNS interface class name and
so a status consumer can group interfaces by transport instead of by
their peer label.

A `match` on it outside `traits.rs` may produce a string, a number or a
status field, and nothing else. As of 2026-07-30 there are exactly two
consumers: `transport.rs`'s sparse-map bookkeeping (where `Unknown`
means "no entry") and `rpc/handlers.rs::interface_type`. A third that
decides *what the stack does* — a longer timeout for LoRa, a skipped
step on serial — is the wrong-layer fix this page describes; widen the
`Interface` trait so the interface answers the question itself.

This rule is deliberately not machine-checked. A guard could only see
a syntactic comparison against a variant, which is not the shape the
violation takes: an exhaustive `match kind` returning a timeout reads
identically to one returning a label. Both existing consumers would
need an exemption, so on today's three call sites the exemption list
would be longer than the finding, and it would grow with every
legitimate status field. The rule is stated here and on the enum
instead.

See also: [Storage and Embedding](storage-and-embedding.md) for the
parallel isolation of persistence and time, and the
[RNode protocol](../rnode-protocol.md) page for the LoRa carrier
details an interface must handle.
