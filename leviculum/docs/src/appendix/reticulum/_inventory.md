# Reticulum reference symbol inventory (frozen)

Frozen ground-truth enumeration of the wire surfaces in the vendored Python RNS
reference. The specification is written against it; the
[Coverage ledger](12-coverage-ledger.md) maps every entry to a section and a
proof. To refresh, re-enumerate the source at the pinned commit and diff; a new
unclassified symbol is a coverage gap.

## Pin

| Component | Version | Submodule commit |
|-----------|---------|------------------|
| Reticulum (`reference/Reticulum`) | RNS 1.3.5 | `d5e62d4e15c5fe2e170f7bd9e120551671f21a27` |

## Classification key

- **N** normative: crosses the wire or is observable by a peer; specified exactly
  and proven.
- **I** informative: internal behaviour an implementation may diverge on.
- **X** out of scope: interface drivers beyond framing, daemon/CLI, scaffolding.

---

## Reticulum.py — system constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| `MTU` | 500 | 93 | N |
| `MDU` | 464 | 152 | N |
| `TRUNCATED_HASHLENGTH` | 128 | 145 | N |
| `HEADER_MINSIZE` | 19 | 147 | N |
| `HEADER_MAXSIZE` | 35 | 148 | N |
| `IFAC_MIN_SIZE` | 1 | 149 | N |
| `IFAC_SALT` | 32-byte hex | 150 | N |

## Cryptography/* 

| File | Symbol / method | Line | Class |
|------|-----------------|------|-------|
| Token.py | `Token` (modified Fernet), `TOKEN_OVERHEAD=48`, `encrypt`, `decrypt`, `verify_hmac` | 50,77,87,100 | N |
| X25519.py | `X25519PrivateKey`/`PublicKey`, `generate`, `exchange` | 126,139 | N |
| Ed25519.py | `Ed25519PrivateKey`/`PublicKey`, `sign`, `verify` | 53,69 | N |
| AES.py | `AES_128_CBC`, `AES_256_CBC` (16-byte IV, PKCS7) | — | N |
| HMAC.py | RFC 2104 HMAC-SHA256, `new`, `digest` | — | N |
| HKDF.py | `hkdf(length, derive_from, salt, context)` | 35 | N |
| PKCS7.py | block size 16 padding | — | N |

## Identity.py (980 lines) — class `Identity`

### Constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| `KEYSIZE` | 512 | 59 | N |
| `RATCHETSIZE` | 256 | 64 | N |
| `RATCHET_EXPIRY` | 2592000 | 69 | I |
| `TOKEN_OVERHEAD` | 48 | 77 | N |
| `HASHLENGTH` | 256 | 80 | N |
| `SIGLENGTH` | 512 | 81 | N |
| `NAME_HASH_LENGTH` | 80 | 83 | N |
| `TRUNCATED_HASHLENGTH` | 128 | 84 | N |
| `DERIVED_KEY_LENGTH` | 64 | 90 | N |

### Methods

| Method | Line | Class |
|--------|------|-------|
| `full_hash` / `truncated_hash` | 373/383 | N |
| `get_random_hash` | 393 | N |
| `_get_ratchet_id` / `_ratchet_public_bytes` / `_generate_ratchet` | 417-425 | N |
| `validate_announce` | 532 | N |
| `encrypt` / `decrypt` | 827/872 | N |
| `sign` / `validate` | 931/948 | N |
| `update_hashes` / `get_public_key` / `get_private_key` | 808/757/750 | N |
| `recall` / `remember` | — | I (resolution) |

## Destination.py (680 lines) — class `Destination`

### Constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| `SINGLE/GROUP/PLAIN/LINK` | 0x00-0x03 | 63-66 | N |
| `PROVE_NONE/APP/ALL` | 0x21-0x23 | 69-71 | N |
| `IN/OUT` | 0x11/0x12 | 79-80 | N |
| `RATCHET_COUNT` | 512 | 85 | I |
| `RATCHET_INTERVAL` | 1800 | 90 | I |
| `PR_TAG_WINDOW` | 30 | 83 | I |

### Methods

| Method | Line | Class |
|--------|------|-------|
| `expand_name` | 96 | N |
| `hash` / `hash_from_name_and_identity` | 116/141 | N |
| `announce` | 243 | N |
| `encrypt` / `decrypt` | 585/611 | N |

## Packet.py (603 lines) — class `Packet`, `PacketReceipt`

### Constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| packet types `DATA/ANNOUNCE/LINKREQUEST/PROOF` | 0x00-0x03 | 60-63 | N |
| header types `HEADER_1/HEADER_2` | 0x00/0x01 | 67-68 | N |
| context bytes `NONE..LRPROOF` | 0x00-0xFF | 72-92 | N |
| `FLAG_SET/FLAG_UNSET` | 0x01/0x00 | 95-96 | N |
| `ENCRYPTED_MDU` | 383 | 106 | N |
| `PLAIN_MDU` | =MDU (464) | 110 | N |
| `PacketReceipt FAILED/SENT/DELIVERED/CULLED` | 0,1,2,0xFF | 408-415 | I |
| `EXPL_LENGTH/IMPL_LENGTH` | 96/64 | — | N (proofs) |

