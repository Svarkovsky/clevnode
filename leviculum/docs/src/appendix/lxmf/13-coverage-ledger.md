# Coverage ledger

This is the traceability matrix from the frozen [Symbol inventory](_inventory.md)
to the specification. Every **normative (N)** symbol maps to a section and a
proof. Every **informative (I)** and **out-of-scope (X)** symbol carries a reason.
A normative symbol with no section and proof is a coverage gap.

This ledger preserves the original LXMF 0.9.6 source-line audit. Active Rust
compatibility is locked to the 1.1.0 canonical fixture. Entries for `/offer`,
propagation-node ingest, and `LXMPeer` describe the Python protocol only;
`leviculum-lxmf` implements origin uploads and `/get` mailbox collection, not
the server or peer state machine. The `/get` implementation is interoperable
within one request/response Resource segment; Python's larger split form is a
documented implementation gap rather than claimed coverage.
Post-audit implementation and API differences introduced through LXMF 1.1.0
are tracked separately in the [parity status](15-parity-status.md).

Proof column: `vector` (a `[VEC-...]`), `computed` (derivation shown), `quoted`
(byte/enum value cited verbatim), `n/a` (informative/out-of-scope).

## LXMessage.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| representation `UNKNOWN/PACKET/RESOURCE` | 24-26 | N | 05 | quoted |
| method `OPPORTUNISTIC/DIRECT/PROPAGATED/PAPER` | 29-32 | N | 05 | quoted |
| unverified `SOURCE_UNKNOWN/SIGNATURE_INVALID` | 35-36 | N | 03 | quoted |
| size constants `DESTINATION_LENGTH … PLAIN_PACKET_MAX_CONTENT` | 39-94 | N | 02 | computed (vector `constants`) |
| ticket constants `TICKET_EXPIRY/GRACE/RENEW/INTERVAL`, `COST_TICKET` | 48-52 | N | 08 | quoted (vector `constants`) |
| `URI_SCHEMA`, `QR_MAX_STORAGE`, `PAPER_MDU` | 102-105 | N | 02, 05 | computed |
| `ENCRYPTION_DESCRIPTION_*` | 97-99 | I | — | n/a (local labels) |
| `QR_ERROR_CORRECTION` | 103 | I | — | n/a (QR rendering) |
| state constants | 14-21 | I | 06 | n/a (local lifecycle) |
| `set_title/content_*`, `*_as_string` | 190-205 | N | 03 | vector VEC-MSG-1/3 |
| `set_fields/get_fields` | 212-218 | N | 03, 04 | vectors VEC-MSG-2/VEC-MSG-NEGATIVE-FIELD |
| `validate_stamp` | 270 | N | 07 | vector VEC-STAMP-1 |
| `get_stamp` | 293 | N | 07 | vector VEC-MSG-TICKET (ticket branch) |
| `get_propagation_stamp` | 326 | N | 07, 10 | vector VEC-STAMP-PN |
| `pack` | 352 | N | 03, 05 | vector VEC-MSG-1/2 |
| `__as_packet` | 623 | N | 05, 06 | vector VEC-DLV-OPP/DIRECT |
| `__as_resource` | 637 | N | 05, 06 | quoted |
| `packed_container`, `write_to_directory`, `unpack_from_file` | 657-810 | I/N | 11 | n/a (local storage) |
| `as_uri` | 687 | N | 05, 10 | vector VEC-PAPER-URI |
| `as_qr` | 707 | I | — | n/a (QR rendering) |
| `unpack_from_bytes` | 735 | N | 03 | vector VEC-MSG-3 |
| `send`, `__mark_*`, `__resource_concluded`, timers | 460-620 | I | 06, 11 | n/a (orchestration) |
| msgpack sites 364/378/433/669/741/747 | — | N | 03, 10 | vector |
| `time.time()` 354/433 | — | N | 03, 10 | vector (timestamp fields) |

