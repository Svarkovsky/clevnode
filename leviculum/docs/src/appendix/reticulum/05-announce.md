# Announce

An announce broadcasts a destination's keys so peers can address and route to it.
This section is proven by `[VEC-ANN-NORATCHET]` and `[VEC-ANN-RATCHET]`.

## Packet

An announce is an ANNOUNCE-type packet (`packet_type = 0x01`) to the announced
SINGLE destination. The context flag (header bit 5) is set when a ratchet is
included (`Destination.py:310-311`). `[VEC-ANN-NORATCHET]` flags byte `01`
(ANNOUNCE, context flag 0); `[VEC-ANN-RATCHET]` flags byte with context flag 1.

## Announce data layout

The packet data is (`Destination.py:301`):

```
public_key(64) || name_hash(10) || random_hash(10) || [ratchet(32)] || signature(64) || app_data
```

The `ratchet` field is present only when the context flag is set. Without it the
data is `64 + 10 + 10 + 64 = 148` bytes plus app_data; with it, 180 plus
app_data. `[VEC-ANN-NORATCHET]` carries 150 data bytes (148 + 2-byte app_data);
`[VEC-ANN-RATCHET]` carries 182 (180 + 2).

The `random_hash` is `get_random_hash()[0:5] ||
int(time.time()).to_bytes(5, "big")` (`Destination.py:282`): 5 random bytes and a
5-byte big-endian Unix timestamp. It makes each announce unique and lets
receivers reject replays.

## Signed data

The signature covers (`Destination.py:297-300`):

```
signature = sign( destination_hash || public_key || name_hash || random_hash || [ratchet] || app_data )
```

The **destination hash is signed but not transmitted** in the announce data; the
receiver recomputes it from the transmitted keys (below). This binds the keys to
the address without spending 16 bytes on the wire.

## Validation

A receiver validates an announce by (`Identity.py:532-634`):

1. parsing `public_key = data[:64]`, then `name_hash`, `random_hash`, optional
   `ratchet` (when the context flag is set), `signature`, and `app_data` at the
   offsets above (`Identity.py:546-564`);
2. reconstructing `signed_data` and verifying the signature against the
   transmitted public key (`Identity.py:579`);
3. recomputing `expected_destination_hash = truncated_hash(name_hash ||
   identity_hash)` and checking it matches (`Identity.py:584-585`);
4. remembering the public key, app_data, and (if present) the ratchet for future
   encryption (`Identity.py:598,618-619`).

`[VEC-ANN-NORATCHET]` and `[VEC-ANN-RATCHET]` are **frozen-injection** vectors:
under pinned randomness and time the announce reproduces byte for byte, and the
genuine `validate_announce` returns true. An implementation MUST reproduce the
signed-data order and the destination-hash recomputation, or a Python peer
rejects the announce.

## Path response

A path response is an announce re-emitted with context `PATH_RESPONSE` (0x0B)
rather than `NONE`, in reply to a path request (see [Transport](09-transport.md)).
The announce data is identical; only the packet context differs.
