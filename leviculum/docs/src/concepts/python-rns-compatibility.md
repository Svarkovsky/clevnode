# Python-RNS Compatibility

Leviculum is built to live in the same mesh as Python Reticulum
(`rnsd`) and to be a drop-in replacement for the daemon and its
tooling. Compatibility is pursued at two distinct levels, and one
thing that is *not* pursued at all.

## Level 1: wire and semantic compatibility

The protocol the two stacks speak must be identical on the air. The
exact bytes of identities, destinations, announces, packets, and links
are fixed by the
[Reticulum specification](../appendix/reticulum-specification.md);
the message format layered on top is fixed by the
[LXMF specification](../appendix/lxmf-specification.md). Leviculum
implements those formats so that a Python peer cannot tell a Leviculum
neighbour from another Python node.

Semantic compatibility goes beyond byte layout: behaviours a Python
peer *expects* from a neighbour — answering path requests, rebroadcast
decisions, link lifecycle, ratchet handling — must still be delivered.
Where the precise expected behaviour matters and is subtle, it is
captured as a source-of-truth reference; the broadcast path is
documented in
[Broadcast: Python-RNS parity reference](../architecture-broadcast-python-parity.md),
which records what Python does for every broadcast mechanism so the
Rust core can match it.

Semantic compatibility is decided field by field: what a peer
*decides* from a value we generate is part of the contract, even when
the byte layout is right. The audit method and testing rule for that
are in [Wire Field Semantics](wire-field-semantics.md).

It cuts both ways. The reference's **accept set** is as much of the
contract as its output set, and it is wider — Python's decoder takes
forms Python's encoder never emits. A mesh has more than two
implementations in it (reticulum-kt, microReticulum, hand-rolled
senders), and any of them may pick a legal encoding Python happens not
to use. Testing only against `rnsd` cannot see this: it never produces
the form we refuse. Being stricter than the reference on the read
path is a compatibility defect of the same standing as emitting a
wrong value, and it fails silently — see
[Wire Field Semantics](wire-field-semantics.md#the-mirror-question-what-do-we-refuse-to-read).

## Level 2: drop-in daemon and tooling

`lnsd` shares two interfaces with Python's `rnsd`:

- **The shared-instance IPC socket.** A running daemon exposes a local
  control/data channel that client tools connect to. Leviculum speaks
  the same protocol, so Python's `rnstatus`, `rnpath`, `rnprobe`, and
  `rncp` drive a running `lnsd` without modification, and the
  Leviculum tools `lnstest` and `lncp` drive a running `rnsd` just the
  same. The RPC control channel that backs `rnstatus`/`rnpath`/
  `rnprobe` is implemented in `leviculum-std/src/rpc/` (it speaks
  Python's `multiprocessing.connection` framing with pickle payloads,
  see `rpc/connection.rs` and `rpc/pickle.rs`).
- **The config-file format.** `lnsd` parses the same INI-style config
  that `rnsd` uses (`leviculum-std/src/config.rs`,
  `leviculum-std/src/ini_config.rs`). Even keys Leviculum does not act
  on are parsed for compatibility — for example `shared_instance_type`
  and `shared_instance_socket` are read and honoured per RNS 1.3.x
  semantics so an existing `rnsd` config works unchanged
  (`leviculum-std/src/config.rs:47-53`).

This drop-in property is a deliberate design goal, not an accident.
It is also what makes honest A/B testing possible: the test harness
points the *same* client binary (e.g. `lnstest selftest`) at either
daemon, never a parallel per-stack driver. A parallel driver would
smuggle configuration differences into what claims to be a stack
comparison.

## What is explicitly *not* a goal: internal parity

Compatibility is not the same as parity.

- **Compatibility** — our stacks interoperate at the wire and semantic
  level.
- **Parity** — our internals mirror Python's (same algorithms, same
  retry timings, same state-machine structure).

Leviculum needs the first, not the second. The historical parity
documents under `docs/src/architecture-*-python-parity.md` are
reference material for *getting behaviour right*, not commitments to
maintain identical internals.

## The deviation rule

A deviation from Python-RNS's implementation is acceptable if and only
if **all three** of the following hold:

1. Wire-format compatibility is preserved.
2. Semantic compatibility is preserved (behaviours Python peers expect
   from a neighbour are still delivered).
3. The deviation measurably improves robustness or mesh delivery.

"Because Python does it differently" is not, on its own, an objection;
only "this breaks wire or semantic compatibility" is. The
[interface-isolation](interface-isolation.md) design — interfaces
applying their own jitter, CSMA, and airtime budgeting — is a
deliberate deviation that satisfies this rule.

A deviation that is not written down is indistinguishable from a bug.
Each one is pinned here with the reference line it departs from, so
the next reader can check the claim instead of re-deriving it.

### Pinned deviation: ingress-control default on dial-out links

