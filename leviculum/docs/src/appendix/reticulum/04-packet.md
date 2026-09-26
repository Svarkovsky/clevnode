# Packet format

This section is normative and proven by `[VEC-PKT-PLAIN]`, `[VEC-PKT-ENC]`, and
`[VEC-PKT-HEADER2]`.

## Header bitfield

The first byte is a bitfield (`Packet.get_packed_flags`, `Packet.py:169-175`;
decoded in `unpack`, `:247-251`):

```
bit 7   IFAC flag        (set by the interface, not by pack; see Framing/IFAC)
bit 6   header type      0 = HEADER_1, 1 = HEADER_2
bit 5   context flag     context-specific; set when an announce carries a ratchet
bit 4   transport type   0 = BROADCAST, 1 = TRANSPORT
bits3-2 destination type 00 SINGLE, 01 GROUP, 10 PLAIN, 11 LINK
bits1-0 packet type      00 DATA, 01 ANNOUNCE, 10 LINKREQUEST, 11 PROOF
```

`packed_flags = (header_type<<6) | (context_flag<<5) | (transport_type<<4) |
(destination_type<<2) | packet_type`.

## HEADER_1 layout

```
flags(1) || hops(1) || destination_hash(16) || context(1) || data
offset 0     1          2                       18           19
```

(`Packet.pack`, `Packet.py:177-239`; `unpack`, `:262-264`). The fixed header is
`HEADER_MINSIZE = 19` bytes (`Reticulum.py:147`).

`[VEC-PKT-PLAIN]` is a PLAIN HEADER_1 DATA packet,
`0800fc0910664040482cd653166c8f225520006869`:

```
08                                 flags: PLAIN(0x08) + DATA, HEADER_1, BROADCAST
00                                 hops
fc0910664040482cd653166c8f225520   destination_hash(16)
00                                 context (NONE)
6869                               data = "hi"
```

The flags decode to ifac 0, header_type 0, context_flag 0, transport_type 0,
destination_type 2 (PLAIN), packet_type 0 (DATA). `[VEC-PKT-ENC]` is the SINGLE
encrypted equivalent (flags `00`); under injection its encrypted body reproduces
byte for byte.

## HEADER_2 layout

When the header type bit is set, a 16-byte transport id precedes the destination
hash (`Packet.py:255-259`):

```
flags(1) || hops(1) || transport_id(16) || destination_hash(16) || context(1) || data
offset 0     1          2                   18                      34            35
```

`HEADER_MAXSIZE = 35` bytes (`Reticulum.py:148`). `[VEC-PKT-HEADER2]` shows the
constructed layout `4000 a0..af b0..bf 00 64617461`: flags `40` (header_type 1),
transport id `a0a1…af`, destination hash `b0b1…bf`. HEADER_2 is emitted by
transport nodes forwarding toward a known next hop.

## Sizes

```
MTU                = 500                              (Reticulum.py:93)
HEADER_MAXSIZE     = 2 + (128/8)*2 + 1 = 35           (Reticulum.py:148)
MDU                = 500 - 35 - 1 = 464               (Reticulum.py:152)
Packet.ENCRYPTED_MDU = 383                            (Packet.py:106)
Packet.PLAIN_MDU     = MDU = 464                      (Packet.py:110)
```

## Context bytes

The context byte (offset 18 for HEADER_1) selects the packet's role within its
type. Full set (`Packet.py:72-92`):

| Hex | Name | Used by |
|-----|------|---------|
| 0x00 | NONE | generic data |
| 0x01 | RESOURCE | resource part |
| 0x02 | RESOURCE_ADV | resource advertisement |
| 0x03 | RESOURCE_REQ | resource part request |
| 0x04 | RESOURCE_HMU | resource hashmap update |
| 0x05 | RESOURCE_PRF | resource proof |
| 0x06 | RESOURCE_ICL | resource initiator cancel |
| 0x07 | RESOURCE_RCL | resource receiver cancel |
| 0x08 | CACHE_REQUEST | cache request |
| 0x09 | REQUEST | link request (application) |
| 0x0A | RESPONSE | link response |
| 0x0B | PATH_RESPONSE | transport path response |
| 0x0C | COMMAND | command |
| 0x0D | COMMAND_STATUS | command status |
| 0x0E | CHANNEL | channel data |
| 0xFA | KEEPALIVE | link keepalive |
| 0xFB | LINKIDENTIFY | link identification |
| 0xFC | LINKCLOSE | link close |
| 0xFD | LINKPROOF | (deprecated) |
| 0xFE | LRRTT | link RTT |
| 0xFF | LRPROOF | link request proof |

## Proofs

A delivery proof is a PROOF packet over the original packet hash
(`Packet.get_hashable_part`, `:355`; `validate_proof`, `:498`). Two forms exist:
explicit (`packet_hash(32) || signature(64)`, `EXPL_LENGTH = 96`) and implicit
(`signature(64)`, `IMPL_LENGTH = 64`). The reference currently emits explicit
proofs (`Link.prove_packet`, `Link.py:383`). Whether a destination proves is
governed by its proof strategy (see [Destination](03-destination.md)).
