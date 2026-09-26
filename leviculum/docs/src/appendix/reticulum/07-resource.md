# Resource

A resource is a reliable, segmented transfer over a link for data larger than a
single packet. This section is proven by `[VEC-RES-ADV]` and `[VEC-RES-PROOF]`.
Window adaptation and timeout scheduling are informative.

## Advertisement

The sender advertises a resource with a RESOURCE_ADV packet (context 0x02)
carrying a msgpack dictionary (`ResourceAdvertisement.pack`, `Resource.py:1333-1355`):

| Key | Meaning |
|-----|---------|
| `t` | transfer size (encrypted bytes) |
| `d` | total uncompressed data size |
| `n` | number of parts |
| `h` | resource hash (32) |
| `r` | random hash (4) |
| `o` | original (first-segment) hash (32) |
| `i` | segment index |
| `l` | total segments |
| `q` | associated request id, or nil |
| `f` | flags byte |
| `m` | hashmap segment (4-byte `MAPHASH_LEN` entries) |

The flags byte is (`Resource.py:1307`):

```
f = (has_metadata<<5) | (is_response<<4) | (is_request<<3) | (split<<2) | (compressed<<1) | encrypted
```

`[VEC-RES-ADV]` is a **computed** vector (a live advertisement needs a Resource
over a link): the dictionary with `compressed` and `encrypted` set yields flags
`03` and packs to 146 bytes under the genuine msgpack. `MAPHASH_LEN = 4` and
`RANDOM_HASH_SIZE = 4`.

## Parts, requests, and hashmap

- A **part** is a RESOURCE packet (context 0x01) carrying up to one SDU of
  pre-encrypted data.
- The receiver requests parts with a RESOURCE_REQ packet (context 0x03) whose
  first byte is the hashmap status (`HASHMAP_IS_EXHAUSTED = 0xFF` /
  `HASHMAP_IS_NOT_EXHAUSTED = 0x00`), optionally followed by the last received
  map index and the requested 4-byte part hashes.
- The sender extends the hashmap with a RESOURCE_HMU packet (context 0x04)
  carrying `resource_hash || msgpack([segment_index, hashmap_bytes])`.

The hashmap lets the receiver request parts by hash, and the sender stream
hashes as the window advances. The sliding window sizes (`WINDOW`, `WINDOW_MIN`,
`WINDOW_MAX*`) and the part/proof timeout factors are informative.

## Proof and cancellation

On completion the receiver assembles the parts, verifies integrity, and the
sender sends a RESOURCE_PRF packet (context 0x05, unencrypted) carrying
(`Resource.py:755-756`):

```
proof      = full_hash(data || resource_hash)
proof_data = resource_hash(32) || proof(32)
```

a single SHA-256 over the assembled data concatenated with the resource hash,
prefixed by the resource hash. `validate_proof` accepts when `proof_data` is 64
bytes and its second half matches the expected proof (`Resource.py:782-786`).
`[VEC-RES-PROOF]` records this construction. Either party may abort with
RESOURCE_ICL (0x06, initiator) or RESOURCE_RCL (0x07, receiver).

## Compression and segmentation

A resource MAY be bz2-compressed (the `compressed` flag) and, when larger than a
single segment, split into `total_segments` segments chained by the `original`
hash. The maximum efficient single-segment size and metadata limits are
implementation guidance.