The reference gives every interface `ingress_control = True`
(`Interface.py:112`), overridable per interface by the config key of
the same name (`Reticulum.py:768-769`, applied at `Reticulum.py:910`).
Leviculum defaults it **off** on dial-out point-to-point links —
`TCPClientInterface`, `BackboneClientInterface`, `UDPInterface`, and an
`I2PInterface` without `connectable` — and leaves it **on** everywhere
else, including every listener
(`ingress_control_default_for_type`, `leviculum-std/src/config.rs:685`).

Against the rule: the flag decides only whether *we* hold incoming
announces, so no wire byte and no behaviour a peer observes changes
(1 and 2). It gains us the announces the limiter would otherwise hold
silently on a link carrying one known peer's startup burst — the
mechanism behind the Codeberg #44 flake, on our receive side (3).
An operator who wants the reference behaviour writes
`ingress_control = yes` on the interface.

The default is a *role* distinction, not a medium one: an interface
that accepts connections from arbitrary unknown peers is exactly the
announce-storm surface the limiter exists for, so a listener keeps the
reference default. What a listener resolves is inherited by every
connection it accepts, as in the reference
(`TCPInterface.py:582`, `I2PInterface.py:951`,
`BackboneInterface.py:409`). Shared-instance IPC clients are never
ingress-limited on either stack — the reference hard-wires
`should_ingress_limit` to `False` for them
(`LocalInterface.py:137-138`) — so that is not a deviation.

### Pinned deviation: an absent `txpower` is the board maximum

The reference resolves an omitted `txpower` key to **0 dBm**
(`RNodeInterface.py:153`: `int(c["txpower"]) if "txpower" in c else 0`).
Leviculum resolves it to **the board maximum, capped by the lawful
e.r.p. limit for the configured frequency**: 22 dBm — the ceiling of
the SX1262 high-power PA and the highest value an RNode-firmware board
takes before clamping — or the sub-band's limit from ERC 70-03,
whichever is lower (`rnode::resolve_tx_power` and
`DEFAULT_TX_POWER_DBM`, `leviculum-core/src/rnode.rs:674`, capped by
`lawful_erp_dbm`, applied in both interface builders and in the
`SerialInterface` LNode path). The standalone LNode firmware's
compiled profile carries the uncapped board maximum
(`RadioConfig::eu_medium`, `leviculum-nrf/src/lora.rs:240-276`), which
is the capped resolution's own result at that profile's 869.463 MHz.

Against the rule: TX power is a local modem setting. It is never on the
wire, and no peer — Python or otherwise — learns or expects anything
about a neighbour's transmit power, so (1) and (2) are untouched. What
it gains (3) is the whole failure mode: 0 dBm is 1 mW, and a 1 mW node
has **no symptom at the node**. It boots, configures, transmits, logs
nothing unusual, and is simply not heard. 22 dBm is 158 mW. An operator
who wants 0 dBm writes `txpower = 0` and gets 0 — the resolution keeps
`None` and `Some(0)` distinct, the same way an explicit
`airtime_limit_long = 0` beats the derived lawful default.

Because the request is not preceded by a capability probe, a board
whose maximum is lower answers by clamping and echoing the clamped
value (`RNode_Firmware/RNode_Firmware.ino:861-879` — 17 dBm on an
SX127x, `PA_MAX_OUTPUT` on an SX1262 with an external PA). Confirmation
is otherwise an exact match on both stacks (ours at
`leviculum-std/src/interfaces/rnode.rs:469`, the reference at
`RNodeInterface.py:677`), so the derived default — and only the derived
default — accepts a confirmation *below* what it asked for, logs the
board's ceiling, and runs. An explicitly configured power keeps the
strict check: a board that cannot deliver a value the operator chose
must say so. A confirmation *above* the request is a mismatch either
way.

**Regulatory note (EU 863-870 MHz).** 27 dBm e.r.p. is permitted only
in 869.4-869.65 MHz (ERC Recommendation 70-03, Annex 1, sub-band
h1.7); every other listed European sub-band allows at most 25 mW
e.r.p. = 14 dBm. That is why the derived default is capped by
frequency (`rnode::lawful_erp_dbm`): a node on a community frequency
like 867.2 MHz (Rotterdam/Duffel), 867.5 (UK), 868.0 (Bern) or 868.2
(Madrid) resolves an absent `txpower` to 14 dBm, not 22. An explicit
`txpower` wins even above the cap — the operator may hold a licence
or sit in another jurisdiction — and the excess is logged. Inside
869.4-869.65 MHz, 22 dBm *conducted* stays under the 500 mW limit up
to roughly 7 dBi of antenna gain (22 + 7 - 2.15 dBd ≈ 26.9 dBm
e.r.p.); above that the operator has to set `txpower` down
explicitly, and that residual is documentation, not a runtime
warning: the stack does not know what antenna is attached, and a
warning it cannot condition on anything is a warning operators learn
to ignore. The narrowband alarm bands between the wideband sub-bands
refuse a LoRa carrier at interface build outright
(`rnode::erp_band_gap`): a 125 kHz signal cannot meet their ≤ 25 kHz
channel spacing on any power.

