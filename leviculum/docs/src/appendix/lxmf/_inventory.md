# LXMF reference symbol inventory (frozen)

This file is the historical, frozen enumeration of every symbol and wire
surface in the Python LXMF 0.9.6 audit baseline. The coverage ledger
([Coverage ledger](13-coverage-ledger.md)) maps every entry here to a
specification section and a proof. The active implementation and canonical
fixture are pinned separately to LXMF 1.1.0 (`795fdaa`).

Do not edit by hand to reflect wishful coverage. To refresh, re-enumerate the
source at the pinned commit and diff. A new symbol that appears unclassified is
a coverage gap.

The inventory is broader than the Rust implementation. In particular,
`/offer`, propagation-node hosting/storage, and `LXMPeer` synchronisation are
reference-only; `leviculum-lxmf` implements origin uploads and the recipient
`/get` client.

## Pin

| Component | Version | Submodule commit |
|-----------|---------|------------------|
| LXMF (`reference/LXMF`) | 0.9.6 | `8499729024a4cddfceb47ca07188bb5b1d11d179` |
| Reticulum (`reference/Reticulum`) | RNS 1.3.5 | `d5e62d4e15c5fe2e170f7bd9e120551671f21a27` |

Reference files under `reference/LXMF/LXMF/`: `LXMessage.py`, `LXMF.py`,
`LXMRouter.py`, `LXMPeer.py`, `LXStamper.py`, `Handlers.py`, `_version.py`,
`Utilities/lxmd.py`.

## Classification key

- **N** normative: crosses the wire or is observable by a Python peer; must be
  specified exactly and proven.
- **I** informative: internal behaviour an implementer may diverge on without
  breaking interop; described, not byte-proven.
- **X** out of scope: daemon, CLI, build or test scaffolding.

---

## LXMessage.py (827 lines) — class `LXMessage` (line 13)

### Constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| state `GENERATING/OUTBOUND/SENDING/SENT/DELIVERED/REJECTED/CANCELLED/FAILED` | 0x00,0x01,0x02,0x04,0x08,0xFD,0xFE,0xFF | 14-21 | I |
| representation `UNKNOWN/PACKET/RESOURCE` | 0x00,0x01,0x02 | 24-26 | N |
| method `OPPORTUNISTIC/DIRECT/PROPAGATED/PAPER` | 0x01,0x02,0x03,0x05 | 29-32 | N |
| unverified `SOURCE_UNKNOWN/SIGNATURE_INVALID` | 0x01,0x02 | 35-36 | N |
| `DESTINATION_LENGTH` | 16 | 39 | N |
| `SIGNATURE_LENGTH` | 64 | 40 | N |
| `TICKET_LENGTH` | 16 | 41 | N |
| `TICKET_EXPIRY/GRACE/RENEW/INTERVAL` | 21d/5d/14d/1d | 48-51 | N |
| `COST_TICKET` | 0x100 | 52 | N |
| `TIMESTAMP_SIZE` | 8 | 60 | N |
| `STRUCT_OVERHEAD` | 8 | 61 | N |
| `LXMF_OVERHEAD` | 112 | 62 | N |
| `ENCRYPTED_PACKET_MDU` | derived | 67 | N |
| `ENCRYPTED_PACKET_MAX_CONTENT` | 295 | 78 | N |
| `LINK_PACKET_MDU` | `RNS.Link.MDU` | 83 | N |
| `LINK_PACKET_MAX_CONTENT` | 319 | 89 | N |
| `PLAIN_PACKET_MDU` | `RNS.Packet.PLAIN_MDU` | 93 | N |
| `PLAIN_PACKET_MAX_CONTENT` | 368 | 94 | N |
| `ENCRYPTION_DESCRIPTION_AES/EC/UNENCRYPTED` | strings | 97-99 | I |
| `URI_SCHEMA` | "lxm" | 102 | N |
| `QR_ERROR_CORRECTION` | "ERROR_CORRECT_L" | 103 | I |
| `QR_MAX_STORAGE` | 2953 | 104 | N |
| `PAPER_MDU` | 2210 | 105 | N |

### Methods (wire-relevant marked N)

