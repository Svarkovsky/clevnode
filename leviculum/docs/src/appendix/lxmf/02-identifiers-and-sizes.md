# Identifiers and sizes

All sizes below are the genuine class attributes of the reference, captured in
[`vectors.json`](vectors/vectors.json) `constants`.

## Identifiers

| Identifier | Width | Definition | Citation |
|------------|-------|------------|----------|
| Destination hash | 16 | Reticulum SINGLE destination hash of `lxmf/delivery` | `LXMessage.py:39` |
| Source hash | 16 | sender's `lxmf/delivery` destination hash | `LXMessage.py:380-381` |
| Signature | 64 | Ed25519 over the signed part | `LXMessage.py:40` |
| Message hash / message-id | 32 | `full_hash(dest || src || msgpack(payload))` | `LXMessage.py:365-366` |
| Transient-id | 32 | `full_hash(lxmf_data)` for propagation | `LXMessage.py:431` |
| Stamp | 32 | proof-of-work nonce | `LXStamper.py:13` |
| Ticket | 16 | shared secret for stamp shortcut | `LXMessage.py:41` |

The message hash and message-id are the same value (`LXMessage.py:366`); this
document uses "message-id". `[VEC-MSG-1]` shows `message_id_hex == hash_hex`.

## Overhead and packet sizes

The fixed overhead and the three single-packet content limits are derived
constants. The derivations (MUST evaluate to these values):

```
TIMESTAMP_SIZE   = 8                                  (LXMessage.py:60)
STRUCT_OVERHEAD  = 8                                  (LXMessage.py:61)
LXMF_OVERHEAD    = 2*16 + 64 + 8 + 8 = 112            (LXMessage.py:62)

ENCRYPTED_PACKET_MDU         = RNS.Packet.ENCRYPTED_MDU + 8 = 391   (LXMessage.py:67)
ENCRYPTED_PACKET_MAX_CONTENT = 391 - 112 + 16            = 295      (LXMessage.py:78)

LINK_PACKET_MDU              = RNS.Link.MDU              = 431       (LXMessage.py:83)
LINK_PACKET_MAX_CONTENT      = 431 - 112                = 319       (LXMessage.py:89)

PLAIN_PACKET_MDU             = RNS.Packet.PLAIN_MDU      = 464       (LXMessage.py:93)
PLAIN_PACKET_MAX_CONTENT     = 464 - 112 + 16           = 368       (LXMessage.py:94)

PAPER_MDU = ((2953 - (3 + 3)) * 6) // 8                 = 2210      (LXMessage.py:105)
```

`QR_MAX_STORAGE = 2953` (`LXMessage.py:104`); the `+ 16` terms restore the
destination hash that is excluded from `LXMF_OVERHEAD` accounting for the
encrypted single-packet forms. These are the thresholds the delivery-method
selector compares against; see [Delivery methods and sizing](05-delivery-and-sizing.md).

## Content size

The reference defines the content size of a packed message as

```
content_size = len(packed_payload) - TIMESTAMP_SIZE - STRUCT_OVERHEAD
             = len(packed_payload) - 16
```

(`LXMessage.py:385`). This is the value compared against the limits above. Note
it is computed from the serialized payload length, not from the raw content
bytes, so msgpack framing and the title and fields count toward it.
