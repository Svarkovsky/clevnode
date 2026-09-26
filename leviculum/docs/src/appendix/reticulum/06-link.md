# Link

A link is an ephemeral, forward-secret session between two destinations,
established by an ECDH handshake. This section is proven by `[VEC-LINK]`.

## Link request

The initiator sends a LINKREQUEST packet (`packet_type = 0x02`) whose data is
(`Link.py:308-317`):

```
ephemeral_X25519_public(32) || ephemeral_Ed25519_public(32) || signalling(3)
```

`ECPUBSIZE = 64` (the two public keys). The 3-byte `signalling` field encodes the
proposed link MTU (21 bits) and mode (3 bits): `(mtu & 0x1FFFFF) | ((mode<<5 &
0xE0)<<16)`, packed big-endian, low 3 bytes (`Link.signalling_bytes`,
`Link.py:148-151`). The only enabled mode is `MODE_AES256_CBC = 0x01`.

## Link id

```
hashable = packet.get_hashable_part()        # masked-flags byte || addressing || data
if len(packet.data) > ECPUBSIZE:             # strip trailing signalling bytes
    hashable = hashable[:-(len(packet.data) - ECPUBSIZE)]
link_id  = truncated_hash(hashable)          # 16 bytes
```

(`Link.link_id_from_lr_packet`, `Link.py:340-347`): the hashable part is trimmed by
the number of bytes the request data exceeds the 64-byte key block (i.e. the
signalling bytes) before hashing.
`[VEC-LINK]` link id `4725ac1375601d182afec3610f019b25`. The link id replaces the
destination hash in the addressing of all subsequent link packets, and is also
the link's salt (below).

## Proof and handshake

The responder replies with a PROOF packet, context `LRPROOF` (0xFF), data
(`Link.py:371-377`):

```
signature(64) || ephemeral_X25519_public(32) || signalling(3)
```

where `signature = sign( link_id || responder_eph_X25519_pub ||
responder_eph_Ed25519_pub || signalling )` (`Link.py:373`). The initiator
validates it against the destination's known identity (`Link.py:417-420`).

Both sides then derive the session key (`Link.handshake`, `Link.py:353-366`):

```
shared      = own_ephemeral_private.exchange(peer_ephemeral_public)   # X25519 ECDH
session_key = hkdf(length=64, derive_from=shared, salt=link_id, context=None)
```

64 bytes for `MODE_AES256_CBC` (32 key + 32 HMAC). `get_salt()` returns the link
id and `get_context()` returns `None` (`Link.py:643,646`). `[VEC-LINK]` proves
the ECDH is symmetric (`ecdh_agreement = true`, both sides compute the same
shared secret) and records the resulting 64-byte session key
`569ac51a07fb242f…`. An implementation MUST derive the link id and session key
identically.

## Link operations (context bytes)

Once active, link packets are DATA packets addressed by link id, encrypted with
the session key, distinguished by context byte:

| Context | Hex | Encrypted | Payload | Citation |
|---------|-----|-----------|---------|----------|
| `LRPROOF` | 0xFF | no | sig(64) + eph_pub(32) + signalling(3) | `Link.py:371-377` |
| `LRRTT` | 0xFE | yes | msgpack(float rtt) | `Link.py:440` |
| `LINKIDENTIFY` | 0xFB | yes | identity_public(32) + sign(link_id\|\|public)(64) | `Link.py:459-471` |
| `KEEPALIVE` | 0xFA | no | single byte `0xFF` | `Link.py:848-851` |
| `LINKCLOSE` | 0xFC | yes | link_id | — |

After proof, the initiator measures RTT and sends an `LRRTT` packet; either side
MAY `identify` (prove an identity over the link) by sending a `LINKIDENTIFY`
packet. Keepalive cadence, the stale/close watchdog, and MTU discovery are
informative.