| Method | Line | Class |
|--------|------|-------|
| `__init__` | 113 | N (field defaults) |
| `set_title_from_string/bytes`, `title_as_string` | 190-196 | N |
| `set_content_from_string/bytes`, `content_as_string` | 199-205 | N |
| `set_fields`, `get_fields` | 212-218 | N |
| `validate_stamp` | 270 | N |
| `get_stamp` | 293 | N |
| `get_propagation_stamp` | 326 | N |
| `pack` | 352 | N |
| `send` | 460 | I |
| `determine_compression_support` | 507 | N |
| `determine_transport_encryption` | 517 | I |
| `__mark_delivered/propagated/paper_generated` | 558-582 | I |
| `__resource_concluded`, `__propagation_resource_concluded` | 594-605 | I |
| `__link_packet_timed_out`, `__update_transfer_progress` | 613-620 | I |
| `__as_packet` | 623 | N |
| `__as_resource` | 637 | N |
| `packed_container` | 657 | N |
| `write_to_directory` | 672 | I |
| `as_uri` | 687 | N |
| `as_qr` | 707 | I |
| `unpack_from_bytes` (static) | 735 | N |
| `unpack_from_file` (static) | 810 | I |

### msgpack sites
364 (pack payload, N), 378 (pack payload, N), 433 (propagation envelope, N),
669 (packed_container, N), 741 (unpack payload, N), 747 (re-pack for hash, N),
812 (unpack from file, I).

### RNS primitives
`Identity.full_hash` 365/431, `Identity.truncated_hash` 274, `source.sign` 375,
`identity.validate` 794, `Destination.encrypt` 427/446, `Destination.SINGLE/PLAIN/LINK/GROUP`
395-548, `Packet` 476, `Link.ACTIVE` 647, `Resource` 651/653, `Identity.recall` 759/765,
size constants `Identity.TRUNCATED_HASHLENGTH/SIGLENGTH` 39/40, `Packet.ENCRYPTED_MDU/PLAIN_MDU`
67/93, `Link.MDU` 83.

### Filesystem / time
`open/write` 677-679 (write_to_directory, I). `time.time()` 354, 433 (N: payload
and envelope timestamps).

---

## LXMF.py (217 lines) — module

### Constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| `APP_NAME` | "lxmf" | 1 | N |
| `FIELD_EMBEDDED_LXMS..FIELD_RENDERER` | 0x01..0x0F | 8-22 | N |
| `FIELD_CUSTOM_TYPE/DATA/META` | 0xFB-0xFD | 34-36 | N |
| `FIELD_NON_SPECIFIC/DEBUG` | 0xFE-0xFF | 40-41 | N |
| `AM_CODEC2_*` | 0x01..0x09 | 55-63 | N |
| `AM_OPUS_*` | 0x10..0x19 | 66-75 | N |
| `AM_CUSTOM` | 0xFF | 79 | N |
| `RENDERER_PLAIN/MICRON/MARKDOWN/BBCODE` | 0x00-0x03 | 89-92 | N |
| `PN_META_VERSION..PN_META_CUSTOM` | 0x00..0xFF | 98-104 | N |
| `SF_COMPRESSION` | 0x00 | 108 | N |

### Functions

| Function | Line | Class |
|----------|------|-------|
| `display_name_from_app_data` | 117 | N |
| `stamp_cost_from_app_data` | 141 | N |
| `compression_support_from_app_data` | 154 | N |
| `pn_name_from_app_data` | 169 | N |
| `pn_stamp_cost_from_app_data` | 182 | N |
| `pn_announce_data_is_valid` | 191 | N |

### msgpack sites
123, 146, 159, 173, 186, 194 (all unpack of announce app_data, N).

---

## LXStamper.py (396 lines) — module

### Constants

| Symbol | Value | Line | Class |
|--------|-------|------|-------|
| `WORKBLOCK_EXPAND_ROUNDS` | 3000 | 10 | N |
| `WORKBLOCK_EXPAND_ROUNDS_PN` | 1000 | 11 | N |
| `WORKBLOCK_EXPAND_ROUNDS_PEERING` | 25 | 12 | N |
| `STAMP_SIZE` | 32 | 13 | N |
| `PN_VALIDATION_POOL_MIN_SIZE` | 256 | 14 | I |

### Functions

| Function | Line | Class |
|----------|------|-------|
| `stamp_workblock` | 18 | N |
| `stamp_value` | 31 | N |
| `stamp_valid` | 42 | N |
| `validate_peering_key` | 48 | N |
| `validate_pn_stamp` | 53 | N |
| `validate_pn_stamps_job_simple/multip`, `validate_pn_stamps` | 67-87 | I (parallelism) |
| `generate_stamp` | 92 | N (algorithm) |
| `cancel_work` | 113 | I |
| `job_simple/linux/android` | 145-260 | I (platform PoW workers) |

### msgpack sites
24 (salt `packb(n)`, N). RNS: `Cryptography.hkdf` 22, `Identity.full_hash` 24/34/44.

---

## Handlers.py (92 lines)

