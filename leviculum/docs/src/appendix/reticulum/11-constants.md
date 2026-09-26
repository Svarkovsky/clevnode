# Constants reference

Grouped constants with values and citations. Derived sizes are captured in
[`vectors.json`](vectors/vectors.json) `constants`.

## System (`Reticulum.py`)

| Constant | Value | Line |
|----------|-------|------|
| `MTU` | 500 | 93 |
| `MDU` | 464 | 152 |
| `TRUNCATED_HASHLENGTH` | 128 bits | 145 |
| `HEADER_MINSIZE` | 19 | 147 |
| `HEADER_MAXSIZE` | 35 | 148 |
| `IFAC_MIN_SIZE` | 1 | 149 |

## Identity (`Identity.py`)

| Constant | Value | Line |
|----------|-------|------|
| `KEYSIZE` | 512 bits | 59 |
| `RATCHETSIZE` | 256 bits | 64 |
| `TOKEN_OVERHEAD` | 48 | 77 |
| `HASHLENGTH` | 256 bits | 80 |
| `SIGLENGTH` | 512 bits | 81 |
| `NAME_HASH_LENGTH` | 80 bits | 83 |
| `DERIVED_KEY_LENGTH` | 64 | 90 |

## Destination (`Destination.py`)

`SINGLE` 0x00, `GROUP` 0x01, `PLAIN` 0x02, `LINK` 0x03 (63-66); `PROVE_NONE` 0x21,
`PROVE_APP` 0x22, `PROVE_ALL` 0x23 (69-71); `IN` 0x11, `OUT` 0x12 (79-80).

## Packet (`Packet.py`)

Packet types `DATA` 0x00, `ANNOUNCE` 0x01, `LINKREQUEST` 0x02, `PROOF` 0x03
(60-63). Header types `HEADER_1` 0x00, `HEADER_2` 0x01 (67-68). `FLAG_SET` 0x01,
`FLAG_UNSET` 0x00 (95-96). `ENCRYPTED_MDU` 383 (106), `PLAIN_MDU` 464 (110).
Context bytes 0x00-0xFF (72-92) — full table in [Packet format](04-packet.md).
Proof lengths `EXPL_LENGTH` 96, `IMPL_LENGTH` 64.

## Link (`Link.py`)

`ECPUBSIZE` 64, `KEYSIZE` 32, `LINK_MTU_SIZE` 3, `MTU_BYTEMASK` 0x1FFFFF,
`MODE_BYTEMASK` 0xE0, `MODE_AES256_CBC` 0x01. States `PENDING` 0x00 ..
`CLOSED` 0x04 (informative). Context bytes `KEEPALIVE` 0xFA, `LINKIDENTIFY` 0xFB,
`LINKCLOSE` 0xFC, `LINKPROOF` 0xFD, `LRRTT` 0xFE, `LRPROOF` 0xFF.

## Resource (`Resource.py`)

`MAPHASH_LEN` 4, `RANDOM_HASH_SIZE` 4, `HASHMAP_IS_EXHAUSTED` 0xFF,
`HASHMAP_IS_NOT_EXHAUSTED` 0x00, advertisement `OVERHEAD` 134 (1235). Context
bytes `RESOURCE` 0x01 .. `RESOURCE_RCL` 0x07 (Packet.py:73-79). Window sizes and
timeout factors are informative.

## Channel / Buffer (`Channel.py`, `Buffer.py`)

`SMT_STREAM_DATA` 0xff00, `STREAM_ID_MAX` 0x3fff, combined `OVERHEAD` 8.

## Transport (`Transport.py`) — informative

`PATHFINDER_M` 128, `PATHFINDER_R` 1, `PATHFINDER_G` 5 s, `PATHFINDER_RW` 0.5 s,
`PATHFINDER_E` 7 days, `AP_PATH_TIME` 1 day, `ROAMING_PATH_TIME` 6 h,
`LOCAL_REBROADCASTS_MAX` 2, `PATH_REQUEST_TIMEOUT` 15 s, `PATH_REQUEST_MI` 20 s
(50-83). Table index constants `IDX_*` (3547-3586).
