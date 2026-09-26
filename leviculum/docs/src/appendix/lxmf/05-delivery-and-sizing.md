# Delivery methods and sizing

## Method and representation constants

| Method | Value | Citation |
|--------|-------|----------|
| `OPPORTUNISTIC` | 0x01 | `LXMessage.py:29` |
| `DIRECT` | 0x02 | `LXMessage.py:30` |
| `PROPAGATED` | 0x03 | `LXMessage.py:31` |
| `PAPER` | 0x05 | `LXMessage.py:32` |

| Representation | Value | Citation |
|----------------|-------|----------|
| `UNKNOWN` | 0x00 | `LXMessage.py:24` |
| `PACKET` | 0x01 | `LXMessage.py:25` |
| `RESOURCE` | 0x02 | `LXMessage.py:26` |

The representation records whether the message goes out as a single Reticulum
Packet or as a Reticulum Resource (multi-packet transfer). PAPER uses neither.

## Selection algorithm

`pack()` sets `method` and `representation` from `desired_method` and the content
size (`LXMessage.py:390-458`). The normative rules:

1. If no method is desired, default to `DIRECT` (`LXMessage.py:392-393`).
2. **OPPORTUNISTIC** is valid only for SINGLE or PLAIN destinations. If the
   content size exceeds `ENCRYPTED_PACKET_MAX_CONTENT` (295) for a SINGLE
   destination, the reference falls back to `DIRECT` (`LXMessage.py:397-401`).
   Otherwise representation is `PACKET` (`LXMessage.py:404-415`). For PLAIN
   destinations the limit is `PLAIN_PACKET_MAX_CONTENT` (368).
3. **DIRECT**: if content size `<= LINK_PACKET_MAX_CONTENT` (319), representation
   is `PACKET`; otherwise `RESOURCE` (`LXMessage.py:417-424`).
4. **PROPAGATED**: the message is wrapped into the propagation envelope (see
   [Propagation](10-propagation.md)); if the envelope size `<=
   LINK_PACKET_MAX_CONTENT` it is a `PACKET`, otherwise a `RESOURCE`
   (`LXMessage.py:423-441`).
5. **PAPER**: if the encrypted paper form `<= PAPER_MDU` (2210) the representation
   is paper; otherwise `pack()` raises (`LXMessage.py:443-455`).

`content_size` is the serialized-payload measure from
[Identifiers and sizes](02-identifiers-and-sizes.md) (`LXMessage.py:385`).

An implementation MUST apply the same thresholds so that a message a Python peer
would send as a single packet is not sent as a resource (and vice versa), since
the on-air framing differs.

## On-air forms

| Method | Representation | On-air bytes | Citation |
|--------|----------------|--------------|----------|
| OPPORTUNISTIC | PACKET | `packed[16:]` (destination hash omitted) | `LXMessage.py:631` |
| DIRECT | PACKET | full `packed` over a Link | `LXMessage.py:633` |
| DIRECT | RESOURCE | full `packed` as a Resource over a Link | `LXMessage.py:650-651` |
| PROPAGATED | PACKET/RESOURCE | `propagation_packed` to a propagation node | `LXMessage.py:634-635,652-653` |
| PAPER | (paper) | `lxm://` URI or QR | `LXMessage.py:687-702` |

The opportunistic packet omits the leading 16-byte destination hash because the
Reticulum packet header already addresses the destination; the receiver
reconstructs the full message by prepending the known destination hash. This is
proven by `[VEC-DLV-OPP]` (`on_air_hex == packed[16:]`) and the direct full-bytes
form by `[VEC-DLV-DIRECT]`.
