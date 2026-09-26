# Constants reference

Every constant in the [Symbol inventory](_inventory.md), grouped, with value and
citation. Values are the genuine reference values; the derived sizes are captured
in [`vectors.json`](vectors/vectors.json) `constants`.

## Application (`LXMF.py`)

| Constant | Value | Line |
|----------|-------|------|
| `APP_NAME` | "lxmf" | 1 |
| `SF_COMPRESSION` | 0x00 | 108 |

## Message states (`LXMessage.py:14-21`) — informative

`GENERATING` 0x00, `OUTBOUND` 0x01, `SENDING` 0x02, `SENT` 0x04, `DELIVERED`
0x08, `REJECTED` 0xFD, `CANCELLED` 0xFE, `FAILED` 0xFF.

## Representations and methods (`LXMessage.py:24-32`)

`UNKNOWN` 0x00, `PACKET` 0x01, `RESOURCE` 0x02. `OPPORTUNISTIC` 0x01, `DIRECT`
0x02, `PROPAGATED` 0x03, `PAPER` 0x05.

## Unverified reasons (`LXMessage.py:35-36`)

`SOURCE_UNKNOWN` 0x01, `SIGNATURE_INVALID` 0x02.

## Sizes (`LXMessage.py:39-105`)

| Constant | Value | Line |
|----------|-------|------|
| `DESTINATION_LENGTH` | 16 | 39 |
| `SIGNATURE_LENGTH` | 64 | 40 |
| `TICKET_LENGTH` | 16 | 41 |
| `TIMESTAMP_SIZE` | 8 | 60 |
| `STRUCT_OVERHEAD` | 8 | 61 |
| `LXMF_OVERHEAD` | 112 | 62 |
| `ENCRYPTED_PACKET_MDU` | 391 | 67 |
| `ENCRYPTED_PACKET_MAX_CONTENT` | 295 | 78 |
| `LINK_PACKET_MDU` | 431 | 83 |
| `LINK_PACKET_MAX_CONTENT` | 319 | 89 |
| `PLAIN_PACKET_MDU` | 464 | 93 |
| `PLAIN_PACKET_MAX_CONTENT` | 368 | 94 |
| `QR_MAX_STORAGE` | 2953 | 104 |
| `PAPER_MDU` | 2210 | 105 |
| `URI_SCHEMA` | "lxm" | 102 |

## Tickets (`LXMessage.py:48-52`)

| Constant | Value | Seconds | Line |
|----------|-------|---------|------|
| `TICKET_EXPIRY` | 21 days | 1 814 400 | 48 |
| `TICKET_GRACE` | 5 days | 432 000 | 49 |
| `TICKET_RENEW` | 14 days | 1 209 600 | 50 |
| `TICKET_INTERVAL` | 1 day | 86 400 | 51 |
| `COST_TICKET` | 0x100 (256) | — | 52 |

## Fields (`LXMF.py:8-41`)

`FIELD_EMBEDDED_LXMS` 0x01 … `FIELD_RENDERER` 0x0F; `FIELD_CUSTOM_TYPE` 0xFB,
`FIELD_CUSTOM_DATA` 0xFC, `FIELD_CUSTOM_META` 0xFD; `FIELD_NON_SPECIFIC` 0xFE,
`FIELD_DEBUG` 0xFF. See [Fields](04-fields.md) for the full table.

## Renderers and audio modes (`LXMF.py:55-92`)

`RENDERER_PLAIN` 0x00 … `RENDERER_BBCODE` 0x03. `AM_CODEC2_*` 0x01–0x09,
`AM_OPUS_*` 0x10–0x19, `AM_CUSTOM` 0xFF.

## Propagation metadata (`LXMF.py:98-104`)

`PN_META_VERSION` 0x00, `PN_META_NAME` 0x01, `PN_META_SYNC_STRATUM` 0x02,
`PN_META_SYNC_THROTTLE` 0x03, `PN_META_AUTH_BAND` 0x04, `PN_META_UTIL_PRESSURE`
0x05, `PN_META_CUSTOM` 0xFF.

## Stamps (`LXStamper.py:10-14`)

| Constant | Value | Line |
|----------|-------|------|
| `WORKBLOCK_EXPAND_ROUNDS` | 3000 | 10 |
| `WORKBLOCK_EXPAND_ROUNDS_PN` | 1000 | 11 |
| `WORKBLOCK_EXPAND_ROUNDS_PEERING` | 25 | 12 |
| `STAMP_SIZE` | 32 | 13 |
| `PN_VALIDATION_POOL_MIN_SIZE` | 256 | 14 |

## Python propagation peer (`LXMPeer.py:14-50`; Rust server out of scope)

Paths `OFFER_REQUEST_PATH` "/offer" (14), `MESSAGE_GET_PATH` "/get" (15). States
`IDLE` 0x00 … `RESOURCE_TRANSFERRING` 0x05 (17-22). Errors `ERROR_NO_IDENTITY`
0xF0, `ERROR_NO_ACCESS` 0xF1, `ERROR_INVALID_KEY` 0xF3, `ERROR_INVALID_DATA`
0xF4, `ERROR_INVALID_STAMP` 0xF5, `ERROR_THROTTLED` 0xF6, `ERROR_NOT_FOUND` 0xFD,
`ERROR_TIMEOUT` 0xFE (24-31). `STRATEGY_LAZY` 0x01, `STRATEGY_PERSISTENT` 0x02
(33-34). `MAX_UNREACHABLE` 14 days, `SYNC_BACKOFF_STEP` 12 min,
`PATH_REQUEST_GRACE` 7.5 s (39-50).

`leviculum-lxmf` implements origin-client uploads, the `/get` mailbox-client
path, and response errors. It does not expose `/offer`, these peer-sync
states/strategies, or a propagation server. Its default `/get` transfer limit
is 1000 KB, which stays below Core's current 1,048,575-byte single-segment
request/response Resource ceiling.

## Router (`LXMRouter.py:30-83`) — informative

See [Router internals](11-router-informative.md) for the full set
(`MAX_DELIVERY_ATTEMPTS`, `MESSAGE_EXPIRY`, `PROPAGATION_COST`, etc.).