| Class / method | Line | Class |
|----------------|------|-------|
| `LXMFDeliveryAnnounceHandler.received_announce` | 9/15 | N (announce parsing) |
| `LXMFPropagationAnnounceHandler.received_announce` | 35/41 | N + I (auto-peer is I) |

msgpack: 46 (unpack announce, N). RNS: `Transport.hops_to` 71/72 (I).

---

## LXMRouter.py (2733 lines) — class `LXMRouter` (line 29)

Mostly **I** (router internals: jobloop, queues, persistence, peer rotation,
retry cadences). The **N** surfaces are the announce/app-data builders and the
propagation request handlers and packers.

### Normative surfaces

| Symbol | Line | Class |
|--------|------|-------|
| `get_propagation_node_announce_metadata` | 302 | N |
| `get_propagation_node_app_data` | 307 | N |
| `get_announce_app_data` | 986 | N |
| `generate_ticket` | 1025 | N |
| `message_get_request` | 1427 | N (`/get` request shape) |
| `message_list_response` | 1507 | N |
| `message_get_response` | 1552 | N |
| `offer_request` | 2142 | N (`/offer` handler) |
| `propagation_packet` | 2110 | N |
| `propagation_resource_concluded` | 2194 | N |
| `lxmf_propagation` | 2310 | N (transient ingest) |
| `ingest_lxm_uri` | 2370 | N (paper ingest) |
| `lxmf_delivery` | 1732 | N (inbound delivery dispatch) |

### Informative constants (selected; full set in [Router internals](11-router-informative.md))
`MAX_DELIVERY_ATTEMPTS=5`(30), `PROCESSING_INTERVAL=4`(31), `DELIVERY_RETRY_WAIT=10`(32),
`PATH_REQUEST_WAIT=7`(33), `MESSAGE_EXPIRY=30d`(38), `STAMP_COST_EXPIRY=45d`(39),
`MAX_PEERS=20`(43), `AUTOPEER_MAXDEPTH=4`(45), `PEERING_COST=18`(50), `PROPAGATION_COST=16`(54),
`PROPAGATION_LIMIT=256`(55), `SYNC_LIMIT=256*40`(56), `DELIVERY_LIMIT=1000`(57),
`PN_STAMP_THROTTLE=180`(60), PR_* states 62-77 (N: appear in `/get` FSM signalling),
request paths `STATS_GET/SYNC_REQUEST/UNPEER_REQUEST` 81-83 (I), JOB_* intervals 853-860 (I).

### Persistence / time
Extensive filesystem use (message store, peers, tickets, costs, stats) — all **I**.
80+ `time.time()` calls — **I** except where a value is signed or hashed.

---

## LXMPeer.py (642 lines) — class `LXMPeer` (line 13)

| Symbol | Line | Class |
|--------|------|-------|
| `OFFER_REQUEST_PATH="/offer"`, `MESSAGE_GET_PATH="/get"` | 14-15 | N |
| state `IDLE..RESOURCE_TRANSFERRING` | 17-22 | I |
| `ERROR_NO_IDENTITY..ERROR_TIMEOUT` | 24-31 | N (`/offer` response codes) |
| `STRATEGY_LAZY/PERSISTENT`, `DEFAULT_SYNC_STRATEGY` | 33-35 | I |
| `MAX_UNREACHABLE=14d`, `SYNC_BACKOFF_STEP=12m`, `PATH_REQUEST_GRACE=7.5` | 39-50 | I |
| `from_bytes`/`to_bytes` (peer persistence) | 52/138 | I |
| `generate_peering_key` | 242 | N |
| `sync` | 267 | I + N (offer payload shape) |
| `offer_response` | 396 | N |
| `resource_concluded` | 488 | N (sync resource payload) |

msgpack: 54/172 (peer persistence, I), 462 (sync resource `packb([time, lxm_list])`, N).

---

## _version.py (2 lines)
`__version__ = "0.9.6"` (1) — **N** (reference pin).

## Utilities/lxmd.py (1127 lines)
Daemon and CLI. **X** out of scope (not protocol). Listed for completeness only.

---

## Deferred RNS primitives (cited into `reference/Reticulum`, not re-specified)

`RNS.Identity.full_hash` (SHA-256), `truncated_hash`, Ed25519 `sign`/`validate`,
`RNS.Destination.encrypt`/`decrypt` (ECDH + AES token + ratchets),
`RNS.Cryptography.hkdf`, and the size constants `RNS.Identity.TRUNCATED_HASHLENGTH`
(128), `SIGLENGTH` (512), `HASHLENGTH` (256), `RNS.Packet.ENCRYPTED_MDU`/`PLAIN_MDU`,
`RNS.Link.MDU`. Their exact behaviour-as-used is pinned by the test vectors.
