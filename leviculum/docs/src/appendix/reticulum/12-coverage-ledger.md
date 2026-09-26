# Coverage ledger

Traceability matrix from the frozen [Symbol inventory](_inventory.md) to the
specification. Every normative (N) symbol maps to a section and a proof; every
informative (I) and out-of-scope (X) symbol carries a reason. Proof: `vector`
(a `[VEC-...]`), `computed` (derivation shown), `quoted` (value cited verbatim),
`n/a` (informative/out-of-scope).

## Reticulum.py / Cryptography

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| MTU, MDU, HEADER sizes, TRUNCATED_HASHLENGTH | Reticulum.py:93-152 | N | 04 | computed (vector constants) |
| IFAC_MIN_SIZE, IFAC_SALT | Reticulum.py:149-150 | N | 10 | quoted |
| full/truncated hash | Identity.py:373-390 | N | 01 | vector VEC-HASH |
| HKDF, HMAC, AES, PKCS7 | Cryptography/* | N | 01 | vector VEC-HKDF/HMAC/AES |
| Token (modified Fernet), TOKEN_OVERHEAD | Token.py | N | 01,02 | vector VEC-ID-TOKEN |
| X25519, Ed25519 | Cryptography/* | N | 01,02 | vector VEC-ID-SIGN/LINK |

## Identity.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| KEYSIZE, HASHLENGTH, SIGLENGTH, NAME_HASH_LENGTH, DERIVED_KEY_LENGTH, RATCHETSIZE | 59-90 | N | 02 | quoted/computed |
| key material, identity hash, get_public_key | 750-810 | N | 02 | vector VEC-ID-HASH |
| sign / validate | 931-964 | N | 02 | vector VEC-ID-SIGN |
| encrypt / decrypt (token) | 827-928 | N | 02 | vector VEC-ID-TOKEN |
| validate_announce | 532-634 | N | 05 | vector VEC-ANN-* |
| ratchet id / generation | 417-425 | N | 02 | quoted |
| RATCHET_EXPIRY, recall/remember | 69,— | I | 02 | n/a (rotation/resolution) |

## Destination.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| SINGLE/GROUP/PLAIN/LINK, PROVE_*, IN/OUT | 63-80 | N | 03,04 | quoted |
| name hash / destination hash | 116-141 | N | 03 | vector VEC-DEST-HASH |
| announce (data + signed data) | 243-317 | N | 05 | vector VEC-ANN-* |
| encrypt / decrypt | 585-611 | N | 03 | quoted (VEC-ID-TOKEN) |
| RATCHET_COUNT/INTERVAL, PR_TAG_WINDOW | 83-90 | I | 02 | n/a |

## Packet.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| packet types, header types, context bytes, FLAG_* | 60-96 | N | 04 | quoted |
| ENCRYPTED_MDU / PLAIN_MDU | 106-110 | N | 04 | computed |
| pack / unpack | 177-272 | N | 04 | vector VEC-PKT-PLAIN/ENC/HEADER2 |
| get_hashable_part, validate_proof, EXPL/IMPL_LENGTH | 355,498 | N | 04 | quoted |
| PacketReceipt states | 408-415 | I | 04 | n/a (local) |

## Link.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| ECPUBSIZE, KEYSIZE, LINK_MTU_SIZE, masks, MODE_AES256_CBC | — | N | 06 | quoted |
| link_id_from_lr_packet, set_link_id | 340-351 | N | 06 | vector VEC-LINK |
| handshake (session key), get_salt/get_context | 353-366,643 | N | 06 | vector VEC-LINK |
| prove, signalling_bytes | 371-377,148 | N | 06 | vector VEC-LINK |
| identify, send_keepalive, context payloads | 459,848 | N | 06 | quoted |
| states, KEEPALIVE/STALE timing, watchdog | — | I | 06 | n/a |

## Resource.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| ResourceAdvertisement.pack, flags, keys | 1278-1355 | N | 07 | vector VEC-RES-ADV |
| MAPHASH_LEN, RANDOM_HASH_SIZE, HASHMAP_* | — | N | 07 | quoted |
| prove / validate_proof | 752-786 | N | 07 | vector VEC-RES-PROOF |
| context bytes RESOURCE..RESOURCE_RCL | Packet.py:73-79 | N | 04,07 | quoted |
| WINDOW*, timeout factors, advertise/assemble | — | I | 07 | n/a (flow control) |
| status enum | — | I | 07 | n/a |

## Channel.py / Buffer.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| Envelope.pack/unpack | Channel.py:174-200 | N | 08 | vector VEC-CHAN-ENVELOPE |
| StreamDataMessage header, SMT_STREAM_DATA, STREAM_ID_MAX, OVERHEAD | Buffer.py:80-92 | N | 08 | vector VEC-STREAM-HDR |
| MessageState, CEType, sequencing | — | I | 08 | n/a |

## Transport.py

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| transmit / inbound (IFAC) | 1051-1434 | N | 10 | vector VEC-IFAC |
| request_path (path request) | 2771-2787 | N | 09 | vector VEC-PATH-REQUEST |
| path response (PATH_RESPONSE rebroadcast) | 2943-2972 | N | 09 | quoted |
| PATHFINDER_*, path TTLs, pacing | 50-83 | I | 09 | n/a (routing) |
| path/announce/link/reverse/tunnel tables, IDX_* | 3547-3586 | I | 09 | n/a (internal) |
| dedup, jobs, table culling | 508,— | I | 09 | n/a |

## Interfaces

| Symbol(s) | file:line | Class | Section | Proof |
|-----------|-----------|-------|---------|-------|
| HDLC FLAG/ESC/ESC_MASK, escape | TCPInterface.py:44-52,323 | N | 10 | vector VEC-HDLC |
| KISS FEND/FESC framing | KISSInterface.py | N | 10 | quoted |
| interface drivers (TCP/LoRa/serial specifics) | Interfaces/* | X | — | n/a (medium drivers) |

## Result

Every normative symbol maps to a section and a proof; no normative row has an
empty section or `n/a` proof. Informative and out-of-scope rows are reasoned.
Coverage is complete against the frozen inventory at commit `d5e62d4`. Re-auditing
after a reference bump: re-enumerate the source and diff the inventory; a new
symbol appears here unclassified.