### Methods

| Method | Line | Class |
|--------|------|-------|
| `get_packed_flags` | 169 | N |
| `pack` / `unpack` | 177/242 | N |
| `get_hashable_part` | 355 | N |
| `validate_proof_packet` / `validate_proof` | 443/498 | N |

## Link.py (1538 lines) — class `Link`

### Constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| `ECPUBSIZE` | 64 | — | N |
| `KEYSIZE` | 32 | — | N |
| `LINK_MTU_SIZE` | 3 | — | N |
| `MTU_BYTEMASK` | 0x1FFFFF | — | N |
| `MODE_BYTEMASK` | 0xE0 | — | N |
| `MODE_AES256_CBC` | 0x01 | — | N |
| states `PENDING..CLOSED` | 0x00-0x04 | — | I |
| `KEEPALIVE` / `STALE_TIME` | 360/720 | — | I |

### Methods

| Method | Line | Class |
|--------|------|-------|
| `link_id_from_lr_packet` / `set_link_id` | 340/349 | N |
| `handshake` | 353 | N |
| `prove` / `validate_proof` | 371/396 | N |
| `signalling_bytes` / `mtu_from_lp_packet` / `mode_from_lp_packet` | 148+ | N |
| `identify` | 459 | N |
| `send_keepalive` | 848 | N |
| `get_salt` / `get_context` | 643/646 | N |
| watchdog, RTT scheduling, teardown | — | I |

### Context-byte payloads (N)

`LRPROOF` 0xFF (sig(64)+eph_pub(32)+signalling(3)), `LRRTT` 0xFE (msgpack float),
`LINKIDENTIFY` 0xFB (pub(32)+sig(64)), `LINKCLOSE` 0xFC (link_id), `KEEPALIVE`
0xFA (single byte 0xFF), `REQUEST`/`RESPONSE` 0x09/0x0A.

## Resource.py (1380 lines) — `Resource`, `ResourceAdvertisement`

### Constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| `MAPHASH_LEN` | 4 | — | N |
| `RANDOM_HASH_SIZE` | 4 | — | N |
| `HASHMAP_IS_EXHAUSTED/NOT` | 0xFF/0x00 | — | N |
| `WINDOW*`, `*_TIMEOUT_FACTOR`, `MAX_RETRIES` | various | — | I |
| `OVERHEAD` (advertisement) | 134 | 1235 | N |
| status `NONE..CORRUPT` | 0x00-0x08 | — | I |

### Methods

| Method | Line | Class |
|--------|------|-------|
| `ResourceAdvertisement.__init__` / `pack` / `unpack` | 1278/1333 | N |
| `hashmap_update` / `request` / `receive_part` | — | N (wire) / I (scheduling) |
| `prove` / `validate_proof` | 752/782 | N |
| `advertise` / `assemble` / window adaptation | 508/672 | I |

Context bytes `RESOURCE` 0x01, `RESOURCE_ADV` 0x02, `RESOURCE_REQ` 0x03,
`RESOURCE_HMU` 0x04, `RESOURCE_PRF` 0x05, `RESOURCE_ICL` 0x06, `RESOURCE_RCL`
0x07 (Packet.py:73-79) — N.

## Channel.py (738 lines) / Buffer.py (371 lines)

| Symbol | Line | Class |
|--------|------|-------|
| `Envelope.pack`/`unpack` (`>HHH` type,seq,len) | 192/179 | N |
| `StreamDataMessage` header (14-bit id, compressed, eof) | Buffer 80-92 | N |
| `SMT_STREAM_DATA` 0xff00, `STREAM_ID_MAX` 0x3fff, `OVERHEAD` 8 | — | N |
| `MessageState`, `CEType` enums | — | I |
| channel sequencing/retransmission | — | I |

## Transport.py (3585 lines) — class `Transport`

### Normative surfaces

| Symbol | Line | Class |
|--------|------|-------|
| `transmit` (IFAC masking) | 1051 | N |
| `inbound` (IFAC unmasking) | 1398 | N |
| `request_path` (path request packet) | 2771 | N |
| `path_request_handler` / path response (`PATH_RESPONSE` rebroadcast) | 2866/2943 | N |

### Informative

`PATHFINDER_*`, `AP_PATH_TIME`, `ROAMING_PATH_TIME`, `LOCAL_REBROADCASTS_MAX`,
`PATH_REQUEST_*` (50-83); the path/announce/link/reverse/tunnel tables and their
`IDX_*` shapes (3547-3586); announce retransmission timing and jitter; dedup;
table culling in `jobs` (508). All **I**.

---

## Out of scope (X)

Interface drivers under `RNS/Interfaces/*` beyond the HDLC/KISS framing
documented in [Framing and IFAC](10-framing-ifac.md); the daemon/CLI tooling;
shared-instance IPC.
