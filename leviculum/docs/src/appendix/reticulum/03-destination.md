# Destination

A destination is an addressable endpoint. This section is proven by
`[VEC-DEST-HASH]`.

## Naming and hashing

A destination name is the dotted concatenation of an app name and aspects, e.g.
`test.vec` (`expand_name`, `Destination.py:96`). The hashes are
(`Destination.py:116-141`):

```
name_hash        = full_hash(app_name [. aspect ...])[:10]      # 80 bits
destination_hash = truncated_hash(name_hash || identity_hash)   # 16 bytes
```

`[VEC-DEST-HASH]` for app `test`, aspect `vec`, identity hash
`069092a03c194639207219dd05f9c840`: name hash `9da53eec82a28ce2f2e9` (10 bytes),
destination hash `07d4541d4fdc0abfacc9364fdf979ee1` (16 bytes). An implementation
MUST derive these identically, since the destination hash is the on-wire address
and is recomputed by every receiver of an announce.

## Types

| Type | Value | Encryption | Citation |
|------|-------|------------|----------|
| `SINGLE` | 0x00 | per-recipient ECIES token to the identity (or ratchet) | `Destination.py:63` |
| `GROUP` | 0x01 | symmetric (shared key, not auto-distributed) | `:64` |
| `PLAIN` | 0x02 | none (cleartext) | `:65` |
| `LINK` | 0x03 | per-link session key | `:66` |

The type occupies bits 3-2 of the packet header (see [Packet format](04-packet.md)).

## Direction and proof strategy

- Direction: `IN = 0x11`, `OUT = 0x12` (`Destination.py:79-80`). Only `IN` SINGLE
  destinations may be announced (`Destination.py:251-255`).
- Proof strategy: `PROVE_NONE = 0x21`, `PROVE_APP = 0x22`, `PROVE_ALL = 0x23`
  (`Destination.py:69-71`) controls whether the destination automatically returns
  delivery proofs (see [Packet format](04-packet.md) proofs).

## Encryption and decryption

`Destination.encrypt`/`decrypt` (`Destination.py:585,611`) delegate to the
identity's token for SINGLE destinations, applying the current ratchet when
enabled. This is the path LXMF and link setup use for SINGLE-addressed payloads.