## LXMF.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| `APP_NAME` | 1 | N | 00 | quoted |
| `FIELD_*` (all) | 8-41 | N | 04 | quoted (VEC-MSG-2 for one) |
| `AM_*` audio modes | 55-79 | N | 04 | quoted |
| `RENDERER_*` | 89-92 | N | 04 | quoted |
| `PN_META_*` | 98-104 | N | 09 | quoted (VEC-ANN-PROPAGATION) |
| `SF_COMPRESSION` | 108 | N | 12 | quoted |
| `display_name_from_app_data`, `stamp_cost_from_app_data` | 117-152 | N | 09 | vector VEC-ANN-DELIVERY |
| `compression_support_from_app_data` | 154 | N | 09 | quoted |
| `pn_name_from_app_data`, `pn_stamp_cost_from_app_data`, `pn_announce_data_is_valid` | 169-217 | N | 09 | vector VEC-ANN-PROPAGATION |

## LXStamper.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| `WORKBLOCK_EXPAND_ROUNDS*`, `STAMP_SIZE` | 10-13 | N | 07 | vectors VEC-STAMP-1/VEC-STAMP-PN |
| `stamp_workblock` | 18 | N | 07 | vector VEC-STAMP-1 |
| `stamp_value` | 31 | N | 07 | vector VEC-STAMP-1 |
| `stamp_valid` | 42 | N | 07 | vector VEC-STAMP-1 |
| `validate_peering_key` | 48 | N | 10 | quoted |
| `validate_pn_stamp` | 53 | N | 10 | quoted |
| `generate_stamp` | 92 | N | 07 | quoted |
| `validate_pn_stamps*`, `job_*`, `cancel_work`, `PN_VALIDATION_POOL_MIN_SIZE` | 67-354 | I | 07 | n/a (PoW parallelism) |

## Handlers.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| `LXMFDeliveryAnnounceHandler.received_announce` | 9-32 | N | 09 | vector VEC-ANN-DELIVERY |
| `LXMFPropagationAnnounceHandler.received_announce` | 35-72 | N+I | 09, 11 | vector / n/a (auto-peer) |

## LXMRouter.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| `get_propagation_node_announce_metadata`, `get_propagation_node_app_data` | 302-319 | N | 09 | vector VEC-ANN-PROPAGATION |
| `get_announce_app_data` | 986 | N | 09 | vector VEC-ANN-DELIVERY |
| `generate_ticket`, `remember_ticket`, `get_outbound_ticket*` | 1025-1086 | N | 08 | vector VEC-MSG-TICKET (FIELD_TICKET value) |
| `message_get_request/list_response/get_response` | 1427-1591 | N | 10 | quoted |
| `offer_request`, `propagation_packet`, `propagation_resource_concluded`, `lxmf_propagation`, `ingest_lxm_uri` | 2110-2392 | N | 10 | quoted |
| `lxmf_delivery` | 1732 | N | 06 | quoted |
| `PR_*` states | 62-77 | N | 10 | quoted |
| request paths `STATS/SYNC/UNPEER` | 81-83 | I | 11 | n/a |
| delivery/expiry/peer/job constants | 30-83, 853-860 | I | 11 | n/a (scheduling) |
| persistence, queues, jobloop, rotation, `time.time()` | various | I | 11 | n/a (internal) |

## LXMPeer.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| `OFFER_REQUEST_PATH`, `MESSAGE_GET_PATH` | 14-15 | N | 10 | quoted |
| `ERROR_*` | 24-31 | N | 10 | quoted |
| `generate_peering_key` | 242 | N | 10 | quoted |
| `sync` (offer payload), `offer_response`, `resource_concluded` | 267-520 | N | 10 | quoted |
| state constants, strategy, timing, `from_bytes/to_bytes`, peer counts | 17-50, 52-175, 544-640 | I | 11 | n/a (internal/persistence) |
| msgpack 462 (sync resource) | — | N | 10 | quoted |

## _version.py / Utilities/lxmd.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| `__version__` | _version.py:1 | N | 00 | quoted (pin) |
| daemon/CLI | lxmd.py | X | — | n/a (not protocol) |

## Result

Every normative symbol in the inventory maps to a section and a proof above; no
normative row is left with an empty section or `n/a` proof. Informative and
out-of-scope rows are reasoned. Coverage is therefore complete against the frozen
inventory at the pinned commit. To re-audit after a reference bump, re-enumerate
the source and diff the inventory; any new symbol appears here unclassified.