### Not a deviation: a class constant is not the value on the wire

An interface's `HW_MTU` in the reference is a class attribute that
looks like the answer and is not it. `TCPInterface.HW_MTU = 262144`
(`TCPInterface.py:42`) is only what the interface carries into
`interface_post_init` (`Reticulum.py:879`), which immediately runs
`optimise_mtu` (`Interface.py:198`) over the interface bitrate for
every interface with `AUTOCONFIGURE_MTU` set — TCP among them. With
`TCPServerInterface.BITRATE_GUESS` = 10 Mbps
(`TCPInterface.py:453`) the derivation lands on 8192 up to and
including Reticulum 1.5.0, and on 16384 from 1.5.2 on, where the
thresholds became `>=`. The class value never reaches the wire on
either.

We read the class constant instead of the derivation until Codeberg
#355. The measurement that closed it drove the same two Python
clients, attached to the shared instances of two relays, over one TCP
hop between the relays, swapping only which daemon the relays were:

| relay 1 | relay 2 | negotiated link MTU | largest single packet |
| --- | --- | --- | --- |
| `rnsd` 1.3.5 | `rnsd` 1.3.5 | 8192 | 8111 |
| `rnsd` 1.5.2 | `rnsd` 1.5.2 | 16384 | 16303 |
| `lnsd` (before) | `lnsd` (before) | 262144 | 262063 |
| `lnsd` (after) | `lnsd` (after) | 16384 | 16303 |

So the constant acted on the wire: a Python client on our shared
instance negotiated a link MTU 32x larger than the same client gets
from `rnsd`, and the frames actually crossed the TCP hop at that size.
A Python peer *on* the path clamps a too-large signalled MTU down to
its own on the hop it carries — the mixed rows of the same measurement
settle on 8192 — so nothing broke as long as one was there to do it.
That conditional is the semantic-compatibility risk: a Reticulum 1.5.x
peer's receive path rejects a frame longer than its own HW_MTU, so any
route change onto such a peer silently drops the traffic. Speed is
Priority 2 and does not buy that.

The lesson generalises past MTU: before adopting a reference class
attribute as a value we signal, check whether the reference derives it
at interface post-init. `UDPInterface` is the sibling case still open
— it sets `AUTOCONFIGURE_MTU = False`, so a Python peer signals no MTU
at all for a UDP hop and links stay at the base protocol MTU, while we
signal `HW_MTU = 1064`.

## Same-interface relay on shared media

Path-directed transport forwarding transmits on the next-hop
interface even when it equals the receiving interface. Same-interface
relay is NOT suppressed — it is how multi-hop works on one shared
medium. In the fundamental single-channel LoRa topology A-B-C (A↔B
and B↔C in range, A↮C), B's only route to A is back out of the very
interface C's packet arrived on; a relay that declines that hop kills
every data flow the announce flood just made possible.

The reference behaves this way on every forwarding path:

- Path-table data and link requests: the outbound interface IS the
  receiving-side path interface — `outbound_interface`
  (`Transport.py:1583`) — and `Transport.transmit`
  (`Transport.py:1635`) sends there with no receiving-interface
  guard.
- Link-table data: when next-hop and receiving interface are equal,
  "direction doesn't matter, and we simply repeat the packet"
  (`Transport.py:1651`).
- Proofs: an LRPROOF goes back out of the interface the link request
  arrived on (`Transport.py:2197`), a data proof out of the reverse
  table's receiving interface (`Transport.py:2263`) — on one shared
  channel, each is the interface the proof itself arrived on.

Loop-freedom never came from interface suppression. It comes from
transport_id addressing (only the addressed relay processes a Type2
transport packet), the hop-count limit, and packet-hash dedup —
`has_packet_hash` (`leviculum-core/src/transport.rs:2757`) drops a
repeated copy, `add_packet_hash`
(`leviculum-core/src/transport.rs:2798`) records it.

The forwarding decision lives in the media-agnostic core
(`forward_on_interface_from`,
`leviculum-core/src/transport.rs:6475`). Whether the relayed echo
needs TX spacing on a half-duplex channel is the interface's business
— see [Interface Isolation](interface-isolation.md).

Pinned by `test_pkt_journey_same_interface_relay_forward` (one relay,
one interface, forward asserted with `iface_out == iface_in`) and the
three-node `test_shared_medium_multihop_data_forward` (announce flood
A→C via B, then data C→A delivered through B's single shared
interface), both in `leviculum-core/src/transport.rs`.
